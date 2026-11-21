"""viy's helper on a Strata pod (docs/strata-engine.md 4.3), standard library only.

    python3 strata_pod.py prepare   first boot: Strata's release, the model, its pack, draft head
    python3 strata_pod.py config    Strata's run settings, from the environment, every boot
    python3 strata_pod.py watch     the idle watchdog, then ready, then the table's warm read

The boot script exports the settings (STRATA_*, RPCTL_*); the pod's env carries VLLM_API_KEY,
the watchdog's limits and keys. The volume, at STRATA_ROOT:

    release/<version>/engine, control   Strata's two release archives, unpacked (kb d172)
    model/                              MODEL_REPO's files (MODEL_FILES)
    pack/, mtp/                         Strata's pack of the model and its draft head
    ready.json                          what the first boot prepared; its absence starts one

The run settings hold the server's API key, so `config` writes them to the container
(/run/strata-run.json), never to the volume, as vLLM keeps its key in the pod's env alone.

The watchdog is startup.py's (vLLM's pod helper, kb d003), with its rules, phases and clock
file unchanged, but it reads Strata's /metrics?format=prometheus: Strata's /metrics is JSON
unless asked (kb f162), and counters that never move would stop a pod in use. It also counts
the first boot's preparation as activity, for at most two hours: vLLM has no such step.
"""
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tarfile
import threading
import time
import urllib.error
import urllib.request

ACTIVITY_METRICS = ("vllm:prompt_tokens_total", "vllm:generation_tokens_total")
WATCHDOG_POLL_S = 30
GRAPHQL = "https://api.runpod.io/graphql"
REST = "https://rest.runpod.io/v1"
HF_HUB = "huggingface_hub[hf_xet]==2.4.0"
PLE_TENSOR = "per_layer_token_embd"
KV_FORMATS = ("int8", "q4_0", "k8v4", "fp16")
HUGE_PAGES = ("auto", "off")
RUN_CONFIG = Path("/run/strata-run.json")
# While this file exists the volume is being prepared, which the idle watchdog counts as
# activity for at most PREPARE_GRACE_S from its creation (the hard cap still applies).
PREPARING = Path("/run/strata-preparing")
PREPARE_GRACE_S = 2 * 3600


def phase(message):
    print(f"RPCTL_PHASE {message}", flush=True)


def atomic_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(f".{os.getpid()}.tmp")
    temp.write_text(json.dumps(data, indent=1))
    temp.replace(path)


def env(name):
    value = os.environ.get(name, "")
    if not value:
        raise SystemExit(f"strata_pod: {name} is not set")
    return value


def root():
    return Path(env("STRATA_ROOT"))


def release_dir():
    return root() / "release" / env("STRATA_VERSION")


# ------------------------------------------------------------------ the first boot

def wanted():
    """What a prepared volume must hold for this boot: a change of any of it prepares again."""
    return {"version": env("STRATA_VERSION"), "repo": env("RPCTL_MODEL_REPO"),
            "files": os.environ.get("STRATA_MODEL_FILES", "")}


def prepared():
    """The ready record when it matches this boot's release and model, else None."""
    try:
        ready = json.loads((root() / "ready.json").read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(ready, dict) or any(ready.get(k) != v for k, v in wanted().items()):
        return None
    return ready


def fetch(url, sha256, dest, attempts=3, opener=urllib.request.urlopen):
    """Download url to dest, checking its sha256; a file already there with that hash is kept.
    A mismatch is never kept: the archive comes from the internet."""
    if dest.exists() and file_sha256(dest) == sha256:
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    temp = dest.with_suffix(dest.suffix + ".part")
    last = None
    for attempt in range(attempts):
        digest = hashlib.sha256()
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "strata-pod/1"})
            with opener(request, timeout=60) as response, open(temp, "wb") as out:
                while chunk := response.read(1 << 22):
                    digest.update(chunk)
                    out.write(chunk)
        except (OSError, http.client.HTTPException) as error:
            # a connection cut mid-body is http.client.IncompleteRead, not an OSError
            last = f"{type(error).__name__}: {error}"
            if attempt + 1 < attempts:
                time.sleep(5 * (attempt + 1))
            continue
        if digest.hexdigest() != sha256:
            temp.unlink(missing_ok=True)
            raise SystemExit(f"strata_pod: {url} has sha256 {digest.hexdigest()}, not {sha256}")
        temp.replace(dest)
        return dest
    temp.unlink(missing_ok=True)
    raise SystemExit(f"strata_pod: could not download {url} ({last})")


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 22):
            digest.update(chunk)
    return digest.hexdigest()


