//! The seam between viy-pod and the engines that serve the model on the pod
//! (kb d171, docs/strata-engine.md 4.1). Each engine lives in a crate of its
//! own (`viy-vllm`, `viy-strata`) that depends on this one and on
//! viy-config only, never on the other engine or on viy-pod; viy-pod picks
//! one by the `ENGINE` key and asks it, through [`Engine`], for what the pod
//! runs. Codex keeps its local servers the same way, in `codex-rs/ollama`
//! and `codex-rs/lmstudio`, chosen by provider id.

use serde_json::Value;
use viy_config::Config;

/// The engines a profile can name in `ENGINE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineName {
    /// vLLM, the default: the Qwen3.8-27B profile.
    Vllm,
    /// Strata, for GGUF models (docs/strata-engine.md).
    Strata,
}

impl EngineName {
    /// The one `ENGINE` names, in any case; empty or unset means vLLM.
    pub fn resolve(cfg: &Config) -> Result<Self, String> {
        let written = cfg.get("ENGINE").unwrap_or("").trim();
        match written.to_lowercase().as_str() {
            "" | "vllm" => Ok(Self::Vllm),
            "strata" => Ok(Self::Strata),
            _ => Err(format!(
                "unknown ENGINE '{written}'; supported: strata, vllm"
            )),
        }
    }

    /// Its `ENGINE` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vllm => "vllm",
            Self::Strata => "strata",
        }
    }
}

/// What an engine provides to the pod code.
pub trait Engine: Sync {
    /// Its `ENGINE` value.
    fn name(&self) -> EngineName;

    /// The create-pod request body in RunPod REST v1's shapes (viy-pod's
    /// `wire` sends it as v2): image, start command, environment, ports and
    /// the placement fields, which failover then sets per attempt
    /// (`dataCenterIds`, `networkVolumeId`, `gpuTypeIds`).
    fn pod_body(
        &self,
        cfg: &Config,
        api_key: &str,
        volume_id: &str,
        public_key: Option<&str>,
    ) -> Result<Value, BadNumber>;
}

/// A config value a pod body needs as a number but that is not one. rp
/// stops with Python's exception here; viy names the key (kb d047).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{key} must be {kind}, not {value:?}")]
pub struct BadNumber {
    pub key: &'static str,
    pub value: String,
    pub kind: &'static str,
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
