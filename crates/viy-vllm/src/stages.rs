//! vLLM's boot stages: the log text that ends each stage of a boot, read by
//! viy-pod's boot report, and what to check when one is the largest share
//! of an overrun. The `RPCTL_PHASE` markers are the ones [`crate::boot`]'s
//! script prints; the others are vLLM's own log lines. The last stage, to
//! the local endpoint, is viy-pod's.

/// (key, label, the log text that ends the stage). Each stage runs from the
/// end of the previous one; a missing marker folds its time into the next
/// stage, so a vLLM that rewords a line shows as one stage absorbing
/// another.
pub const STAGES: [(&str, &str, &str); 9] = [
    ("pull", "GPU allocation + image pull", "=== boot "),
    (
        "prepare",
        "boot script (caches, sshd)",
        "RPCTL_PHASE starting vLLM",
    ),
    ("imports", "vLLM CLI imports", "non-default args"),
    ("api_init", "config, processor, engine spawn", "world_size="),
    (
        "engine_init",
        "engine: CUDA + processor init",
        "Starting to load model",
    ),
    ("weights", "weights to GPU", "Model loading took"),
    (
        "compile",
        "encoder profiling + compile",
        "torch.compile took",
    ),
    ("engine", "KV cache + CUDA graphs", "init engine (profile"),
    ("api", "API server warmup", "HTTP server started"),
];

/// What to check when a stage of [`STAGES`] is the biggest share of an
/// overrun; empty for a key that is not one.
pub fn hint(key: &str) -> &'static str {
    match key {
        "pull" => {
            "the host had to download the image; keep VLLM_IMAGE on the stock Docker Hub \
             vllm/vllm-openai image"
        }
        "prepare" => "something in the boot script ran in the foreground before vLLM",
        "imports" => "vLLM's Python imports; check for a new image or new vLLM arguments",
        "api_init" => "API-server config/processor load; check for new vLLM arguments",
        "engine_init" => "engine CUDA and processor init",
        "weights" => "network-volume read; check the 'weights prewarmed' line in viy logs",
        "compile" => "compile cache missed; the first boot on a new GPU type compiles once",
        "engine" => "KV-cache profiling or CUDA graph capture (warmup after a compile miss)",
        "api" => "API-server warmup; check MM_WARMUP is 0",
        _ => "",
    }
}

#[cfg(test)]
#[path = "stages_tests.rs"]
mod tests;
