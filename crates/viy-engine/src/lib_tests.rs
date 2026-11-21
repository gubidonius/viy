use pretty_assertions::assert_eq;
use viy_config::Config;

use super::*;

fn with_engine(value: Option<&str>) -> Config {
    let mut cfg = Config::defaults();
    if let Some(value) = value {
        cfg.set("ENGINE", value);
    }
    cfg
}

#[test]
fn an_unset_or_empty_engine_is_vllm() {
    assert_eq!(
        EngineName::resolve(&with_engine(None)),
        Ok(EngineName::Vllm)
    );
    assert_eq!(
        EngineName::resolve(&with_engine(Some(""))),
        Ok(EngineName::Vllm)
    );
    assert_eq!(
        EngineName::resolve(&with_engine(Some(" "))),
        Ok(EngineName::Vllm)
    );
}

#[test]
fn engines_are_named_in_any_case() {
    for (value, name) in [
        ("vllm", EngineName::Vllm),
        ("vLLM", EngineName::Vllm),
        ("strata", EngineName::Strata),
        ("Strata", EngineName::Strata),
    ] {
        assert_eq!(EngineName::resolve(&with_engine(Some(value))), Ok(name));
        assert_eq!(name.as_str(), value.to_lowercase());
    }
}

#[test]
fn an_unknown_engine_names_the_supported_ones() {
    assert_eq!(
        EngineName::resolve(&with_engine(Some("SGLang"))),
        Err("unknown ENGINE 'SGLang'; supported: strata, vllm".to_string())
    );
}

#[test]
fn a_bad_number_names_its_key() {
    let error = BadValue {
        key: "GPU_COUNT",
        value: "two".into(),
        kind: "a whole number",
    };
    assert_eq!(
        error.to_string(),
        "GPU_COUNT must be a whole number, not \"two\""
    );
}
