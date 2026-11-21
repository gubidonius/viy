use std::path::Path;
use std::process::Command;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::stages::STAGES;
use crate::stages::hint;

fn with<'a>(overrides: impl IntoIterator<Item = (&'a str, &'a str)>) -> Config {
    let mut cfg = Config::defaults();
    cfg.set("ENGINE", "strata");
    for (key, value) in overrides {
        cfg.set(key, value);
    }
    cfg
}

fn script() -> String {
    build_boot_script(&with([])).unwrap()
}

/// The script in a scratch folder, removed with the returned guard.
fn scratch_file(name: &str, text: &str) -> (ScratchDir, std::path::PathBuf) {
    let dir = ScratchDir::new("viy-strata").unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

#[test]
fn the_script_parses() {
    let (_dir, path) = scratch_file("boot.sh", &script());
    let out = Command::new("bash").arg("-n").arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// shellcheck on the boot script, when this machine has it.
#[test]
fn shellcheck_finds_nothing() {
    let Ok(out) = Command::new("shellcheck").arg("--version").output() else {
        eprintln!("shellcheck is not installed; bash -n still checks the script");
        return;
    };
    assert!(out.status.success());
    let (_dir, path) = scratch_file("check.sh", &format!("#!/bin/bash\n{}", script()));
    let out = Command::new("shellcheck")
        .args(["-S", "warning"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn the_settings_are_exported_before_the_script() {
    let script = script();
    assert!(script.starts_with("set -uo pipefail\nexport STRATA_ROOT=/workspace/strata\n"));
    for line in [
        "export STRATA_VERSION=0.2.16",
        "export STRATA_ENGINE_SHA256=6723e1b6b6042bfe1d368723ebf873afb0fa65f4f1908d948dd865098f6cfb9e",
        "export STRATA_CONTROL_SHA256=045b6a7d7fbfbddab814794db5763d6244c887a394403b9fe0e8a4714d9bef8c",
        "export STRATA_PARALLEL=2",
        "export STRATA_BATCH_MTP=1",
        "export STRATA_PLE_IO=mmap",
        "export STRATA_PLE_WARM=1",
        "export STRATA_VRAM_RESERVE_MIB=1536",
        "export STRATA_KV=int8",
        "export STRATA_HUGE_PAGES=off",
        "export STRATA_HOST=127.0.0.1",
        "export RPCTL_PORT=8000",
        "export RPCTL_CACHE_DIR=/workspace/rpctl-cache",
    ] {
        assert!(script.contains(&format!("\n{line}\n")), "{line}");
    }
    let cfg = with([("MODEL_FILES", "Q5_K_M/*"), ("TUNNEL", "0")]);
    let script = build_boot_script(&cfg).unwrap();
    assert!(script.contains("\nexport STRATA_MODEL_FILES='Q5_K_M/*'\n"));
    assert!(script.contains("\nexport STRATA_HOST=0.0.0.0\n"));
}

#[test]
fn the_helper_is_written_out_whole() {
    let script = script();
    assert!(
        !STRATA_POD_PY.contains("STRATA_POD_PY"),
        "the heredoc's end marker"
    );
    let start = script.find("<<'STRATA_POD_PY'\n").unwrap() + "<<'STRATA_POD_PY'\n".len();
    let end = script[start..].find("\nSTRATA_POD_PY\n").unwrap();
    assert_eq!(&script[start..start + end], STRATA_POD_PY.trim_end());
}

/// The watchdog starts before the first boot's preparation, which can take
/// minutes, and the server runs last, in the foreground.
#[test]
fn the_watchdog_runs_from_the_start_and_the_server_last() {
    let script = script();
    let at = |text: &str| script.find(text).unwrap_or_else(|| panic!("{text}"));
    assert!(at("strata-pod.py watch &") < at("strata-pod.py prepare"));
    assert!(at("strata-pod.py prepare") < at("strata-pod.py config"));
    assert!(at("strata-pod.py config") < at("RPCTL_PHASE starting Strata"));
    // the run settings hold the API key: the container's, never the volume's
    assert!(script.contains("--config /run/strata-run.json"));
    assert!(
        script
            .trim_end()
            .ends_with("--port \"$RPCTL_PORT\" 2>&1 | tee -a /workspace/strata.log")
    );
}

#[test]
fn bad_settings_are_refused_before_a_pod_is_made() {
    for (key, value, said) in [
        (
            "STRATA_PLE_IO",
            "direct",
            r#"STRATA_PLE_IO must be mmap or ram (direct reads the volume at 13 MB/s, kb f157), not "direct""#,
        ),
        (
            "STRATA_KV",
            "bf16",
            r#"STRATA_KV must be int8, q4_0, k8v4 or fp16, not "bf16""#,
        ),
        (
            "STRATA_HUGE_PAGES",
            "on",
            r#"STRATA_HUGE_PAGES must be auto or off, not "on""#,
        ),
        (
            "STRATA_PARALLEL",
            "9",
            r#"STRATA_PARALLEL must be a whole number from 1 to 8, not "9""#,
        ),
        (
            "STRATA_PARALLEL",
            "two",
            r#"STRATA_PARALLEL must be a whole number, not "two""#,
        ),
        (
            "STRATA_BATCH_MTP",
            "yes",
            r#"STRATA_BATCH_MTP must be 0 or 1, not "yes""#,
        ),
        (
            "MIN_RAM_GB",
            "lots",
            r#"MIN_RAM_GB must be a whole number, not "lots""#,
        ),
    ] {
        let error = build_pod_body(&with([(key, value)]), "k", "v", None).unwrap_err();
        assert_eq!(error.to_string(), said, "{key}");
    }
}

#[test]
fn the_body_is_the_shared_one_around_strata() {
    let body = build_pod_body(&with([]), "sk-SYNTHETIC", "vol1", Some("ssh-ed25519 AAAA")).unwrap();
    assert_eq!(
        body["imageName"],
        "runpod/pytorch:1.0.3-cu1281-torch280-ubuntu2404"
    );
    assert_eq!(body["minRAMPerGPU"], 150);
    assert_eq!(body["networkVolumeId"], "vol1");
    assert_eq!(body["dockerEntrypoint"], json!(["/bin/bash", "-c"]));
    assert_eq!(body["dockerStartCmd"], json!([script()]));
    assert_eq!(body["ports"], json!(["22/tcp"]));
    assert_eq!(body["allowedCudaVersions"], json!(["13.0"]));
    let env = &body["env"];
    assert_eq!(env["VLLM_API_KEY"], "sk-SYNTHETIC");
    assert_eq!(env["PUBLIC_KEY"], "ssh-ed25519 AAAA");
    assert_eq!(env["RPCTL_IDLE_TTL_S"], "3600");
    assert_eq!(env["RPCTL_MAX_POD_S"], "28800");
    let body = build_pod_body(&with([("MIN_RAM_GB", "0")]), "k", "v", None).unwrap();
    assert!(body.get("minRAMPerGPU").is_none());
}

#[test]
fn the_release_is_pinned() {
    for archive in [release::ENGINE, release::CONTROL] {
        assert!(archive.url.starts_with(&format!(
            "https://github.com/Niko1221/Strata/releases/download/v{}/",
            release::VERSION
        )));
        assert_eq!(archive.sha256.len(), 64);
        assert!(archive.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}

#[test]
fn every_stage_has_a_hint_and_the_scripts_markers_are_in_it() {
    let script = script();
    for (key, _, marker) in STAGES {
        assert!(!hint(key).is_empty(), "{key}");
        if marker.starts_with("RPCTL_PHASE") || marker.starts_with("=== boot") {
            assert!(script.contains(marker), "{key}: {marker:?}");
        }
    }
}

/// The helper's tests: the watchdog's cases from startup_tests.py, the
/// preparation and the run settings.
#[test]
fn strata_pod_passes_its_tests() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/strata_pod_tests.py");
    let out = Command::new("python3")
        .args(["-I", "-B"])
        .arg(&tests)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Ran 43 tests"), "{stderr}");
}
