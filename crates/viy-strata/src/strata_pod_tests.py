"""Tests of assets/strata_pod.py, run with python3 under cargo test
(boot_tests.rs::strata_pod_passes_its_tests). The watchdog's are startup_tests.py's IdleClock,
stop, new-container and loop cases (viy-vllm), run on Strata's copy, plus Strata's Prometheus
request.

    python3 -I crates/viy-strata/src/strata_pod_tests.py
"""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ASSET = Path(__file__).resolve().parent.parent / "assets" / "strata_pod.py"
_spec = importlib.util.spec_from_file_location("strata_pod", ASSET)
pod = importlib.util.module_from_spec(_spec)
sys.modules["strata_pod"] = pod
_spec.loader.exec_module(pod)
# No test marks the real container disk; the folder goes when the run ends.
_MARKS = tempfile.TemporaryDirectory(prefix="strata-pod-tests-")
pod.CONTAINER_MARK = Path(_MARKS.name) / "mark"

HOUR = 3600
# Strata 0.2.16's /metrics?format=prometheus after a run (pod n2k01nm2n0ftmx, 2026-08-08, kb f162)
METRICS = """# HELP vllm:prompt_tokens_total Prompt tokens processed.
# TYPE vllm:prompt_tokens_total counter
vllm:prompt_tokens_total{model_name="model"} %s
# HELP vllm:generation_tokens_total Tokens generated.
# TYPE vllm:generation_tokens_total counter
vllm:generation_tokens_total{model_name="model"} %s
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{model_name="model"} 0
"""


class TokenCountTests(unittest.TestCase):
    def test_sums_strata_prompt_and_generated_tokens_only(self):
        self.assertEqual(pod.token_count(METRICS % ("227547", "170")), 227717.0)
        self.assertEqual(pod.token_count("garbage"), 0.0)

    def test_a_json_reply_has_no_tokens(self):
        # plain /metrics is JSON: the reason the watchdog asks for Prometheus
        self.assertEqual(pod.token_count('{"engine": {"model": "model"}}'), 0.0)


class IdleClockTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.path = Path(self._tmp.name) / "watchdog-p1.json"

    def clock(self, now, pod_id="p1"):
        return pod.IdleClock(self.path, pod_id, ttl_s=HOUR, max_s=8 * HOUR, now=now)

    def test_streaming_refreshes_and_silence_expires(self):
        c = self.clock(0)
        self.assertIsNone(c.tick(30, 100))
        self.assertIsNone(c.tick(3000, 150))
        self.assertIsNone(c.tick(3000 + HOUR - 1, 150))
        self.assertIn("idle", c.tick(3000 + HOUR, 150))

    def test_unchanged_counters_and_unreadable_metrics_count_as_idle(self):
        c = self.clock(0)
        c.tick(30, 100)
        c.tick(1800, None)
        self.assertIn("idle", c.tick(HOUR, 100))

    def test_a_boot_that_never_serves_expires(self):
        c = self.clock(0)
        self.assertIn("idle", c.tick(HOUR, None))

    def test_hard_cap_despite_activity(self):
        c = self.clock(0)
        tokens = 0
        for t in range(0, 8 * HOUR, 600):
            tokens += 10
            self.assertIsNone(c.tick(t, tokens))
        self.assertIn("hard cap", c.tick(8 * HOUR, tokens + 10))

    def test_a_restarted_container_keeps_the_countdown(self):
        c = self.clock(0)
        c.tick(30, 100)
        c.tick(600, 200)
        restarted = self.clock(2000)
        self.assertEqual(restarted.last_activity, 600)
        self.assertIn("idle", restarted.tick(600 + HOUR, 0))

    def test_another_pods_clock_is_not_inherited(self):
        self.clock(0).tick(30, 100)
        other = self.clock(5000, pod_id="p2")
        self.assertEqual(other.started, 5000)