def unpack(archive, dest):
    """Unpack into a fresh dest. tarfile's data filter refuses paths outside it, links that
    leave it and device files."""
    temp = dest.with_name(dest.name + ".unpacking")
    shutil.rmtree(temp, ignore_errors=True)
    temp.mkdir(parents=True)
    with tarfile.open(archive) as tar:
        tar.extractall(temp, filter="data")
    shutil.rmtree(dest, ignore_errors=True)
    temp.replace(dest)


def engine_exe(engine_dir):
    """The engine binary: the `strata` file next to engine-component.json."""
    for marker in sorted(engine_dir.rglob("engine-component.json")):
        exe = marker.parent / "strata"
        if exe.is_file():
            return exe
    raise SystemExit(f"strata_pod: no engine binary under {engine_dir}")


def gguf_tensor_names(path):
    """The tensor names of one GGUF file, from its header."""
    scalar = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}

    def read(f, fmt):
        return struct.unpack(fmt, f.read(struct.calcsize(fmt)))[0]

    def skip(f, kind):
        if kind in scalar:
            f.seek(scalar[kind], 1)
        elif kind == 8:
            f.seek(read(f, "<Q"), 1)
        elif kind == 9:
            inner, count = read(f, "<I"), read(f, "<Q")
            for _ in range(count):
                skip(f, inner)
        else:
            raise ValueError(f"{path}: unknown GGUF value type {kind}")

    with open(path, "rb") as f:
        if f.read(4) != b"GGUF":
            raise ValueError(f"{path}: not a GGUF file")
        read(f, "<I")
        tensors, keys = read(f, "<Q"), read(f, "<Q")
        for _ in range(keys):
            f.seek(read(f, "<Q"), 1)
            skip(f, read(f, "<I"))
        names = []
        for _ in range(tensors):
            names.append(f.read(read(f, "<Q")).decode("utf-8", "replace"))
            dims = read(f, "<I")
            f.seek(8 * dims + 4 + 8, 1)
        return names


def model_files(model_dir):
    """(trunk, ple): the model's first GGUF file, and the one that holds the n-gram table
    (per_layer_token_embd) or None. A model split in several files starts at its
    -00001-of-N file; files of an MTP variant (an MTP folder or -MTP- name) are not the trunk."""
    ggufs = sorted(p for p in model_dir.rglob("*.gguf")
                   if "MTP" not in p.relative_to(model_dir).parts and "-MTP-" not in p.name)
    firsts = [p for p in ggufs if "-00001-of-" in p.name]
    if len(firsts) == 1:
        trunk = firsts[0]
        prefix, total = trunk.name.split("-00001-of-")
        parts = [p for p in ggufs if p.parent == trunk.parent
                 and p.name.startswith(prefix + "-") and p.name.endswith("-of-" + total)]
    elif not firsts and len(ggufs) == 1:
        trunk, parts = ggufs[0], ggufs
    else:
        raise SystemExit(f"strata_pod: cannot tell the model's first GGUF file in {model_dir}: "
                         f"{[str(p.relative_to(model_dir)) for p in (firsts or ggufs)][:6]}")
    ple = next((p for p in parts if any(PLE_TENSOR in n for n in gguf_tensor_names(p))), None)
    return trunk, ple


def model_folder(model, repo, files):
    """The folder MODEL_REPO's files go to. Another model's files would sit next to this one's
    (two trunks), so a changed MODEL_REPO or MODEL_FILES starts from an empty folder; the same
    one keeps what is there, and its download resumes."""
    marker = model / ".viy-model.json"
    try:
        same = json.loads(marker.read_text()) == {"repo": repo, "files": files}
    except (OSError, ValueError):
        same = False
    if not same:
        shutil.rmtree(model, ignore_errors=True)
        atomic_json(marker, {"repo": repo, "files": files})
    return model


def file_patterns(files):
    """MODEL_FILES as download patterns: comma separated, spaces around each dropped; none
    for the whole repo."""
    return [p.strip() for p in files.split(",") if p.strip()]


