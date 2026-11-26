"""Exercise the deployed watchdog code against synthetic metrics and metadata."""
import http.server
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import MagicMock, call, patch

assets = pathlib.Path(__file__).resolve().parents[1] / "assets"
spec = importlib.util.spec_from_file_location("watchdog", assets / "watchdog.py")
watchdog = importlib.util.module_from_spec(spec)
with patch.object(sys, "argv", ["watchdog", "8000", "60", "120"]):
    spec.loader.exec_module(watchdog)


class StrataMetrics(http.server.BaseHTTPRequestHandler):
    """Strata 0.2.16's /metrics (viy kb f162): JSON unless Accept: text/plain."""
    seen = []

    def do_GET(self):
        type(self).seen.append((self.path, self.headers.get("Authorization")))
        if self.headers.get("Accept") == "text/plain":
            body = (b'# TYPE vllm:prompt_tokens_total counter\n'
                    b'vllm:prompt_tokens_total{model_name="model"} 40\n'
                    b'vllm:generation_tokens_total{model_name="model"} 65\n')
        else:
            body = b'{"engine": {"model": "model", "max_context": 262144}}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class Watchdog(unittest.TestCase):
    def test_reads_strata_counters_with_the_launch_key(self):
        server = http.server.HTTPServer(("127.0.0.1", 0), StrataMetrics)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory() as home:
                key = pathlib.Path(home) / "api-key"
                with patch.object(watchdog, "port", server.server_port), patch.object(watchdog, "api_key", key):
                    self.assertEqual(watchdog.metrics(), 105)
                    key.write_text("mock-key")
                    self.assertEqual(watchdog.metrics(), 105)
        finally:
            server.shutdown()
            server.server_close()
        self.assertEqual(StrataMetrics.seen, [("/metrics", None), ("/metrics", "Bearer mock-key")])

    def test_stop_targets_only_the_instance_returned_by_metadata(self):
        with patch.object(watchdog, "get", side_effect=[b"instance-test", b"mock-bearer"]) as get, patch.object(watchdog.opener, "open") as send:
            watchdog.stop_self()
        for call in get.call_args_list:
            self.assertEqual(call.args[1], {"Metadata": "true"})
        request = send.call_args.args[0]
        self.assertEqual(request.full_url, "https://api.nebius.cloud/compute/v1/instances/stop")
        self.assertEqual(json.loads(request.data), {"id": "instance-test"})
        self.assertEqual(request.get_header("Authorization"), "Bearer mock-bearer")

    def test_counts_prompt_and_generation_for_a_running_request(self):
        metrics = b'vllm:prompt_tokens_total{model="fixture"} 4\nvllm:generation_tokens_total 3\nother 99\n'
        with patch.object(watchdog, "get", return_value=metrics):
            self.assertEqual(watchdog.metrics(), 7)

    def test_active_generation_prevents_idle_stop_but_the_lifetime_cap_wins(self):
        with tempfile.TemporaryDirectory() as home:
            clock = pathlib.Path(home) / "clock"
            clock.write_text(json.dumps([0, 3]))
            # Keep monotonic time in the same frame as the VM's boot-relative clock.
            real_read = pathlib.Path.read_text
            def read(path, *args, **kwargs):
                if str(path) == "/proc/uptime":
                    return "100 10"
                return real_read(path, *args, **kwargs)
            with patch.object(watchdog, "clock", clock), patch.object(pathlib.Path, "read_text", read), patch.object(watchdog, "metrics", side_effect=[4, 5, 6]), patch.object(watchdog.time, "monotonic", side_effect=[100, 100, 115, 130]), patch.object(watchdog.time, "sleep", side_effect=[None, None, StopIteration]), patch.object(watchdog, "stop_self") as stop:
                with self.assertRaises(StopIteration):
                    watchdog.main()
            self.assertEqual(json.loads(clock.read_text()), [130, 6])
            stop.assert_called_once()

    def launch(self, installed_watchdog):
        script = (assets / "launch.py").read_text()
        calls = []
        root = MagicMock()
        root.read_text.return_value = installed_watchdog
        payload = {"env": {"VLLM_API_KEY": "mock-key"}, "command": "true", "image": "fixture", "port": 8000}
        def run(argv, **kwargs):
            calls.append(argv)
            return MagicMock(returncode=1 if "-C" in argv else 0, stdout=b"false")
        import io
        with patch("sys.stdin", io.StringIO(json.dumps(payload))), patch("pathlib.Path", return_value=root), patch("os.umask"), patch("subprocess.run", side_effect=run), patch("subprocess.check_output", return_value="169.254.169.254 STREAM metadata\n"):
            exec(compile(script, "launch.py", "exec"), {"WATCHDOG": "current watchdog"})
        return calls, root

    def test_launch_installs_this_clients_watchdog_before_checking_it(self):
        calls, root = self.launch("older watchdog")
        restart = calls.index(["systemctl", "restart", "gpu-idle.service"])
        self.assertLess(restart, calls.index(["systemctl", "is-active", "--quiet", "gpu-idle.service"]))
        self.assertIn(call("current watchdog"), root.with_suffix.return_value.write_text.call_args_list)
        self.assertIn(call("mock-key"), root.__truediv__.return_value.with_suffix.return_value.write_text.call_args_list)
        calls, _ = self.launch("current watchdog")
        self.assertNotIn(["systemctl", "restart", "gpu-idle.service"], calls)

    def test_container_launch_requires_the_watchdog_mount_and_metadata_firewall(self):
        calls, _ = self.launch("current watchdog")
        docker = calls[-1]
        self.assertEqual(docker[:2], ["docker", "run"])
        self.assertIn("127.0.0.1:8000:8000", docker)
        self.assertIn(["/usr/local/sbin/gpu-workspace"], calls[:-1])
        self.assertIn(["systemctl", "is-active", "--quiet", "gpu-idle.service"], calls[:-1])
        self.assertIn(["iptables", "-I", "DOCKER-USER", "-d", "169.254.169.254", "-j", "REJECT"], calls[:-1])
        block = calls.index(["iptables", "-I", "DOCKER-USER", "-d", "169.254.0.0/16", "-j", "REJECT"])
        for protocol in ("udp", "tcp"):
            allow_dns = ["iptables", "-I", "DOCKER-USER", "-i", "docker0", "-d",
                         "169.254.169.2", "-p", protocol, "--dport", "53", "-j", "RETURN"]
            self.assertGreater(calls[:-1].index(allow_dns), block)


