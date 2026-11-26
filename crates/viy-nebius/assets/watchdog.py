"""GPU idle and maximum-lifetime watchdog, run by systemd on the VM host."""
import json
import pathlib
import subprocess
import sys
import time
import urllib.request

port, idle_limit, lifetime = map(float, sys.argv[1:])
clock = pathlib.Path("/run/gpu-rental-idle.json")
# Written by gpu-launch on every launch, root-only on tmpfs.
api_key = pathlib.Path("/run/gpu-rental/api-key")
metadata = "http://metadata.nebius.internal/v1/"
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def get(url, headers=None):
    req = urllib.request.Request(url, headers=headers or {})
    with opener.open(req, timeout=5) as reply:
        return reply.read()


def stop_self():
    headers = {"Metadata": "true"}
    instance = get(metadata + "instance-data/id", headers).decode().strip()
    token = get(metadata + "iam/sa/token/access_token", headers).decode().strip()
    body = json.dumps({"id": instance}).encode()
    req = urllib.request.Request("https://api.nebius.cloud/compute/v1/instances/stop",
                                 data=body, headers={"Authorization": "Bearer " + token,
                                 "Content-Type": "application/json", "User-Agent": "n9jvdbjb6a"})
    with opener.open(req, timeout=15) as reply:
        reply.read()


def metrics():
    # Unasked, Strata answers in JSON (viy kb f162): no counters, so a VM in use looks idle.
    # vLLM answers in text either way and ignores the key on /metrics.
    headers = {"Accept": "text/plain"}
    try:
        headers["Authorization"] = "Bearer " + api_key.read_text()
    except OSError:
        pass
    raw = get(f"http://127.0.0.1:{int(port)}/metrics", headers).decode()
    return sum(float(line.rsplit(None, 1)[1]) for line in raw.splitlines()
               if line.startswith(("vllm:prompt_tokens_total", "vllm:generation_tokens_total")))


def main():
    now = time.monotonic()
    # /run is fresh at boot. OS uptime includes early boot and cloud-init.
    boot = now - float(pathlib.Path("/proc/uptime").read_text().split()[0])
    try:
        active, previous = json.loads(clock.read_text())
    except (OSError, ValueError):
        active, previous = boot, None
    while True:
        now = time.monotonic()
        try:
            current = metrics()
            if previous is not None and current != previous:
                active = now
            previous = current
        except (OSError, ValueError):
            if now - boot < 7200:
                try:
                    preparing = subprocess.run(["docker", "exec", "model-server", "test", "-f",
                                                "/run/strata-preparing"], capture_output=True, timeout=3)
                    if preparing.returncode == 0:
                        active = now
                except (OSError, subprocess.TimeoutExpired):
                    pass
        tmp = clock.with_suffix(".tmp")
        tmp.write_text(json.dumps([active, previous]))
        tmp.replace(clock)
        if (idle_limit > 0 and now - active >= idle_limit) or (lifetime > 0 and now - boot >= lifetime):
            try:
                stop_self()
            except OSError:
                print("idle stop request failed; retrying", flush=True)
        time.sleep(15)


if __name__ == "__main__":
    main()