def run(*argv, **kwargs):
    print("+", " ".join(str(a) for a in argv), flush=True)
    subprocess.run([str(a) for a in argv], check=True, **kwargs)


def prepare():
    """First boot, or a new release or model: everything Strata needs, onto the volume, the
    ready record last. A boot whose volume is ready returns at once."""
    if prepared():
        return
    phase("preparing the volume (first boot)")
    PREPARING.touch()
    try:
        _prepare()
    finally:
        PREPARING.unlink(missing_ok=True)


def _prepare():
    t0 = time.time()
    rel = release_dir()
    for part in ("ENGINE", "CONTROL"):
        url, sha = env(f"STRATA_{part}_URL"), env(f"STRATA_{part}_SHA256")
        archive = fetch(url, sha, root() / "downloads" / url.rsplit("/", 1)[1])
        unpack(archive, rel / part.lower())
    phase(f"Strata {env('STRATA_VERSION')} unpacked ({time.time() - t0:.0f} s)")
    py = rel / "control" / "python" / "bin" / "python3"
    control = rel / "control"

    venv = root() / "venv"
    if not (venv / "bin" / "python").exists():
        run(sys.executable, "-m", "venv", venv)
    run(venv / "bin" / "pip", "install", "-q", HF_HUB)
    repo, files = env("RPCTL_MODEL_REPO"), os.environ.get("STRATA_MODEL_FILES", "")
    model = model_folder(root() / "model", repo, files)
    patterns = file_patterns(files)
    run(venv / "bin" / "python", "-c",
        "import sys; from huggingface_hub import snapshot_download; "
        "snapshot_download(sys.argv[1], allow_patterns=sys.argv[3:] or None, local_dir=sys.argv[2])",
        repo, model, *(patterns or []))
    phase(f"model downloaded ({time.time() - t0:.0f} s)")

    trunk, ple = model_files(model)
    pack = root() / "pack"
    shutil.rmtree(pack, ignore_errors=True)
    run(py, control / "tools" / "iq_pack.py", "--gguf", trunk, "--out", pack, "--compat-bf16")
    # The draft head: the original model's, as Strata's guide for third-party GGUFs does. Without
    # it Strata drafts by lookup only, which works, slower.
    mtp = root() / "mtp"
    shutil.rmtree(mtp, ignore_errors=True)
    try:
        run(py, control / "tools" / "mtp_fetch.py", "fetch", "--out", mtp)
        run(py, control / "tools" / "mtp_pack.py", "--src", mtp, "--experts", "q2_0",
            "--out", mtp / "mtp-q2_0.gguf")
        run(py, control / "tools" / "mtp_rt.py", "--gguf", mtp / "mtp-q2_0.gguf", "--out", mtp / "rt")
        shutil.copy(control / "data" / "draft_vocab.bin", mtp / "rt" / "draft_vocab.bin")
        draft = str(mtp / "rt")
    except (subprocess.CalledProcessError, OSError) as error:
        phase(f"draft head failed ({type(error).__name__}); Strata drafts by lookup only")
        draft = None
    ready = dict(wanted(), engine=str(engine_exe(rel / "engine")), control=str(control),
                 trunk=str(trunk), ple=str(ple) if ple else None, pack=str(pack), mtp=draft,
                 prepared_s=round(time.time() - t0, 1))
    atomic_json(root() / "ready.json", ready)
    phase(f"volume prepared ({ready['prepared_s']:.0f} s)")


# ------------------------------------------------------------------ every boot