class WorkspaceRepair(unittest.TestCase):
    def test_repairs_only_the_deployed_probe_and_preserves_the_launch_input(self):
        with tempfile.TemporaryDirectory() as directory:
            helper = pathlib.Path(directory) / "gpu-workspace"
            script = (assets / "retry-launch.sh").read_text()
            # Use unprivileged fixture paths and echo the launcher's stdin.
            script = script.replace("/usr/local/sbin/gpu-workspace", str(helper))
            script = script.replace("sudo -n ", "") + "\nexec cat"
            payload = b'{"command":"fixture"}'
            old = '    signatures=$(wipefs --noheadings -o TYPE "$dev")'
            source = f"set -eu\ntest ! -e marker\n{old}\ntest -z \"$signatures\"\n"
            helper.write_text(source)
            helper.chmod(0o700)
            def retry():
                return subprocess.run(["sh", "-c", script], input=payload, capture_output=True)
            result = retry()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, payload)
            fixed = source.replace("--noheadings -o", "--no-act --noheadings --output")
            self.assertEqual(helper.read_text(), fixed)
            self.assertEqual(helper.stat().st_mode & 0o777, 0o700)
            for other in [fixed, source.replace('"$dev"', '"$other"')]:
                helper.write_text(other)
                result = retry()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, b"")
                self.assertEqual(helper.read_text(), other)


if __name__ == "__main__":
    unittest.main()
