//! kb d149: the one place a `!` command is made is the terminal UI's
//! submit of a `!` a person typed. No other code (`viy exec`, the plain
//! chat on stdin, `viy dev drive`, the engine, the chat, a tool) builds
//! [`viy_protocol::Op::Shell`], so nothing the model writes, and no script,
//! can run one. A new producer fails this test and needs a decision.

use std::path::Path;
use std::path::PathBuf;

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && !path.to_string_lossy().ends_with("_tests.rs")
        {
            out.push(path);
        }
    }
}

/// Where `text` builds an `Op::Shell`: the variant with a field given a
/// value (`model_sees: ...`), not a pattern that only names it.
fn builds(text: &str) -> usize {
    text.match_indices("Op::Shell")
        .filter(|(at, _)| {
            let rest = &text[at + "Op::Shell".len()..];
            let rest = rest.trim_start();
            rest.starts_with('{')
                && rest[1..]
                    .split('}')
                    .next()
                    .is_some_and(|fields| fields.contains("model_sees:"))
        })
        .count()
}

#[test]
fn only_the_terminal_ui_makes_a_shell_op() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for krate in std::fs::read_dir(&crates).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    assert!(files.len() > 100, "found only {} sources", files.len());
    let made: Vec<String> = files
        .iter()
        .filter(|f| {
            // Test modules inside a source file are cut off where they start.
            let text = std::fs::read_to_string(f).unwrap();
            let text = text.split("#[cfg(test)]\nmod tests {").next().unwrap();
            builds(text) > 0
        })
        .map(|f| {
            f.strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(made, ["viy-term/src/app.rs"]);
    let app = std::fs::read_to_string(crates.join("viy-term/src/app.rs")).unwrap();
    assert_eq!(builds(&app), 1);
}
