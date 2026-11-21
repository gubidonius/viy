use pretty_assertions::assert_eq;
use serde_json::Value;
use viy_config::Layers;
use viy_config::Project;
use viy_testkit::config_tree::ConfigTree;
use viy_testkit::config_tree::case_env;
use viy_testkit::config_tree::case_files;
use viy_testkit::fixtures;

use super::*;

fn paths(tree: &ConfigTree) -> Paths {
    let config_home = tree.config_home();
    Paths::from_vars(tree.home(), move |name| {
        (name == "XDG_CONFIG_HOME").then(|| config_home.clone().into_os_string())
    })
}

/// `viy model ARGV` through viy's parser.
fn parse(argv: &[Value]) -> ModelArgs {
    let words = argv.iter().map(|a| a.as_str().unwrap().into());
    let cli = crate::cli::parse(std::iter::once("model".into()).chain(words)).unwrap();
    match cli.command {
        crate::cli::Command::Model(args) => args,
        other => panic!("{other:?}"),
    }
}

/// rpctl's text as viy says it: viy's command name and config directory.
fn as_viy(text: &str, tree: &ConfigTree) -> String {
    text.replace("rp model", "viy model")
        .replace("(rp down)", "(viy down)")
        .replace("&& rp up", "&& viy up")
        .replace("$T/config/rpctl/", "$T/config/viy/")
        .replace("$T", &tree.root().to_string_lossy())
}

/// A profile without the lines of viy's own profile keys, which rp's
/// profiles lack (kb d162); any other file as it is.
fn without_engine_keys(path: &str, text: &str) -> String {
    if !path.contains("profiles/") {
        return text.to_string();
    }
    let ours: Vec<String> = PROFILE_KEYS
        .iter()
        .filter(|key| viy_config::config::ENGINE_KEYS.contains(key))
        .map(|key| format!("{key}="))
        .collect();
    text.split_inclusive('\n')
        .filter(|line| !ours.iter().any(|key| line.starts_with(key.as_str())))
        .collect()
}

#[test]
fn viy_model_does_what_rp_model_does() {
    let fixture = fixtures::load("config/model").unwrap();
    assert_eq!(fixture.cases.len(), 22);
    for marker in [".viy", ".rpctl"] {
        for case in &fixture.cases {
            let name = case["name"].as_str().unwrap();
            let tree = ConfigTree::new(case, marker).unwrap();
            let paths = paths(&tree);
            let env = case_env(case);
            let project = Project::find(&tree.project()).unwrap();
            let cfg = Layers::new(&paths, &project)
                .load(|key| env.get(key).cloned())
                .unwrap();
            let mut out = Vec::new();
            let result = cmd_model(
                &cfg,
                &paths,
                &parse(case["argv"].as_array().unwrap()),
                &mut out,
            );
            let (exit, stderr) = match result {
                Ok(()) => (0, String::new()),
                Err(msg) => (1, format!("{msg}\n")),
            };
            assert_eq!(exit, case["exit"].as_i64().unwrap(), "{name}");
            let stdout = String::from_utf8(out).unwrap();
            assert_eq!(
                stdout,
                as_viy(case["stdout"].as_str().unwrap(), &tree),
                "{name}"
            );
            assert_eq!(
                stderr,
                as_viy(case["stderr"].as_str().unwrap(), &tree),
                "{name}"
            );
            // viy's own profile keys, which rp's profiles lack (kb d162).
            let files: std::collections::BTreeMap<String, String> = tree
                .files()
                .unwrap()
                .into_iter()
                .map(|(path, text)| {
                    let text = without_engine_keys(&path, &text);
                    (path, text)
                })
                .collect();
            assert_eq!(files, case_files(case), "{name} under {marker}");
        }
    }
}

