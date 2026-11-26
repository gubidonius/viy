use std::fs;
use std::process::Command;

use viy_testkit::scratch::ScratchDir;

#[test]
fn shell_completes_commands_options_and_live_profiles() {
    let home = ScratchDir::new("completion").unwrap();
    let profiles = home.path().join("viy/profiles");
    fs::create_dir_all(&profiles).unwrap();
    let run = |words: &[&str], complete: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_viy"));
        cmd.env("XDG_CONFIG_HOME", home.path())
            .env("XDG_STATE_HOME", home.path().join("state"))
            .current_dir(home.path());
        if complete {
            cmd.env("VIY_SHELL_COMPLETE", "bash")
                .env("_CLAP_COMPLETE_INDEX", words.len().to_string())
                .args(["--", "viy"]);
        }
        let result = cmd.args(words).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    assert_eq!(run(&["pro"], true).trim(), "provider");
    assert_eq!(run(&["provider", "neb"], true).trim(), "nebius");
    assert_eq!(run(&["provider", "run"], true).trim(), "runpod");
    assert_eq!(
        run(&["provider", "nebius", "--reg"], true).trim(),
        "--region"
    );
    assert_eq!(
        run(&["provider", "runpod"], false),
        run(&["provider", "use", "runpod"], false)
    );
    assert_eq!(run(&["provider", "use", "neb"], true).trim(), "nebius");
    assert_eq!(
        run(&["provider", "use", "nebius", "--reg"], true).trim(),
        "--region"
    );
    assert_eq!(run(&["boots", "sh"], true).trim(), "show");
    assert!(run(&["model", "fla"], true).is_empty());
    fs::write(profiles.join("flash-next.env"), "").unwrap();
    for action in [
        vec!["model", "fla"],
        vec!["model", "show", "fla"],
        vec!["model", "use", "fla"],
    ] {
        assert_eq!(run(&action, true).trim(), "flash-next");
    }
    assert!(!home.path().join("viy/config").exists());
    assert!(run(&["model", "flash-next"], false).contains("active model profile: flash-next"));
    assert!(run(&["provider", "list"], false).contains("* runpod"));
    assert_eq!(run(&["provider", "show"], false), run(&["provider"], false));
    assert!(run(&["provider", "show", "nebius"], false).contains("nebius · no cached machine"));
}
