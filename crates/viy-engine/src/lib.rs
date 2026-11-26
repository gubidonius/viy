//! The seam between viy-pod and the engines that serve the model on the pod
//! (kb d171, docs/strata-engine.md 4.1). Each engine lives in a crate of its
//! own (`viy-vllm`, `viy-strata`) that depends on this one, on viy-config
//! and on the provider seam's types (`viy-provider`, kb d182), never on the
//! other engine or on viy-pod; viy-pod picks one by the `ENGINE` key and
//! asks it, through [`Engine`], for the pod request it wants. Codex keeps its local servers the same way, in `codex-rs/ollama`
//! and `codex-rs/lmstudio`, chosen by provider id.

use serde_json::Value;
use viy_config::Config;
use viy_provider::PodRequest;

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

    /// Its name in what viy says: "vLLM is serving", "waiting for Strata".
    pub fn label(self) -> &'static str {
        match self {
            Self::Vllm => "vLLM",
            Self::Strata => "Strata",
        }
    }

    /// Its `ENGINE` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vllm => "vllm",
            Self::Strata => "strata",
        }
    }

    /// Whether the profile's server reads images (kb d191): vLLM's does
    /// (`--limit-mm-per-prompt`); Strata's needs a vision section in its run
    /// settings, which viy does not write yet (WP 9.49, kb f194).
    pub fn reads_images(self, cfg: &Config) -> bool {
        let _ = cfg;
        match self {
            Self::Vllm => true,
            Self::Strata => false,
        }
    }
}

/// What an engine provides to the pod code.
pub trait Engine: Sync {
    /// Its `ENGINE` value.
    fn name(&self) -> EngineName;

    /// What the engine asks a provider for (kb d182): image, start command,
    /// environment, ports and the placement, which failover then sets per
    /// attempt.
    fn pod_request(
        &self,
        cfg: &Config,
        api_key: &str,
        volume_id: &str,
        public_key: Option<&str>,
    ) -> Result<PodRequest, BadValue>;

    /// RunPod's create body for [`Engine::pod_request`], in REST v1's shapes
    /// (viy-pod's `wire` sends it as v2).
    fn pod_body(
        &self,
        cfg: &Config,
        api_key: &str,
        volume_id: &str,
        public_key: Option<&str>,
    ) -> Result<Value, BadValue> {
        let request = self.pod_request(cfg, api_key, volume_id, public_key)?;
        Ok(pod::runpod_body_for(cfg, &request))
    }
}

/// A config value a pod body cannot take: not a number where one is needed
/// (rp stops with Python's exception there; viy names the key, kb d047), or
/// not one of the values an engine accepts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{key} must be {kind}, not {value:?}")]
pub struct BadValue {
    pub key: &'static str,
    pub value: String,
    pub kind: &'static str,
}

pub mod pod;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