class StopTests(unittest.TestCase):
    def test_rest_stop_first(self):
        with patch.object(pod, "runpod_call", return_value=(True, "ok")) as call:
            self.assertEqual(pod.stop_self("p1", "k"), "stopped")
        self.assertEqual(call.call_args_list[0][0][:2], ("POST", pod.REST + "/pods/p1/stop"))

    def test_graphql_stop_when_rest_is_refused(self):
        with patch.object(pod, "runpod_call", side_effect=[(False, "HTTP 403"), (True, "ok")]) as call:
            self.assertEqual(pod.stop_self("p1", "k"), "stopped")
        self.assertIn("podStop", call.call_args_list[1][0][3])

    def test_terminate_when_stop_is_refused(self):
        with patch.object(pod, "runpod_call",
                          side_effect=[(False, "x"), (False, "x"), (True, "ok")]) as call:
            self.assertEqual(pod.stop_self("p1", "k"), "terminated")
        self.assertEqual(call.call_args_list[2][0][:2], ("DELETE", pod.REST + "/pods/p1"))

    def test_raises_when_everything_is_refused(self):
        with patch.object(pod, "runpod_call", return_value=(False, "HTTP 403")):
            with self.assertRaises(RuntimeError) as caught:
                pod.stop_self("p1", "k")
        self.assertEqual(str(caught.exception).count("HTTP 403"), 4)


