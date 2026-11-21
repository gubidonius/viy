//! Strata's boot stages: the log text that ends each stage, read by viy-pod's
//! boot report as it reads vLLM's (viy-vllm::stages). The `RPCTL_PHASE` and
//! `=== boot` markers are the boot script's; the others are Strata's server's
//! own stdout, which the script tees into the platform log, not its engine
//! log: "[strata] experts loaded: 85.55 GiB at 2.12 GiB/s (75 s so far)" and
//! "ready: http://127.0.0.1:8080/v1 ..." in server.log of pod n2k01nm2n0ftmx
//! (2026-08-08, kb f161). The last stage, to the local endpoint, is viy-pod's.

/// (key, label, the log text that ends the stage); a missing marker folds
/// its time into the next stage.
pub const STAGES: [(&str, &str, &str); 4] = [
    ("pull", "GPU allocation + image pull", "=== boot "),
    (
        "prepare",
        "boot script (first boot: release, model, pack)",
        "RPCTL_PHASE starting Strata",
    ),
    ("experts", "experts to RAM", "[strata] experts loaded:"),
    ("gpu", "GPU expert cache + warmup", "ready: http://"),
];

/// What to check when a stage of [`STAGES`] is the biggest share of an
/// overrun; empty for a key that is not one.
pub fn hint(key: &str) -> &'static str {
    match key {
        "pull" => {
            "the host had to download the image; keep STRATA_IMAGE on RunPod's stock \
             runpod/pytorch image (kb d172)"
        }
        "prepare" => {
            "the first boot prepares the volume (release, model, pack); on a later boot \
             something in the boot script ran in the foreground"
        }
        "experts" => {
            "network-volume read of the experts; STRATA_PLE_IO=ram reads the n-gram table here too"
        }
        "gpu" => "filling the GPU's expert cache and the server's warmup",
        _ => "",
    }
}
