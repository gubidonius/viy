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
fn strata_is_refused_until_it_is_built() {
    assert_eq!(
        for_config(&with_engine("strata")).err(),
        Some("ENGINE=strata is not built yet (docs/strata-engine.md, WP 9.24)".to_string())
    );
}

#[test]
fn an_unknown_engine_is_refused() {
    assert_eq!(
        for_config(&with_engine("tgi")).err(),
        Some("unknown ENGINE 'tgi'; supported: strata, vllm".to_string())
    );
}