def settings(ready, environ=os.environ):
    """Strata's run settings for server.py --config, from the ready record and the
    environment. The client checked every value before the pod was made (viy-strata)."""
    get = environ.get
    parallel = int(get("STRATA_PARALLEL") or 1)
    ple_io = get("STRATA_PLE_IO") or "mmap"
    if ple_io not in ("mmap", "ram"):
        raise SystemExit(f"strata_pod: STRATA_PLE_IO must be mmap or ram, not {ple_io!r}")
    kv = get("STRATA_KV") or "int8"
    if kv not in KV_FORMATS:
        raise SystemExit(f"strata_pod: STRATA_KV must be one of {', '.join(KV_FORMATS)}, not {kv!r}")
    huge = get("STRATA_HUGE_PAGES") or "auto"
    if huge not in HUGE_PAGES:
        raise SystemExit(f"strata_pod: STRATA_HUGE_PAGES must be auto or off, not {huge!r}")
    args = ["--pack", ready["pack"], "--native", ready["trunk"]]
    if ready.get("ple"):
        args += ["--ple-gguf", ready["ple"]]
        if ple_io == "mmap":
            args += ["--ple-io", "mmap"]
    args += ["--expert-profile", str(Path(ready["control"]) / "data" / "expert-profile.bin"),
             "--expert-cache", "auto", "--prefill", "auto"]
    if ready.get("mtp"):
        args += ["--spec", "4", "--spec-min-p", "0.5", "--mtp", ready["mtp"]]
        if parallel > 1 and get("STRATA_BATCH_MTP") == "1":
            args.append("--batch-mtp")
    args += ["--max-context", get("STRATA_MAX_CONTEXT") or "131072", "--kv", kv,
             "--vram-reserve-mib", get("STRATA_VRAM_RESERVE_MIB") or "1536"]
    cfg = {"exe": ready["engine"], "args": args, "cwd": str(Path(ready["pack"]).parent),
           "tokenizer": str(Path(ready["pack"]) / "tokenizer"),
           "model_name": get("STRATA_MODEL_NAME") or "model",
           "log": get("STRATA_ENGINE_LOG") or "/workspace/strata-engine.log",
           "host": get("STRATA_HOST") or "127.0.0.1", "port": int(get("RPCTL_PORT") or 8000),
           "api_key": get("VLLM_API_KEY", ""), "lazy_load": False}
    if parallel > 1:
        cfg["parallel"] = parallel
    if huge == "off":
        # The engine's environment, as Strata's own setup writes it (setup_config.py):
        # the expert arena without transparent huge pages, whose compaction can stall
        # the load on a host short of free 2 MiB blocks (Strata #2126).
        cfg["env"] = {"STRATA_NO_ARENA_THP": "1"}
    return cfg


def config():
    ready = prepared()
    if not ready:
        raise SystemExit("strata_pod: the volume is not prepared (prepare first)")
    atomic_json(RUN_CONFIG, settings(ready))
    # for the boot script: Strata's Python and its server
    print(f"{ready['control']}/python/bin/python3 {ready['control']}/serve/server.py")


# ------------------------------------------------------------------ the watchdog (startup.py's)

def token_count(metrics_text):
    """Tokens Strata has taken in and produced so far. Any change between two
    polls is activity: a streaming answer moves it every poll, while health
    checks, `viy status` and an open but idle chat never do. A request that is
    stuck (running, but producing nothing) does not move it either."""
    total = 0.0
    for line in metrics_text.splitlines():
        if line.startswith(ACTIVITY_METRICS):
            try:
                total += float(line.rsplit(None, 1)[1])
            except (IndexError, ValueError):
                pass
    return total


class IdleClock:
    """Decides when the pod has been idle for ttl_s, or alive for max_s.

    Wall-clock times are persisted per pod id on the network volume, so a
    container that crash-loops (RunPod restarts it, and the new container
    starts a new watchdog) carries on the same countdown instead of
    restarting it forever."""

    def __init__(self, path, pod_id, ttl_s, max_s, now):
        self.path, self.pod_id, self.ttl_s, self.max_s = path, pod_id, ttl_s, max_s
        self.last_tokens = None
        try:
            saved = json.loads(path.read_text())
            if saved["pod_id"] != pod_id:
                raise ValueError("another pod")
            self.started, self.last_activity = float(saved["started"]), float(saved["last_activity"])
        except (OSError, ValueError, KeyError, TypeError):
            # The clock starts with the container, not when Strata is ready, so a
            # boot that never serves is shut down too.
            self.started = self.last_activity = now
        self.save(now)

    def tick(self, now, tokens):
        """tokens is None when /metrics could not be read, which counts as
        idle: an error must lead to a shutdown, never to running forever."""
        if tokens is not None:
            if self.last_tokens is not None and tokens != self.last_tokens:
                self.last_activity = now
            self.last_tokens = tokens
        self.save(now)
        return self.expired(now)

    def expired(self, now):
        if now - self.last_activity >= self.ttl_s:
            return f"idle for {(now - self.last_activity) / 60:.0f} min"
        if now - self.started >= self.max_s:
            return f"running for {(now - self.started) / 3600:.1f} h (hard cap)"
        return None

    def state(self, now):
        return {"pod_id": self.pod_id, "started": self.started,
                "last_activity": self.last_activity, "checked": now,
                "ttl_s": self.ttl_s, "max_s": self.max_s}

    def save(self, now):
        try:
            atomic_json(self.path, self.state(now))
        except OSError:
            pass  # the in-memory clock still works


