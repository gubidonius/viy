//! What every engine's pod has in common: the create-pod body around the
//! engine's image and start script, the pod's environment (the API key, the
//! watchdog's limits and keys), the ports, and the shell quoting and number
//! checks the bodies need. Moved out of viy-vllm's boot.rs unchanged, so
//! vLLM's body is still rpctl's byte for byte (kb d003; the `pod/boot`
//! fixture checks it).

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_config::Config;
use viy_config::config::gpu_type_list;
use viy_config::config::tunnel_enabled;

use crate::BadValue;

/// What an engine's pod runs: its image, and the script `bash -c` starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodStart {
    pub image: String,
    pub script: String,
}

/// The value of `key`, empty when unset.
pub fn at<'a>(cfg: &'a Config, key: &str) -> &'a str {
    cfg.get(key).unwrap_or("")
}

/// A word as Python's `shlex.quote` writes it, which rpctl's script uses:
/// bare when it is only ASCII letters, digits and `_@%+=:,./-`, else in
/// single quotes with each `'` as `'"'"'`. Not the `shlex` crate's quoting,
/// which differs, because the script must be rpctl's byte for byte.
pub fn quote(word: &str) -> String {
    let safe = |b: u8| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b);
    if word.is_empty() {
        "''".to_string()
    } else if word.bytes().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r#"'"'"'"#))
    }
}

/// A whole number, as Python's `int()` of the value.
pub fn whole(cfg: &Config, key: &'static str) -> Result<i64, BadValue> {
    let value = at(cfg, key);
    value.trim().parse().map_err(|_| BadValue {
        key,
        value: value.to_string(),
        kind: "a whole number",
    })
}

/// `key` (a count of `unit`s, empty for 0) in whole seconds, rounded toward
/// zero, as rpctl's `str(int(float(v or 0) * unit))` writes it.
pub fn seconds(cfg: &Config, key: &'static str, unit: f64) -> Result<String, BadValue> {
    let value = at(cfg, key);
    let bad = || BadValue {
        key,
        value: value.to_string(),
        kind: "a number",
    };
    let n: f64 = if value.is_empty() {
        0.0
    } else {
        value.trim().parse().map_err(|_| bad())?
    };
    let s = (n * unit).trunc();
    if !s.is_finite() {
        return Err(bad());
    }
    // + 0.0 turns -0 into 0.
    Ok(format!("{:.0}", s + 0.0))
}

/// The ports the pod exposes. Tunnel mode exposes only the direct TCP port
/// for sshd: RunPod's `/http` ports are fronted by a proxy that terminates
/// TLS, so anything served through one is plaintext inside their network;
/// not declaring it removes that path.
pub fn ports(cfg: &Config) -> Vec<String> {
    let tcp = "22/tcp".to_string();
    if tunnel_enabled(cfg) {
        vec![tcp]
    } else {
        vec![format!("{}/http", at(cfg, "VLLM_PORT")), tcp]
    }
}

/// The pod's environment, whatever the engine: the Hugging Face cache on
/// the volume, the server's API key, the watchdog's limits and key, the
/// SSH key sshd authorizes.
pub fn env(
    cfg: &Config,
    api_key: &str,
    public_key: Option<&str>,
) -> Result<Map<String, Value>, BadValue> {
    let mut env = Map::new();
    let mut set = |key: &str, value: &str| env.insert(key.to_string(), json!(value));
    set("HF_HOME", "/workspace/hf");
    set("VLLM_API_KEY", api_key);
    // Read by the pod's watchdog, which shuts the pod down when nobody
    // uses it. Pod env, not an export in the boot script: no boot-path
    // change.
    set("RPCTL_IDLE_TTL_S", &seconds(cfg, "IDLE_TTL_MIN", 60.0)?);
    set("RPCTL_MAX_POD_S", &seconds(cfg, "MAX_POD_HOURS", 3600.0)?);
    let hf_token = at(cfg, "HF_TOKEN");
    if !hf_token.is_empty() {
        set("HF_TOKEN", hf_token);
        set("HUGGING_FACE_HUB_TOKEN", hf_token);
    }
    if let Some(key) = public_key.filter(|k| !k.is_empty()) {
        set("PUBLIC_KEY", key);
    }
    let watchdog_key = at(cfg, "RUNPOD_WATCHDOG_API_KEY");
    if !watchdog_key.is_empty() {
        set("RPCTL_WATCHDOG_KEY", watchdog_key);
    }
    Ok(env)
}

/// The create-pod request body (RunPod REST `POST /pods`), keys in rpctl's
/// order, around `start`.
pub fn body(
    cfg: &Config,
    start: PodStart,
    env: Map<String, Value>,
    volume_id: &str,
) -> Result<Value, BadValue> {
    let cuda: Vec<&str> = at(cfg, "CUDA_VERSIONS")
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    let mut body = json!({
        "name": at(cfg, "POD_NAME"),
        "imageName": start.image,
        "computeType": "GPU",
        "cloudType": at(cfg, "CLOUD_TYPE"),
        "gpuCount": whole(cfg, "GPU_COUNT")?,
        "gpuTypeIds": gpu_type_list(cfg),
        "gpuTypePriority": at(cfg, "GPU_TYPE_PRIORITY"),
        "dataCenterIds": [at(cfg, "DATA_CENTER_ID")],
        "networkVolumeId": volume_id,
        "volumeMountPath": "/workspace",
        "containerDiskInGb": whole(cfg, "CONTAINER_DISK_GB")?,
        "ports": ports(cfg),
        "env": env,
        "dockerEntrypoint": ["/bin/bash", "-c"],
        "dockerStartCmd": [start.script],
    });
    if !cuda.is_empty() {
        body["allowedCudaVersions"] = json!(cuda);
    }
    Ok(body)
}
