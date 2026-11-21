//! Which engine serves the model on the pod (kb d171): the `ENGINE` key
//! resolved to the crate that builds what the pod runs, viy-vllm or
//! viy-strata. The local backend runs vLLM alone and takes its arguments
//! from viy-vllm.

use viy_config::Config;
pub use viy_engine::BadValue;
pub use viy_engine::Engine;
pub use viy_engine::EngineName;

/// The engine `cfg`'s `ENGINE` names, vLLM when it names none; an unknown
/// name is the message to stop with.
pub fn for_config(cfg: &Config) -> Result<&'static dyn Engine, String> {
    match EngineName::resolve(cfg)? {
        EngineName::Vllm => Ok(&viy_vllm::VLLM),
        EngineName::Strata => Ok(&viy_strata::STRATA),
    }
}

/// The engine `cfg` names, vLLM for one viy does not know: for what viy
/// says and records, never for what the pod runs ([`for_config`] refuses
/// those before anything is made).
pub fn name(cfg: &Config) -> EngineName {
    EngineName::resolve(cfg).unwrap_or(EngineName::Vllm)
}

/// [`name`]'s label: "vLLM is serving", "Strata is serving".
pub fn label(cfg: &Config) -> &'static str {
    name(cfg).label()
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