def runpod_call(method, url, key, query=None):
    """(ok, detail) for one RunPod API call. Never raises."""
    data = json.dumps({"query": query}).encode() if query else None
    request = urllib.request.Request(
        url, method=method, data=data,
        # Cloudflare in front of RunPod answers 403 to urllib's default
        # User-Agent, whatever the key.
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + key,
                 "User-Agent": "rpctl-watchdog/1"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        return False, f"HTTP {error.code}"
    except OSError as error:
        return False, type(error).__name__
    if query:
        try:
            errors = json.loads(body or b"{}").get("errors")
        except ValueError:
            return False, "bad response"
        if errors:
            return False, str(errors[0].get("message", "error"))[:160]
    return True, "ok"


def can_manage(pod_id, key):
    """Whether RunPod lets this key see the pod: checked once at boot so a key
    that cannot stop the pod is reported straight away, not an hour later."""
    ok, detail = runpod_call("GET", f"{REST}/pods/{pod_id}", key)
    if ok:
        return True, detail
    ok, gql_detail = runpod_call("POST", GRAPHQL, key,
                                 'query { pod(input: {podId: "%s"}) { id } }' % pod_id)
    return ok, "ok" if ok else f"REST {detail}, GraphQL {gql_detail}"


def terminate_self(pod_id, key):
    """Terminate this pod, the same as `viy down`; if RunPod refuses that, stop
    it, which ends GPU billing too (viy reap terminates it later). A restricted
    key may be allowed on only one of the two APIs, so both are tried."""
    tried = []
    for label, method, url, query in (
            ("REST terminate", "DELETE", f"{REST}/pods/{pod_id}", None),
            ("GraphQL terminate", "POST", GRAPHQL,
             'mutation { podTerminate(input: {podId: "%s"}) }' % pod_id)):
        ok, detail = runpod_call(method, url, key, query)
        if ok:
            return "terminated"
        tried.append(f"{label}: {detail}")
    for label, method, url, query in (
            ("REST stop", "POST", f"{REST}/pods/{pod_id}/stop", None),
            ("GraphQL stop", "POST", GRAPHQL,
             'mutation { podStop(input: {podId: "%s"}) { id } }' % pod_id)):
        ok, detail = runpod_call(method, url, key, query)
        if ok:
            return "stopped"
        tried.append(f"{label}: {detail}")
    raise RuntimeError("; ".join(tried))


def metrics_request():
    """Strata's Prometheus counters: asked for both ways it takes (kb f162)."""
    return urllib.request.Request(
        f"http://127.0.0.1:{os.environ['RPCTL_PORT']}/metrics?format=prometheus",
        headers={"Authorization": "Bearer " + os.environ.get("VLLM_API_KEY", ""),
                 "Accept": "text/plain"})


def watchdog(now=time.time, sleep=time.sleep):
    """Shuts the pod down after RPCTL_IDLE_TTL_S without tokens flowing, or
    after RPCTL_MAX_POD_S in total. Runs in the background from second 0 and
    is never waited on, so it adds nothing to the boot."""
    pod_id = os.environ.get("RUNPOD_POD_ID", "")
    # The pod-scoped key RunPod injects can terminate this pod (verified
    # 2026-06-26). RPCTL_WATCHDOG_KEY (RUNPOD_WATCHDOG_API_KEY) overrides it.
    injected = os.environ.get("RUNPOD_API_KEY", "")
    key = os.environ.get("RPCTL_WATCHDOG_KEY") or injected
    ttl_s = float(os.environ.get("RPCTL_IDLE_TTL_S") or 0)
    max_s = float(os.environ.get("RPCTL_MAX_POD_S") or 0) or float("inf")
    if not ttl_s:
        phase("watchdog off (IDLE_TTL_MIN=0)")
        return
    if not (pod_id and key):
        phase("WATCHDOG CANNOT STOP THIS POD: no RunPod key (set RUNPOD_WATCHDOG_API_KEY)")
        return
    clock = IdleClock(Path(os.environ["RPCTL_CACHE_DIR"]) / f"watchdog-{pod_id}.json",
                      pod_id, ttl_s, max_s, now())
    can_stop, detail = can_manage(pod_id, key)
    if injected and key != injected:
        # Diagnostic only: if RunPod's own key works, no dedicated key is needed.
        phase("watchdog: the pod's injected key %s"
              % ("can see this pod" if can_manage(pod_id, injected)[0] else "is refused"))
    if can_stop:
        phase(f"watchdog armed: idle {ttl_s / 60:.0f} min, cap {max_s / 3600:.1f} h")
    else:
        # The clock still runs and is persisted, so viy status can show it.
        phase(f"WATCHDOG CANNOT STOP THIS POD: RunPod refused its key ({detail})")
    request = metrics_request()
    reported = False
    while True:
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                tokens = token_count(response.read().decode(errors="replace"))
        except (OSError, ValueError, http.client.HTTPException):
            tokens = None
        if preparing(time.time()):
            # The first boot's download and pack: no server to ask yet, and not idle.
            clock.last_activity = now()
        reason = clock.tick(now(), tokens)
        if reason and not can_stop:
            if not reported:
                phase(f"watchdog: {reason}, but this pod cannot stop itself")
                reported = True
        elif reason:
            phase(f"watchdog: {reason}; shutting the pod down")
            try:
                phase(f"watchdog: pod {terminate_self(pod_id, key)}")
                return
            except RuntimeError as error:
                phase(f"WATCHDOG FAILED TO STOP THIS POD ({error}); retrying")
        sleep(WATCHDOG_POLL_S)


def preparing(wall):
    """Whether the volume is being prepared, within PREPARE_GRACE_S of its start."""
    try:
        return wall - PREPARING.stat().st_mtime < PREPARE_GRACE_S
    except OSError:
        return False


def _watchdog_forever():
    # A bug in the watchdog must not end it: that would leave the pod billing.
    while True:
        try:
            return watchdog()
        except Exception as error:
            phase(f"watchdog error ({type(error).__name__}); restarting it")
            time.sleep(WATCHDOG_POLL_S)


def loaded(body):
    """Whether a /health reply says the model is loaded. Strata refuses connections until
    then and answers 200 with "loaded": true after (kb f161)."""
    try:
        return json.loads(body).get("loaded") is True
    except (ValueError, AttributeError):
        return False


def warm(path, streams=4):
    """Read a file into the page cache in `streams` parallel slices; seconds it took."""
    size = path.stat().st_size
    chunk = size // streams + 1
    t0 = time.monotonic()

    def read(i):
        with open(path, "rb", buffering=0) as f:
            f.seek(i * chunk)
            left = min(chunk, max(size - i * chunk, 0))
            while left > 0 and (block := f.read(min(1 << 24, left))):
                left -= len(block)

    threads = [threading.Thread(target=read, args=(i,)) for i in range(streams)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return time.monotonic() - t0


def watch():
    threading.Thread(target=_watchdog_forever, name="watchdog").start()
    started = time.monotonic()
    request = urllib.request.Request(
        f"http://127.0.0.1:{os.environ['RPCTL_PORT']}/health",
        headers={"Authorization": "Bearer " + os.environ.get("VLLM_API_KEY", "")})
    # The first boot's preparation comes before the server: wait as long as the watchdog allows.
    while time.monotonic() - started < 6 * 3600:
        try:
            with urllib.request.urlopen(request, timeout=2) as response:
                if response.status == 200 and loaded(response.read()):
                    break
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(2)
    else:
        return
    phase("model ready")
    ready = prepared()
    if (os.environ.get("STRATA_PLE_WARM") == "1" and os.environ.get("STRATA_PLE_IO") == "mmap"
            and ready and ready.get("ple")):
        path = Path(ready["ple"])
        try:
            seconds = warm(path)
            phase(f"n-gram table ({path.stat().st_size / 2**30:.1f} GiB) in the page cache "
                  f"after {seconds:.0f} s")
        except OSError as error:
            phase(f"n-gram table read failed ({type(error).__name__}); Strata pages it in itself")


if __name__ == "__main__":
    {"prepare": prepare, "config": config, "watch": watch}[sys.argv[1]]()
