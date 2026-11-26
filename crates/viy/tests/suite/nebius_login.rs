use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use viy_testkit::pty::{Pty, wait_exit};
use viy_testkit::scratch::ScratchDir;

#[test]
fn provider_shows_the_pending_login_in_the_terminal_only() {
    check_login(&["provider", "nebius"]);
}

#[test]
fn up_shows_the_pending_login_in_the_terminal_only() {
    check_login(&["up"]);
}

fn check_login(args: &[&str]) {
    let scratch = ScratchDir::new("nebius-login").unwrap();
    let dir = scratch.path();
    let paths = viy_config::Paths::under(dir);
    viy_config::Rental::nebius("project-test", "test-region")
        .unwrap()
        .save(&paths)
        .unwrap();
    let cli = dir.join("nebius");
    fs::write(
        &cli,
        r#"#!/bin/sh
echo 'open the following link in your browser: https://example.invalid/sign-in' >&2
echo 'synthetic-token-on-stdout'
for i in $(/usr/bin/seq 1 100); do [ -f release ] && break; /bin/sleep 0.05; done
exit 7
"#,
    )
    .unwrap();
    fs::set_permissions(cli, fs::Permissions::from_mode(0o700)).unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_viy"));
        cmd.args(args)
            .env("PATH", dir)
            .env("HOME", dir)
            .env("XDG_CONFIG_HOME", &paths.config_home)
            .env("XDG_STATE_HOME", &paths.state_home)
            .current_dir(dir);
        cmd
    };
    let tty = Pty::open(20, 120).unwrap();
    let mut child = tty.spawn(&mut command()).unwrap();
    tty.wait_for("the browser link while the command is waiting", |s| {
        s.contents().contains("https://example.invalid/sign-in")
    });
    fs::write(dir.join("release"), "").unwrap();
    assert!(!wait_exit(&mut child).unwrap().success());
    tty.wait_for("the login failure", |s| {
        s.contents().contains("authentication failed")
    });
    tty.with_screen(|s| assert!(!s.screen().contents().contains("synthetic-token-on-stdout")));
    let out = command().output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success());
    assert!(text.contains("requires browser sign-in"), "{text}");
    assert!(!text.contains("Try again in a moment"), "{text}");
    assert!(!text.contains("example.invalid") && !text.contains("synthetic-token-on-stdout"));
}
