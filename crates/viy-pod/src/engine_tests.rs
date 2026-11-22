use pretty_assertions::assert_eq;
use viy_config::Config;

use super::*;

fn with_engine(value: &str) -> Config {
    let mut cfg = Config::defaults();
    cfg.set("ENGINE", value);
    cfg
}

#[test]
fn the_default_config_is_served_by_vllm() {
    let engine = for_config(&Config::defaults()).unwrap();
    assert_eq!(engine.name(), EngineName::Vllm);
}

#[test]
fn vllms_body_through_the_seam_is_viy_vllms_own() {
    let cfg = Config::defaults();
    let engine = for_config(&cfg).unwrap();
    assert_eq!(
        engine
            .pod_body(
                &cfg,
                "sk-SYNTHETIC",
                "vol1",
                Some("ssh-ed25519 AAAA synthetic")
            )
            .unwrap(),
        viy_vllm::build_pod_body(
            &cfg,
            "sk-SYNTHETIC",
            "vol1",
            Some("ssh-ed25519 AAAA synthetic")
        )
        .unwrap()
    );
}

#[test]
fn each_engine_ignores_the_others_keys() {
    assert!(ignored_keys(EngineName::Vllm).contains(&"STRATA_PARALLEL"));
    assert!(!ignored_keys(EngineName::Vllm).contains(&"VLLM_IMAGE"));
    assert!(ignored_keys(EngineName::Strata).contains(&"VLLM_IMAGE"));
    assert!(!ignored_keys(EngineName::Strata).contains(&"MODEL_FILES"));
    // what both read is in neither list
    for key in [
        "MODEL_REPO",
        "MAX_MODEL_LEN",
        "POD_NAME",
        "CUDA_VERSIONS",
        "ENGINE",
    ] {
        assert!(!ignored_keys(EngineName::Vllm).contains(&key), "{key}");
        assert!(!ignored_keys(EngineName::Strata).contains(&key), "{key}");
    }
}

#[test]
fn strata_is_served_by_viy_strata() {
    let engine = for_config(&with_engine("strata")).unwrap();
    assert_eq!(engine.name(), EngineName::Strata);
    assert_eq!(label(&with_engine("strata")), "Strata");
    assert_eq!(label(&Config::defaults()), "vLLM");
    assert_eq!(name(&with_engine("tgi")), EngineName::Vllm);
}

#[test]
fn an_unknown_engine_is_refused() {
    assert_eq!(
        for_config(&with_engine("tgi")).err(),
        Some("unknown ENGINE 'tgi'; supported: strata, vllm".to_string())
    );
}

/// The sshd block of Strata's boot script is vLLM's, the pod's only route
/// in under TUNNEL=1 (kb d003): the same commands, host key paths and
/// options, only the log file named for Strata. Changing one without the
/// other fails here.
#[test]
fn stratas_sshd_block_is_vllms() {
    fn sshd_block(script: &str) -> &str {
        let start = script
            .find("# sshd. In tunnel mode")
            .expect("an sshd block");
        let end = start + script[start..].find("\nfi\n").expect("its end") + "\nfi\n".len();
        &script[start..end]
    }
    let cfg = Config::defaults();
    let vllm = viy_vllm::build_boot_script(&cfg, "sk-SYNTHETIC");
    let strata = viy_strata::build_boot_script(&cfg).unwrap();
    assert_eq!(
        sshd_block(&strata),
        sshd_block(&vllm).replace("/workspace/vllm.log", "/workspace/strata.log")
    );
    assert!(sshd_block(&vllm).contains(viy_vllm::RUNTIME_HOSTKEY));
}

#[test]
fn stratas_body_through_its_crate() {
    let mut cfg = Config::defaults();
    cfg.set("ENGINE", "strata");
    let body = viy_strata::STRATA.pod_body(&cfg, "k", "v", None).unwrap();
    assert_eq!(body["minRAMPerGPU"], 150);
    assert_eq!(viy_strata::STRATA.name(), EngineName::Strata);
}

/// Each engine's request is the neutral one (kb d182): the placement and
/// the volume where the seam names them, and RunPod's body for it is the
/// body the engine gives.
#[test]
fn each_engine_asks_through_a_pod_request() {
    for (name, ram) in [("vllm", None), ("strata", Some(250))] {
        let mut cfg = Config::defaults();
        cfg.set("ENGINE", name);
        if ram.is_some() {
            cfg.set("MIN_RAM_GB", "250");
        }
        let engine = for_config(&cfg).unwrap();
        let request = engine.pod_request(&cfg, "k", "vol9", None).unwrap();
        assert_eq!(request.volume.as_deref(), Some("vol9"), "{name}");
        assert_eq!(request.region, cfg.get("DATA_CENTER_ID").unwrap(), "{name}");
        assert_eq!(request.min_ram_gb, ram, "{name}");
        assert!(
            request.env.iter().any(|(k, _)| k == "VLLM_API_KEY"),
            "{name}"
        );
        let body = engine.pod_body(&cfg, "k", "vol9", None).unwrap();
        assert_eq!(
            body,
            viy_engine::pod::runpod_body(
                cfg.get("CLOUD_TYPE").unwrap(),
                cfg.get("GPU_TYPE_PRIORITY").unwrap(),
                &request
            ),
            "{name}"
        );
    }
}
