//! Strata, the engine of GGUF model profiles, one side of the engine seam
//! (kb d171; viy-engine; docs/strata-engine.md): what a pod runs to serve
//! with Strata. The pod is RunPod's stock image with Strata's pinned release
//! on the network volume (kb d172), which the first boot prepares (kb
//! d169). Nothing in this crate knows about vLLM; viy-pod reaches it
//! through [`STRATA`] and the seam.

use serde_json::Value;
use viy_config::Config;
use viy_engine::BadValue;
use viy_engine::Engine;
use viy_engine::EngineName;

pub mod boot;
pub mod release;
pub mod stages;

pub use boot::STRATA_POD_PY;
pub use boot::build_boot_script;
pub use boot::build_pod_body;

/// The config keys only Strata reads; vLLM ignores them.
pub const OWN_KEYS: &[&str] = viy_config::config::STRATA_KEYS;

/// The Strata engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct Strata;

/// The one instance viy-pod hands out.
pub static STRATA: Strata = Strata;

impl Engine for Strata {
    fn name(&self) -> EngineName {
        EngineName::Strata
    }

    fn pod_body(
        &self,
        cfg: &Config,
        api_key: &str,
        volume_id: &str,
        public_key: Option<&str>,
    ) -> Result<Value, BadValue> {
        build_pod_body(cfg, api_key, volume_id, public_key)
    }
}
