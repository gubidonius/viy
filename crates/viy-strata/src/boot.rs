//! What a Strata pod runs: its boot script and create-pod body
//! (docs/strata-engine.md 4.3). The script is `assets/strata_boot.sh` after
//! the settings as exports, with `assets/strata_pod.py` written out by it;
//! the body is the one every engine shares (viy-engine's `pod`) around
//! STRATA_IMAGE and that script, plus the host RAM the pod asks for. Every
//! setting is checked here, before a pod is made.

use serde_json::Value;
use serde_json::json;
use viy_config::Config;
use viy_config::config::tunnel_enabled;
use viy_engine::BadValue;
use viy_engine::pod;
use viy_engine::pod::PodStart;
use viy_engine::pod::at;
use viy_engine::pod::quote;

use crate::release;

/// The pod helper: the first boot's preparation, the run settings, the
/// watchdog.
pub const STRATA_POD_PY: &str = include_str!("../assets/strata_pod.py");

/// The boot script after its exports; `@@STRATA_POD_PY@@` is the helper.
const BOOT_SH: &str = include_str!("../assets/strata_boot.sh");

/// Where Strata, the model and its pack live on the volume.
pub const ROOT: &str = "/workspace/strata";

/// The n-gram table's ways to read it. `direct` (per token from the volume)
/// is left out: 13 MB/s of random reads on network storage (kb f157).
pub const PLE_IO: &[&str] = &["mmap", "ram"];

/// Strata's KV formats.
pub const KV: &[&str] = &["int8", "q4_0", "k8v4", "fp16"];

fn one_of<'a>(
    cfg: &'a Config,
    key: &'static str,
    allowed: &[&str],
    kind: &'static str,
) -> Result<&'a str, BadValue> {
    let value = at(cfg, key).trim();
    if allowed.contains(&value) {
        Ok(value)
    } else {
        Err(BadValue {
            key,
            value: value.to_string(),
            kind,
        })
    }
}

fn whole_in(
    cfg: &Config,
    key: &'static str,
    range: std::ops::RangeInclusive<i64>,
    kind: &'static str,
) -> Result<i64, BadValue> {
    let n = pod::whole(cfg, key)?;
    if range.contains(&n) {
        Ok(n)
    } else {
        Err(BadValue {
            key,
            value: at(cfg, key).to_string(),
            kind,
        })
    }
}

/// The boot script's settings, in order: what `strata_pod.py` and the
/// script read.
pub fn settings(cfg: &Config) -> Result<Vec<(&'static str, String)>, BadValue> {
    let flag = |key| one_of(cfg, key, &["0", "1"], "0 or 1").map(str::to_string);
    let host = if tunnel_enabled(cfg) {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    Ok(vec![
        ("STRATA_ROOT", ROOT.to_string()),
        ("STRATA_VERSION", release::VERSION.to_string()),
        ("STRATA_ENGINE_URL", release::ENGINE.url.to_string()),
        ("STRATA_ENGINE_SHA256", release::ENGINE.sha256.to_string()),
        ("STRATA_CONTROL_URL", release::CONTROL.url.to_string()),
        ("STRATA_CONTROL_SHA256", release::CONTROL.sha256.to_string()),
        ("RPCTL_MODEL_REPO", at(cfg, "MODEL_REPO").to_string()),
        ("STRATA_MODEL_FILES", at(cfg, "MODEL_FILES").to_string()),
        (
            "STRATA_MODEL_NAME",
            at(cfg, "SERVED_MODEL_NAME").to_string(),
        ),
        (
            "STRATA_MAX_CONTEXT",
            whole_in(
                cfg,
                "MAX_MODEL_LEN",
                1..=1 << 30,
                "a whole number of tokens",
            )?
            .to_string(),
        ),
        (
            "STRATA_PARALLEL",
            whole_in(cfg, "STRATA_PARALLEL", 1..=8, "a whole number from 1 to 8")?.to_string(),
        ),
        ("STRATA_BATCH_MTP", flag("STRATA_BATCH_MTP")?),
        (
            "STRATA_PLE_IO",
            one_of(
                cfg,
                "STRATA_PLE_IO",
                PLE_IO,
                "mmap or ram (direct reads the volume at 13 MB/s, kb f157)",
            )?
            .to_string(),
        ),
        ("STRATA_PLE_WARM", flag("STRATA_PLE_WARM")?),
        (
            "STRATA_VRAM_RESERVE_MIB",
            whole_in(
                cfg,
                "STRATA_VRAM_RESERVE_MIB",
                0..=1 << 20,
                "a whole number of MiB",
            )?
            .to_string(),
        ),
        (
            "STRATA_KV",
            one_of(cfg, "STRATA_KV", KV, "int8, q4_0, k8v4 or fp16")?.to_string(),
        ),
        ("STRATA_HOST", host.to_string()),
        (
            "STRATA_ENGINE_LOG",
            "/workspace/strata-engine.log".to_string(),
        ),
        // read into RAM at load unless --ple-io says otherwise
        ("STRATA_PLE_AUTO_RAM", "1".to_string()),
        ("RPCTL_PORT", at(cfg, "VLLM_PORT").to_string()),
        ("RPCTL_CACHE_DIR", "/workspace/rpctl-cache".to_string()),
    ])
}

/// The script the pod's `bash -c` runs.
pub fn build_boot_script(cfg: &Config) -> Result<String, BadValue> {
    let exports = settings(cfg)?
        .iter()
        .map(|(key, value)| format!("export {key}={}", quote(value)))
        .collect::<Vec<_>>()
        .join("\n");
    let body = BOOT_SH.replace("@@STRATA_POD_PY@@", STRATA_POD_PY.trim_end());
    Ok(format!("set -uo pipefail\n{exports}\n{body}"))
}

/// The create-pod request body: the shared body around STRATA_IMAGE and the
/// boot script, and `minRAMPerGPU` (v1's name) from MIN_RAM_GB: Strata
/// keeps the model's experts and n-gram table in RAM.
pub fn build_pod_body(
    cfg: &Config,
    api_key: &str,
    volume_id: &str,
    public_key: Option<&str>,
) -> Result<Value, BadValue> {
    let env = pod::env(cfg, api_key, public_key)?;
    let start = PodStart {
        image: at(cfg, "STRATA_IMAGE").to_string(),
        script: build_boot_script(cfg)?,
    };
    let min_ram = whole_in(cfg, "MIN_RAM_GB", 0..=1 << 20, "a whole number of GB")?;
    let mut body = pod::body(cfg, start, env, volume_id)?;
    if min_ram > 0 {
        body["minRAMPerGPU"] = json!(min_ram);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "boot_tests.rs"]
mod tests;