/// `viy model show` marks the keys the profile's engine does not read
/// (kb d162): vLLM's own under ENGINE=strata, Strata's under vLLM.
#[test]
fn show_marks_the_keys_the_engine_ignores() {
    let dir = viy_testkit::scratch::ScratchDir::new("model-show").unwrap();
    let paths = Paths::under(dir.path());
    let show = |name: &str| {
        let mut out = Vec::new();
        let argv = [Value::from("show"), Value::from(name)];
        cmd_model(&Config::defaults(), &paths, &parse(&argv), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    profiles::write_profile(
        &paths,
        "flash",
        [
            ("ENGINE", "strata"),
            ("MODEL_FILES", "Q5_K_M/*"),
            ("VLLM_IMAGE", "vllm/vllm-openai:v0.33.0"),
            ("STRATA_PARALLEL", "2"),
        ],
    )
    .unwrap();
    let text = show("flash");
    assert!(text.contains("  ENGINE=strata\n"), "{text}");
    assert!(text.contains("  MODEL_FILES=Q5_K_M/*\n"), "{text}");
    assert!(text.contains("  STRATA_PARALLEL=2\n"), "{text}");
    assert!(
        text.contains("  VLLM_IMAGE=vllm/vllm-openai:v0.33.0  (ignored with ENGINE=strata)\n"),
        "{text}"
    );
    profiles::write_profile(
        &paths,
        "dense",
        [("STRATA_KV", "int8"), ("MAX_MODEL_LEN", "65536")],
    )
    .unwrap();
    let text = show("dense");
    assert!(
        text.contains("  STRATA_KV=int8  (ignored with ENGINE=vllm)\n"),
        "{text}"
    );
    assert!(text.contains("  MAX_MODEL_LEN=65536\n"), "{text}");
}

/// Explore's parallel default reads MAX_NUM_SEQS whatever the engine, so
/// show never calls it ignored.
#[test]
fn max_num_seqs_is_not_vllm_only() {
    assert!(!viy_pod::engine::ignored_keys(EngineName::Strata).contains(&"MAX_NUM_SEQS"));
}

/// Without an ENGINE line of its own, a profile's engine is the rest of the
/// config's; an ENGINE viy does not know is said, not dropped.
#[test]
fn show_takes_the_configs_engine_and_says_a_bad_one() {
    let dir = viy_testkit::scratch::ScratchDir::new("model-show-engine").unwrap();
    let paths = Paths::under(dir.path());
    let show = |cfg: &Config, name: &str| {
        let mut out = Vec::new();
        let argv = [Value::from("show"), Value::from(name)];
        cmd_model(cfg, &paths, &parse(&argv), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    profiles::write_profile(
        &paths,
        "flash",
        [("STRATA_PARALLEL", "2"), ("VLLM_IMAGE", "x")],
    )
    .unwrap();
    let mut strata = Config::defaults();
    strata.set("ENGINE", "strata");
    let text = show(&strata, "flash");
    assert!(text.contains("  STRATA_PARALLEL=2\n"), "{text}");
    assert!(
        text.contains("  VLLM_IMAGE=x  (ignored with ENGINE=strata)\n"),
        "{text}"
    );
    profiles::write_profile(&paths, "typo", [("ENGINE", "strta"), ("VLLM_IMAGE", "x")]).unwrap();
    let text = show(&Config::defaults(), "typo");
    assert!(
        text.contains("  ! unknown ENGINE 'strta'; supported: strata, vllm\n"),
        "{text}"
    );
}

/// `viy model add` from the default config marks nothing as ignored: it
/// writes ENGINE and an empty MODEL_FILES, not the pinned image.
#[test]
fn a_new_profile_shows_no_ignored_keys() {
    let dir = viy_testkit::scratch::ScratchDir::new("model-add-show").unwrap();
    let paths = Paths::under(dir.path());
    let cfg = Config::defaults();
    let run = |argv: &[&str]| {
        let mut out = Vec::new();
        let argv: Vec<Value> = argv.iter().map(|a| Value::from(*a)).collect();
        cmd_model(&cfg, &paths, &parse(&argv), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    run(&["add", "fresh"]);
    let text = run(&["show", "fresh"]);
    assert!(!text.contains("ignored"), "{text}");
    assert!(text.contains("  ENGINE=vllm\n"), "{text}");
    assert!(!text.contains("STRATA_IMAGE"), "{text}");
}
