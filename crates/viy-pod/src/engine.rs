//! Which engine serves the model on the pod (kb d171): the `ENGINE` key
//! resolved to the crate that builds what the pod runs. vLLM's boot script
//! and `startup.py` live in viy-vllm; viy-pod still reads vLLM's stage
//! table and local launch arguments from there directly, until WP
//! 9.24-9.25 put them behind the seam.

use viy_config::Config;
pub use viy_engine::BadValue;
pub use viy_engine::Engine;
pub use viy_engine::EngineName;

/// The engine `cfg`'s `ENGINE` names, vLLM when it names none; an unknown
/// name, or an engine viy does not run yet, is the message to stop with.
pub fn for_config(cfg: &Config) -> Result<&'static dyn Engine, String> {
    match EngineName::resolve(cfg)? {
        EngineName::Vllm => Ok(&viy_vllm::VLLM),
        EngineName::Strata => {
            Err("ENGINE=strata is not built yet (docs/strata-engine.md, WP 9.25)".into())
        }
    }
}

/// The config keys the engine `name` does not read: the other engine's own.
pub fn ignored_keys(name: EngineName) -> &'static [&'static str] {
    match name {
        EngineName::Vllm => viy_strata::OWN_KEYS,
        EngineName::Strata => viy_vllm::OWN_KEYS,
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
