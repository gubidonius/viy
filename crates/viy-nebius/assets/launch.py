"""Runs over authenticated SSH. Its input and the engine script live on tmpfs."""
import json
import os
import pathlib
import shlex
import subprocess
import sys

os.umask(0o077)
spec = json.load(sys.stdin)
root = pathlib.Path("/run/gpu-rental")
root.mkdir(mode=0o700, exist_ok=True)
env = spec["env"]
# The host watchdog's credential for the engine's /metrics.
key = root / "api-key"
staged = key.with_suffix(".tmp")
staged.write_text(env.get("VLLM_API_KEY", ""))
staged.replace(key)
# cloud-init installed the watchdog of the client that created this VM. A resumed VM runs
# this client's (WATCHDOG, prepended by the client); its clock file survives the restart.
installed = pathlib.Path("/usr/local/sbin/gpu-idle.py")
try:
    current = installed.read_text()
except OSError:
    current = None
if current != WATCHDOG:
    staged = installed.with_suffix(".new")
    staged.write_text(WATCHDOG)
    staged.chmod(0o700)
    staged.replace(installed)
    subprocess.run(["systemctl", "restart", "gpu-idle.service"], check=True)
# systemd handles reboots and restarts. Concurrent launches are serialized by the client.
subprocess.run(["systemctl", "is-active", "--quiet", "gpu-idle.service"], check=True)
subprocess.run(["/usr/local/sbin/gpu-workspace"], check=True)
if subprocess.run(["docker", "inspect", "--format", "{{.State.Running}}", "model-server"],
                  capture_output=True).stdout.strip() == b"true":
    sys.exit(0)
subprocess.run(["docker", "rm", "-f", "model-server"], capture_output=True)
script = "set -e\n" + "".join(f"export {name}={shlex.quote(value)}\n" for name, value in env.items())
script += spec["command"]
(root / "engine.sh").write_text(script)
port = str(int(spec["port"]))
# Private bridge, published to host loopback only. Block metadata before starting any container.
subprocess.run(["systemctl", "start", "docker"], check=True)
addresses = {"169.254.0.0/16"}
ips = subprocess.check_output(["getent", "ahostsv4", "metadata.nebius.internal"], text=True)
addresses.update(line.split()[0] for line in ips.splitlines())
for addr in addresses:
    rule = ["DOCKER-USER", "-d", addr, "-j", "REJECT"]
    if subprocess.run(["iptables", "-C", *rule], capture_output=True).returncode:
        subprocess.run(["iptables", "-I", *rule], check=True)
# Nebius VPC DNS is also link-local. Let only DNS on the default bridge
# continue through Docker's normal forwarding rules; all other ports and
# the metadata address remain behind the link-local rejection above.
resolver = "169.254.169.2"
if resolver in addresses:
    sys.exit("DNS resolver overlaps the metadata service")
for protocol in ("udp", "tcp"):
    rule = ["DOCKER-USER", "-i", "docker0", "-d", resolver, "-p", protocol,
            "--dport", "53", "-j", "RETURN"]
    if subprocess.run(["iptables", "-C", *rule], capture_output=True).returncode:
        subprocess.run(["iptables", "-I", *rule], check=True)
# The OS image supplies Docker and NVIDIA Container Toolkit. Docker receives no secret in argv.
command = ["docker", "run", "--detach", "--rm", "--name", "model-server", "--gpus", "all",
           "--shm-size", "8g", "--publish", f"127.0.0.1:{port}:{port}",
           "--volume", "/workspace:/workspace", "--volume", f"{root}/engine.sh:/run/engine.sh:ro",
           "--entrypoint", "/bin/bash", spec["image"], "/run/engine.sh"]
subprocess.run(command, check=True)