class NewContainerTests(unittest.TestCase):
    def test_true_once_per_container(self):
        with tempfile.TemporaryDirectory() as d:
            mark = Path(d) / "mark"
            self.assertTrue(pod.new_container(mark))
            self.assertFalse(pod.new_container(mark))   # a restart

    def test_an_unwritable_mark_keeps_the_saved_countdown(self):
        self.assertFalse(pod.new_container(Path("/nonexistent-dir/mark")))

    def test_a_new_container_starts_a_new_countdown(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "watchdog-p1.json"
            c = pod.IdleClock(path, "p1", ttl_s=HOUR, max_s=8 * HOUR, now=0)
            c.tick(30, 100)
            self.assertIn("idle", c.tick(HOUR, 100))
            kept = pod.IdleClock(path, "p1", ttl_s=HOUR, max_s=8 * HOUR, now=86400)
            self.assertIn("idle", kept.tick(86400, 0))
            resumed = pod.IdleClock(path, "p1", ttl_s=HOUR, max_s=8 * HOUR, now=86400, fresh=True)
            self.assertIsNone(resumed.tick(86430, 0))
            self.assertIn("idle", resumed.tick(86400 + HOUR, 0))


class WatchdogLoopTests(unittest.TestCase):
    def env(self, d, **extra):
        return dict({"RUNPOD_POD_ID": "p1", "RUNPOD_API_KEY": "k", "RPCTL_IDLE_TTL_S": "3600",
                     "RPCTL_MAX_POD_S": "28800", "RPCTL_CACHE_DIR": d, "RPCTL_PORT": "8000",
                     "VLLM_API_KEY": "sk-SYNTHETIC"}, **extra)

    def test_shuts_down_after_an_idle_hour_and_retries_failures(self):
        clock = [0.0]
        asked = []

        def sleep(s):
            clock[0] += s

        def urlopen(request, timeout):
            asked.append(request)
            return io.BytesIO((METRICS % ("5", "5")).encode())

        calls = []

        def stop(pod_id, key):
            calls.append(clock[0])
            if len(calls) == 1:
                raise RuntimeError("transient")
            return "stopped"

        with tempfile.TemporaryDirectory() as d, \
                patch.object(pod.urllib.request, "urlopen", urlopen), \
                patch.object(pod, "can_manage", return_value=(True, "ok")), \
                patch.object(pod, "stop_self", stop), \
                patch.dict("os.environ", self.env(d)):
            pod.watchdog(now=lambda: clock[0], sleep=sleep)
        self.assertEqual(len(calls), 2)
        self.assertGreaterEqual(calls[0], HOUR)
        self.assertLess(calls[0], HOUR + 2 * pod.WATCHDOG_POLL_S)
        # Strata's counters are asked for as Prometheus, both ways it takes (kb f162)
        self.assertEqual(asked[0].full_url, "http://127.0.0.1:8000/metrics?format=prometheus")
        self.assertEqual(asked[0].get_header("Accept"), "text/plain")
        self.assertEqual(asked[0].get_header("Authorization"), "Bearer sk-SYNTHETIC")

    def test_a_resumed_pod_is_not_stopped_by_the_clock_it_was_stopped_with(self):
        # Stopped at 3600, started again at 86400 in a new container (no
        # mark): a new hour. A restart of the same container carries on the
        # old, expired clock.
        for marked, stops_at in ((False, 86400 + HOUR), (True, 86400)):
            with self.subTest(restarted_container=marked), \
                    tempfile.TemporaryDirectory() as d, \
                    patch.object(pod, "CONTAINER_MARK", Path(d) / "mark"), \
                    patch.object(pod.urllib.request, "urlopen",
                                 lambda request, timeout: io.BytesIO((METRICS % ("5", "5")).encode())), \
                    patch.object(pod, "can_manage", return_value=(True, "ok")), \
                    patch.dict("os.environ", self.env(d)):
                Path(d, "watchdog-p1.json").write_text(json.dumps(
                    {"pod_id": "p1", "started": 0, "last_activity": 0, "checked": HOUR}))
                if marked:
                    (Path(d) / "mark").touch()
                clock = [86400.0]
                stopped = []
                with patch.object(pod, "stop_self",
                                  lambda pod_id, key: stopped.append(clock[0]) or "stopped"):
                    pod.watchdog(now=lambda: clock[0],
                                 sleep=lambda s: clock.__setitem__(0, clock[0] + s))
                self.assertGreaterEqual(stopped[0], stops_at)
                self.assertLess(stopped[0], stops_at + 2 * pod.WATCHDOG_POLL_S)

    def test_a_streaming_pod_is_never_stopped(self):
        clock = [0.0]
        tokens = [0]

        def sleep(s):
            clock[0] += s
            if clock[0] > 3 * HOUR:
                raise StopIteration

        def urlopen(request, timeout):
            tokens[0] += 7
            return io.BytesIO((METRICS % (tokens[0], tokens[0])).encode())

        with tempfile.TemporaryDirectory() as d, \
                patch.object(pod.urllib.request, "urlopen", urlopen), \
                patch.object(pod, "can_manage", return_value=(True, "ok")), \
                patch.object(pod, "stop_self") as stop, \
                patch.dict("os.environ", self.env(d)):
            with self.assertRaises(StopIteration):
                pod.watchdog(now=lambda: clock[0], sleep=sleep)
        stop.assert_not_called()

    def test_reports_a_key_runpod_refuses(self):
        with tempfile.TemporaryDirectory() as d, \
                patch.object(pod, "can_manage", return_value=(False, "HTTP 403")), \
                patch.object(pod, "phase") as phase, \
                patch.object(pod.urllib.request, "urlopen", side_effect=OSError), \
                patch.object(pod, "stop_self", return_value="stopped"), \
                patch.dict("os.environ", self.env(d, RPCTL_IDLE_TTL_S="60")):
            clock = [0.0]

            def sleep(s):
                clock[0] += s
                if clock[0] > 600:
                    raise StopIteration
            with self.assertRaises(StopIteration):
                pod.watchdog(now=lambda: clock[0], sleep=sleep)
        lines = [c[0][0] for c in phase.call_args_list]
        self.assertTrue(any("cannot stop itself" in line for line in lines))
        self.assertEqual(lines[0], "WATCHDOG CANNOT STOP THIS POD: RunPod refused its key (HTTP 403)")

    def test_off_and_keyless(self):
        with patch.object(pod, "phase") as phase, \
                patch.dict("os.environ", {"RPCTL_IDLE_TTL_S": "0"}, clear=True):
            pod.watchdog()
        self.assertEqual(phase.call_args[0][0], "watchdog off (IDLE_TTL_MIN=0)")
        with patch.object(pod, "phase") as phase, \
                patch.dict("os.environ", {"RPCTL_IDLE_TTL_S": "60"}, clear=True):
            pod.watchdog()
        self.assertTrue(phase.call_args[0][0].startswith("WATCHDOG CANNOT STOP THIS POD: no RunPod key"))


class ReadyTests(unittest.TestCase):
    def test_health_says_loaded(self):
        # Strata 0.2.16's reply once loaded (kb f161)
        self.assertTrue(pod.loaded(b'{"status": "ok", "max_context": 262144, "model": "model", '
                                   b'"loaded": true, "service": "strata"}'))
        self.assertFalse(pod.loaded(b'{"status": "ok", "loaded": false}'))
        self.assertFalse(pod.loaded(b'{"status": "ok"}'))
        self.assertFalse(pod.loaded(b"<html>"))
        self.assertFalse(pod.loaded(b"[]"))

    def test_warm_reads_the_whole_file(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "table.gguf"
            path.write_bytes(b"x" * 1000003)
            read = []
            real_open = open

            def counting_open(p, mode="r", buffering=-1):
                f = real_open(p, mode, buffering)
                original = f.read

                def tracked(n=-1):
                    block = original(n)
                    read.append(len(block))
                    return block
                f.read = tracked
                return f
            with patch("builtins.open", counting_open):
                pod.warm(path, streams=4)
            self.assertEqual(sum(read), 1000003)


def gguf(path, tensors):
    """A GGUF header with these tensor names and no metadata."""
    out = bytearray(b"GGUF") + struct.pack("<IQQ", 3, len(tensors), 0)
    for name in tensors:
        raw = name.encode()
        out += struct.pack("<Q", len(raw)) + raw + struct.pack("<IQIQ", 1, 1, 0, 0)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(out))


class ModelFilesTests(unittest.TestCase):
    def test_trunk_and_the_shard_with_the_ngram_table(self):
        with tempfile.TemporaryDirectory() as d:
            m = Path(d)
            gguf(m / "Q5_K_M" / "M-Q5_K_M-00001-of-00002.gguf", ["token_embd.weight", "blk.0.attn"])
            gguf(m / "Q5_K_M" / "M-Q5_K_M-00002-of-00002.gguf", ["blk.9.ffn", "per_layer_token_embd.weight"])
            gguf(m / "Q5_K_M" / "MTP" / "M-Q5_K_M-MTP-00001-of-00003.gguf", ["per_layer_token_embd.weight"])
            trunk, ple = pod.model_files(m)
            self.assertEqual(trunk.name, "M-Q5_K_M-00001-of-00002.gguf")
            self.assertEqual(ple.name, "M-Q5_K_M-00002-of-00002.gguf")

    def test_one_file_without_a_table(self):
        with tempfile.TemporaryDirectory() as d:
            gguf(Path(d) / "small.gguf", ["token_embd.weight"])
            trunk, ple = pod.model_files(Path(d))
            self.assertEqual(trunk.name, "small.gguf")
            self.assertIsNone(ple)

    def test_two_models_are_refused(self):
        with tempfile.TemporaryDirectory() as d:
            gguf(Path(d) / "A-00001-of-00002.gguf", ["x"])
            gguf(Path(d) / "B-00001-of-00002.gguf", ["x"])
            with self.assertRaises(SystemExit):
                pod.model_files(Path(d))

    def test_not_a_gguf(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "x.gguf").write_bytes(b"nope")
            with self.assertRaises(ValueError):
                pod.gguf_tensor_names(Path(d) / "x.gguf")


class FetchTests(unittest.TestCase):
    def opener(self, data, calls):
        def urlopen(request, timeout):
            calls.append(request.full_url)
            return io.BytesIO(data)
        return urlopen

    def test_a_matching_archive_is_kept_and_not_fetched_again(self):
        data = b"archive bytes"
        sha = hashlib.sha256(data).hexdigest()
        calls = []
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "a.tar.gz"
            pod.fetch("https://example.invalid/a.tar.gz", sha, dest, opener=self.opener(data, calls))
            self.assertEqual(dest.read_bytes(), data)
            pod.fetch("https://example.invalid/a.tar.gz", sha, dest, opener=self.opener(data, calls))
        self.assertEqual(len(calls), 1)

    def test_a_wrong_hash_is_never_kept(self):
        calls = []
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "a.tar.gz"
            with self.assertRaises(SystemExit) as caught:
                pod.fetch("https://example.invalid/a.tar.gz", "0" * 64, dest,
                          opener=self.opener(b"tampered", calls))
            self.assertFalse(dest.exists())
            self.assertFalse(dest.with_suffix(".gz.part").exists())
        self.assertIn("sha256", str(caught.exception))


class UnpackTests(unittest.TestCase):
    def archive(self, d, members):
        path = Path(d) / "a.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            for name, data in members:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = 0o755
                tar.addfile(info, io.BytesIO(data))
        return path

    def test_unpacks_and_finds_the_engine(self):
        with tempfile.TemporaryDirectory() as d:
            a = self.archive(d, [("0.1.62-cuda13/engine-component.json", b"{}"),
                                 ("0.1.62-cuda13/strata", b"\x7fELF")])
            dest = Path(d) / "engine"
            pod.unpack(a, dest)
            self.assertEqual(pod.engine_exe(dest), dest / "0.1.62-cuda13" / "strata")
            self.assertTrue((dest / "0.1.62-cuda13" / "strata").stat().st_mode & 0o100)

    def test_a_path_outside_the_folder_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            a = self.archive(d, [("../escaped", b"x")])
            with self.assertRaises(tarfile.FilterError):
                pod.unpack(a, Path(d) / "engine")
            self.assertFalse((Path(d) / "escaped").exists())


READY = {"version": "0.2.16", "repo": "org/M-GGUF", "files": "Q5_K_M/*",
         "engine": "/v/release/0.2.16/engine/x/strata", "control": "/v/release/0.2.16/control",
         "trunk": "/v/model/Q5_K_M/M-00001-of-00006.gguf", "ple": "/v/model/Q5_K_M/M-00002-of-00006.gguf",
         "pack": "/v/pack", "mtp": "/v/mtp/rt"}
ENV = {"STRATA_PARALLEL": "2", "STRATA_BATCH_MTP": "1", "STRATA_PLE_IO": "mmap",
       "STRATA_VRAM_RESERVE_MIB": "1536", "STRATA_KV": "int8", "STRATA_MAX_CONTEXT": "131072",
       "STRATA_MODEL_NAME": "flash", "STRATA_HOST": "127.0.0.1", "RPCTL_PORT": "8000",
       "VLLM_API_KEY": "sk-SYNTHETIC", "STRATA_ENGINE_LOG": "/workspace/strata-engine.log",
       "STRATA_HUGE_PAGES": "off"}


class SettingsTests(unittest.TestCase):
    def test_the_measured_configuration(self):
        cfg = pod.settings(READY, ENV)
        self.assertEqual(cfg["args"], [
            "--pack", "/v/pack", "--native", "/v/model/Q5_K_M/M-00001-of-00006.gguf",
            "--ple-gguf", "/v/model/Q5_K_M/M-00002-of-00006.gguf", "--ple-io", "mmap",
            "--expert-profile", "/v/release/0.2.16/control/data/expert-profile.bin",
            "--expert-cache", "auto", "--prefill", "auto",
            "--spec", "4", "--spec-min-p", "0.5", "--mtp", "/v/mtp/rt", "--batch-mtp",
            "--max-context", "131072", "--kv", "int8", "--vram-reserve-mib", "1536"])
        self.assertEqual(cfg["parallel"], 2)
        self.assertEqual((cfg["host"], cfg["port"], cfg["api_key"]), ("127.0.0.1", 8000, "sk-SYNTHETIC"))
        self.assertEqual(cfg["model_name"], "flash")
        self.assertEqual(cfg["tokenizer"], "/v/pack/tokenizer")
        # no MCP servers: the key absent, as Strata's server wants (a list stops it)
        self.assertNotIn("mcp_servers", cfg)
        self.assertFalse(cfg["lazy_load"])
        # the expert arena without transparent huge pages, through the config's env
        self.assertEqual(cfg["env"], {"STRATA_NO_ARENA_THP": "1"})

    def test_huge_pages_auto_leaves_strata_to_choose(self):
        for env in (dict(ENV, STRATA_HUGE_PAGES="auto"),
                    {k: v for k, v in ENV.items() if k != "STRATA_HUGE_PAGES"}):
            self.assertNotIn("env", pod.settings(READY, env))

    def test_ram_one_slot_and_no_draft_head(self):
        ready = dict(READY, mtp=None)
        cfg = pod.settings(ready, dict(ENV, STRATA_PLE_IO="ram", STRATA_PARALLEL="1"))
        self.assertNotIn("--ple-io", cfg["args"])
        self.assertIn("--ple-gguf", cfg["args"])
        self.assertNotIn("--spec", cfg["args"])
        self.assertNotIn("--batch-mtp", cfg["args"])
        self.assertNotIn("parallel", cfg)

    def test_no_table_no_table_flags(self):
        cfg = pod.settings(dict(READY, ple=None), ENV)
        self.assertNotIn("--ple-gguf", cfg["args"])
        self.assertNotIn("--ple-io", cfg["args"])

    def test_direct_and_unknown_formats_are_refused(self):
        for key, value in (("STRATA_PLE_IO", "direct"), ("STRATA_KV", "bf16"),
                           ("STRATA_HUGE_PAGES", "on")):
            with self.assertRaises(SystemExit):
                pod.settings(READY, dict(ENV, **{key: value}))


class PreparedTests(unittest.TestCase):
    def test_a_ready_record_must_match_the_release_and_model(self):
        with tempfile.TemporaryDirectory() as d:
            env = {"STRATA_ROOT": d, "STRATA_VERSION": "0.2.16", "RPCTL_MODEL_REPO": "org/M-GGUF",
                   "STRATA_MODEL_FILES": "Q5_K_M/*"}
            with patch.dict("os.environ", env):
                self.assertIsNone(pod.prepared())
                (Path(d) / "ready.json").write_text(json.dumps(READY))
                self.assertEqual(pod.prepared()["pack"], "/v/pack")
            with patch.dict("os.environ", dict(env, STRATA_VERSION="0.2.17")):
                self.assertIsNone(pod.prepared())
            with patch.dict("os.environ", dict(env, STRATA_MODEL_FILES="Q4_K_M/*")):
                self.assertIsNone(pod.prepared())


class ReviewFixTests(unittest.TestCase):
    """The cases of 9.24's review: a dropped download, a changed model, the key's file,
    the first boot's preparation and the watchdog."""

    def test_a_dropped_download_is_retried(self):
        data = b"engine"
        sha = hashlib.sha256(data).hexdigest()
        tries = []

        def urlopen(request, timeout):
            tries.append(1)
            if len(tries) == 1:
                raise pod.http.client.IncompleteRead(b"eng", 3)
            return io.BytesIO(data)
        with tempfile.TemporaryDirectory() as d, patch.object(pod.time, "sleep"):
            dest = pod.fetch("https://example.invalid/e.tar.gz", sha, Path(d) / "e.tar.gz",
                             opener=urlopen)
            self.assertEqual(dest.read_bytes(), data)
        self.assertEqual(len(tries), 2)

    def test_a_changed_model_starts_from_an_empty_folder(self):
        with tempfile.TemporaryDirectory() as d:
            model = Path(d) / "model"
            pod.model_folder(model, "org/M-GGUF", "Q5_K_M/*")
            (model / "Q5_K_M").mkdir()
            (model / "Q5_K_M" / "old.gguf").write_bytes(b"x")
            pod.model_folder(model, "org/M-GGUF", "Q5_K_M/*")
            self.assertTrue((model / "Q5_K_M" / "old.gguf").exists(), "same model: kept")
            pod.model_folder(model, "org/M-GGUF", "Q4_K_M/*")
            self.assertFalse((model / "Q5_K_M").exists(), "another model: emptied")

    def test_patterns_drop_spaces(self):
        self.assertEqual(pod.file_patterns("Q5_K_M/*, MTP/* ,"), ["Q5_K_M/*", "MTP/*"])
        self.assertEqual(pod.file_patterns(""), [])

    def test_the_run_settings_with_the_key_stay_off_the_volume(self):
        with tempfile.TemporaryDirectory() as d:
            env = dict(ENV, STRATA_ROOT=d, STRATA_VERSION="0.2.16",
                       RPCTL_MODEL_REPO="org/M-GGUF", STRATA_MODEL_FILES="Q5_K_M/*")
            (Path(d) / "ready.json").write_text(json.dumps(READY))
            run_config = Path(d) / "container" / "strata-run.json"
            with patch.dict("os.environ", env), patch.object(pod, "RUN_CONFIG", run_config), \
                    patch("sys.stdout", io.StringIO()):
                pod.config()
            self.assertEqual(json.loads(run_config.read_text())["api_key"], "sk-SYNTHETIC")
            self.assertEqual(sorted(p.name for p in Path(d).iterdir()), ["container", "ready.json"])
        self.assertEqual(pod.RUN_CONFIG, Path("/run/strata-run.json"))

    def watch_until(self, flag_age_s, until):
        """The watchdog with /metrics unreadable and the preparing flag flag_age_s old (None:
        no flag); the simulated time when it shut the pod down, or None."""
        clock = [0.0]
        stopped = []

        def sleep(s):
            clock[0] += s
            if clock[0] > until:
                raise StopIteration
        with tempfile.TemporaryDirectory() as d:
            flag = Path(d) / "preparing"
            if flag_age_s is not None:
                flag.touch()
                os_time = __import__("time").time() - flag_age_s
                __import__("os").utime(flag, (os_time, os_time))
            env = {"RUNPOD_POD_ID": "p1", "RUNPOD_API_KEY": "k", "RPCTL_IDLE_TTL_S": "600",
                   "RPCTL_CACHE_DIR": d, "RPCTL_PORT": "8000"}
            with patch.object(pod, "PREPARING", flag), \
                    patch.object(pod.urllib.request, "urlopen", side_effect=OSError), \
                    patch.object(pod, "can_manage", return_value=(True, "ok")), \
                    patch.object(pod, "stop_self",
                                 side_effect=lambda *a: stopped.append(clock[0]) or "stopped"), \
                    patch.dict("os.environ", env), patch("sys.stdout", io.StringIO()):
                try:
                    pod.watchdog(now=lambda: clock[0], sleep=sleep)
                except StopIteration:
                    pass
        return stopped[0] if stopped else None

    def test_preparing_the_volume_is_not_idle(self):
        self.assertIsNone(self.watch_until(flag_age_s=0, until=3 * 600))

    def test_without_preparation_an_unreadable_server_is_idle(self):
        self.assertGreaterEqual(self.watch_until(flag_age_s=None, until=3 * 600), 600)

    def test_a_preparation_past_its_grace_is_idle(self):
        self.assertIsNotNone(self.watch_until(flag_age_s=pod.PREPARE_GRACE_S + 60, until=3 * 600))


if __name__ == "__main__":
    unittest.main()
