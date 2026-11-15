//! What a command the user typed after `!` runs as code (kb d149): each
//! simple command's program and the scripts an interpreter or `source` is
//! given, through the argv view's unwrapping (`sudo`, `env`, `timeout`,
//! `sh -c '...'`, `eval`, `find -exec`), so the chat can ask before a file
//! the model wrote runs outside the sandbox.

use super::argv::commands;
use super::pipe::basename;
use super::pipe::split_shell;

/// Programs whose operands name the program they run: shells, script
/// interpreters, `source` and `.`. Pythons and Lua are matched by prefix
/// (`python3.12`, `luajit`).
const INTERPRETERS: [&str; 29] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "mksh",
    "fish",
    "csh",
    "tcsh",
    "busybox",
    "source",
    ".",
    "node",
    "nodejs",
    "deno",
    "bun",
    "tsx",
    "ts-node",
    "perl",
    "ruby",
    "php",
    "Rscript",
    "tclsh",
    "wish",
    "pwsh",
    "osascript",
    "awk",
    "gawk",
    "make",
];

fn interprets(prog: &str) -> bool {
    INTERPRETERS.contains(&prog)
        || prog.starts_with("python")
        || prog.starts_with("pypy")
        || prog.starts_with("lua")
}

/// The words `cmd` runs as code: each simple command's program, and every
/// operand of an interpreter that is not an option. `None` where the argv
/// view cannot follow the command (`$(...)`, backticks, process
/// substitution, an unbalanced quote): the caller then takes every word.
pub fn run_targets(cmd: &str) -> Option<Vec<String>> {
    if cmd.contains("<(") || cmd.contains(">(") {
        return None;
    }
    for (_, part) in split_shell(cmd)? {
        shlex::split(&part)?;
    }
    let mut out: Vec<String> = Vec::new();
    for argv in commands(cmd) {
        let Some(first) = argv.first() else {
            continue;
        };
        out.push(first.clone());
        // `.` has no file name, so its basename is empty: named apart.
        if interprets(basename(first)) || first == "." {
            out.extend(
                argv[1..]
                    .iter()
                    .filter(|a| !a.is_empty() && !a.starts_with('-'))
                    .cloned(),
            );
        }
    }
    out.dedup();
    Some(out)
}

/// The folders `cmd` changes into (`cd`, `pushd`), wherever it does: a
/// relative word after one may name a file there.
pub fn cd_targets(cmd: &str) -> Vec<String> {
    commands(cmd)
        .into_iter()
        .filter(|argv| matches!(argv.first().map(|w| basename(w)), Some("cd" | "pushd")))
        .filter_map(|argv| argv[1..].iter().find(|a| !a.starts_with('-')).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn targets(cmd: &str) -> Option<Vec<String>> {
        run_targets(cmd)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn some(words: &[&str]) -> Option<Vec<String>> {
        Some(words.iter().map(|w| (*w).to_string()).collect())
    }

    #[test]
    fn the_program_of_each_simple_command() {
        assert_eq!(targets("./fix.sh --apply"), some(&["./fix.sh"]));
        assert_eq!(
            targets("ls && tools/run | grep x; ./b"),
            some(&["ls", "tools/run", "grep", "./b"])
        );
        assert_eq!(targets("FOO=1 ./fix.sh"), some(&["./fix.sh"]));
    }

    #[test]
    fn an_interpreters_scripts() {
        assert_eq!(targets("bash fix.sh"), some(&["bash", "fix.sh"]));
        assert_eq!(
            targets("python3 -u tools/migrate.py --dry-run data.csv"),
            some(&["python3", "tools/migrate.py", "data.csv"])
        );
        assert_eq!(targets(". ./env.sh"), some(&[".", "./env.sh"]));
        assert_eq!(targets("source env.sh"), some(&["source", "env.sh"]));
        assert_eq!(targets("make -f build.mk"), some(&["make", "build.mk"]));
    }

    #[test]
    fn wrappers_and_inline_scripts_are_unwrapped() {
        assert_eq!(
            targets("sudo -u root env X=1 ./fix.sh"),
            some(&["./fix.sh"])
        );
        assert_eq!(targets("timeout 5 ./fix.sh"), some(&["./fix.sh"]));
        // sh -c's script is followed, and the sh itself is kept.
        assert_eq!(
            targets("sh -c './a.sh && ./b.sh'"),
            some(&["./a.sh", "./b.sh", "sh", "./a.sh && ./b.sh"])
        );
        assert_eq!(
            targets("find . -name '*.sh' -exec ./lint {} ;"),
            some(&["./lint", "find"])
        );
    }

    #[test]
    fn the_folders_it_changes_into() {
        assert_eq!(cd_targets("cd tools && ./run.sh"), ["tools"]);
        assert_eq!(cd_targets("pushd -q a; sh -c 'cd b; ./x'"), ["a", "b"]);
        assert!(cd_targets("./run.sh").is_empty());
    }

    #[test]
    fn what_the_argv_view_cannot_follow_is_none() {
        assert_eq!(targets("bash $(cat which)"), None);
        assert_eq!(targets("`cat which`"), None);
        assert_eq!(targets("bash <(cat fix.sh)"), None);
        assert_eq!(targets("echo 'unclosed"), None);
    }
}
