//! Commands the user types after `!` and `!!` (kb d149), as Codex's user
//! shell commands (core/src/tasks/user_shell.rs,
//! core/src/context/user_shell_command.rs): run in the workspace in the
//! user's login shell, outside the sandbox, with no approval, for at most
//! an hour. The output of a `!` command reaches the model with the user's
//! next message, in Codex's `<user_shell_command>` record; that of a `!!`
//! command only the user sees.
//!
//! What viy adds to Codex's: the output in the record is fenced as
//! untrusted text with its chat-control tokens defused and its secrets
//! masked, and it taints the chat (an unsandboxed command reaches the
//! network, as a networked run_shell does); its URLs never count as the
//! user's, and the reviewer, the title and `/rewind` never read it as
//! something the user wrote. And before either kind runs a file that
//! changed while the model was at work ([`WorkLog`]), the user is asked:
//! the model writes scripts that are rarely read.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;

/// Codex's `USER_SHELL_TIMEOUT_MS`.
pub const TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How long output is read once the command has ended: Codex's
/// `IO_DRAIN_TIMEOUT_MS`, so a job left in the background holding the
/// pipes does not hold the chat.
pub const IO_DRAIN: Duration = Duration::from_secs(2);

/// How a record opens and closes (Codex's `UserShellCommand` markers).
pub const RECORD_OPEN: &str = "<user_shell_command>";
const RECORD_CLOSE: &str = "</user_shell_command>";

/// What the transcript shows of a long output: the user sees much more
/// than the model, but not without end.
pub const SHOWN_CHARS: usize = 200_000;

/// Whether a user message's text part is a `!` command's record.
pub fn is_record(text: &str) -> bool {
    text.starts_with(RECORD_OPEN)
}

/// The command a record holds.
pub fn record_command(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(RECORD_OPEN)?;
    let rest = rest.strip_prefix("\n<command>\n")?;
    let end = rest.find("\n</command>\n")?;
    Some(&rest[..end])
}

/// The text parts of a user message's content that the user wrote: a
/// string, or its text parts without the `!` records and the skills a
/// `$name` attached (kb d184), neither of which is the user's request.
pub fn typed_parts(content: &Value) -> Vec<&str> {
    match content {
        Value::String(s) => vec![s.as_str()],
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .filter(|t| !is_record(t) && !crate::skills::is_part(t))
            .collect(),
        _ => Vec::new(),
    }
}

/// The `!` records in a user message's content.
pub fn records(content: &Value) -> Vec<&str> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .filter(|t| is_record(t))
        .collect()
}

/// How a command ended, as the record and the card tell it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The exit code, or minus the signal; `None` when it timed out.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// stdout, then stderr, as one output.
    pub fn output(&self) -> String {
        let out = self.stdout.trim_end_matches('\n');
        let err = self.stderr.trim_end_matches('\n');
        match (out.is_empty(), err.is_empty()) {
            (_, true) => out.to_string(),
            (true, false) => err.to_string(),
            (false, false) => format!("{out}\n{err}"),
        }
    }

    /// The card's few words: `exit 0`, `timed out`.
    pub fn detail(&self) -> String {
        if self.timed_out {
            "timed out".into()
        } else {
            format!("exit {}", self.exit_code.unwrap_or(-1))
        }
    }
}

/// Codex's record of a user shell command, with `output` (already cut to
/// size, defused and masked) fenced under `nonce`. A timed-out command's
/// exit code is Codex's -1.
pub fn record(command: &str, ran: &Ran, output: &str, nonce: &str) -> String {
    let fenced = viy_tools::untrusted::wrap(output, nonce);
    format!(
        "{RECORD_OPEN}\n<command>\n{command}\n</command>\n<result>\nExit code: {}\nDuration: {:.4} \
         seconds\nOutput:\n{fenced}\n</result>\n{RECORD_CLOSE}",
        ran.exit_code.filter(|_| !ran.timed_out).unwrap_or(-1),
        ran.duration.as_secs_f64(),
    )
}

/// The shell a `!` command runs in: the user's login shell, as Codex runs
/// it (`derive_exec_args` with a login shell), or bash.
pub fn argv(command: &str, shell: Option<&std::ffi::OsStr>) -> Vec<std::ffi::OsString> {
    let shell = shell
        .filter(|s| !s.is_empty() && Path::new(s).is_absolute())
        .map_or_else(|| "/bin/bash".into(), std::ffi::OsStr::to_os_string);
    vec![shell, "-lc".into(), command.into()]
}

// ------------------------------------------------------- the model at work

/// The log's file in viy's state folder.
const WORK_LOG: &str = "model-work.jsonl";
/// How long a window counts before its turn says it ended: a chat that
/// crashed mid-turn leaves its window this long.
const OPEN_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// The log is cut back to its newest windows past this size.
const MAX_BYTES: u64 = 4 << 20;
const KEEP_WINDOWS: usize = 20_000;
/// A file time this close to a window counts as in it: the clock that
/// stamps files ticks coarsely.
const SLACK: f64 = 1.0;

/// When the model was at work, every viy chat's turns alike, in viy's
/// state folder, which the sandbox never sees. A turn's window is written
/// before it starts, open for [`OPEN_FOR`], and closed when it ends, so a
/// chat that crashes still leaves it. A file the model wrote, in a turn's
/// tool calls or with run_shell, was created, written or changed in such a
/// window, and a sandboxed process cannot set a file's change time.
#[derive(Debug, Clone)]
pub struct WorkLog {
    path: PathBuf,
}

/// A turn's window, open until it is dropped.
#[derive(Debug)]
pub struct Window {
    log: WorkLog,
    id: String,
    start: f64,
}

impl Drop for Window {
    /// The turn ended, or was stopped: its window closes now. A failure
    /// leaves the window open for [`OPEN_FOR`], the safe side.
    fn drop(&mut self) {
        let _ = self.log.append(&self.id, self.start, now());
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn secs(t: SystemTime) -> Option<f64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

impl WorkLog {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(WORK_LOG),
        }
    }

    /// A turn begins: its window, written open.
    pub fn begin(&self) -> io::Result<Window> {
        let start = now();
        let mut id = [0u8; 6];
        rand::TryRngCore::try_fill_bytes(&mut rand::rngs::OsRng, &mut id)
            .map_err(io::Error::other)?;
        let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
        self.append(&id, start, start + OPEN_FOR.as_secs_f64())?;
        Ok(Window {
            log: self.clone(),
            id,
            start,
        })
    }

    /// One line in one write under the file's lock; past [`MAX_BYTES`] the
    /// newest [`KEEP_WINDOWS`] are kept.
    fn append(&self, id: &str, start: f64, until: f64) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut line = json!({"id": id, "start": start, "until": until}).to_string();
        line.push('\n');
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        file.lock()?;
        file.write_all(line.as_bytes())?;
        if file.metadata()?.len() > MAX_BYTES {
            let mut all = String::new();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_string(&mut all)?;
            let mut windows = parse(&all);
            windows.drain(..windows.len().saturating_sub(KEEP_WINDOWS));
            let kept: String = windows
                .iter()
                .map(|w| format!("{}\n", json!({"id": w.0, "start": w.1, "until": w.2})))
                .collect();
            file.set_len(0)?;
            file.write_all(kept.as_bytes())?;
        }
        Ok(())
    }

    /// Every window, `(start, until)`, the last line of each turn deciding;
    /// none when no turn has been logged. Read under the lock a cut-back
    /// holds, so it never sees the file half rewritten. An error means
    /// nothing can be told from it.
    pub fn windows(&self) -> io::Result<Vec<(f64, f64)>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        file.lock_shared()?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(parse(&text).into_iter().map(|w| (w.1, w.2)).collect())
    }
}

/// `(id, start, until)` per turn, in the order the turns began.
fn parse(text: &str) -> Vec<(String, f64, f64)> {
    let mut order: Vec<String> = Vec::new();
    let mut last: HashMap<String, (f64, f64)> = HashMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(id), Some(start), Some(until)) =
            (v["id"].as_str(), v["start"].as_f64(), v["until"].as_f64())
        else {
            continue;
        };
        if last.insert(id.to_string(), (start, until)).is_none() {
            order.push(id.to_string());
        }
    }
    order
        .into_iter()
        .filter_map(|id| last.get(&id).map(|&(s, u)| (id, s, u)))
        .collect()
}

/// The files `command` would run as code, from `cwd`: what
/// [`viy_tools::approve::run_targets`] finds, or every word where it cannot
/// follow the command. A word with a `/` is a path from `cwd` or from a
/// folder the command `cd`s into (`~` is `home`); one without is looked
/// for there and in each of `paths` (viy's `PATH`, the login shell's). Only
/// files that exist.
pub fn run_files(
    command: &str,
    cwd: &Path,
    home: &Path,
    paths: &[std::ffi::OsString],
) -> Vec<PathBuf> {
    let words = viy_tools::approve::run_targets(command).unwrap_or_else(|| {
        shlex::split(command).unwrap_or_else(|| {
            command
                .split_whitespace()
                .map(|w| w.trim_matches(['\'', '"']).to_string())
                .collect()
        })
    });
    let tilde = |w: &str| match w.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if w == "~" => home.to_path_buf(),
        None => PathBuf::from(w),
    };
    let mut dirs = vec![cwd.to_path_buf()];
    for d in viy_tools::approve::cd_targets(command) {
        let d = cwd.join(tilde(&d));
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        // `ws/./fix.sh` is `ws/fix.sh`; a link stays itself, for its times.
        let p: PathBuf = p.components().collect();
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    };
    for word in words.iter().filter(|w| !w.is_empty() && !w.contains('\0')) {
        let word = tilde(word);
        for dir in &dirs {
            add(dir.join(&word));
        }
        if word.components().count() == 1 && word.is_relative() {
            for path in paths {
                if let Some(found) = std::env::split_paths(path)
                    .map(|dir| cwd.join(dir).join(&word))
                    .find(|p| p.is_file())
                {
                    add(found);
                }
            }
        }
    }
    out
}

/// Of `files`, those created, written or changed in one of `windows`, and
/// when: the file itself and, for a link, the link too.
pub fn written_in(files: &[PathBuf], windows: &[(f64, f64)]) -> Vec<(PathBuf, f64)> {
    let inside = |t: f64| {
        windows
            .iter()
            .any(|&(s, u)| t >= s - SLACK && t <= u + SLACK)
    };
    let mut out = Vec::new();
    for f in files {
        let metas = [std::fs::symlink_metadata(f).ok(), std::fs::metadata(f).ok()];
        let hit = metas.iter().flatten().find_map(|m| {
            let changed =
                m.ctime() as f64 + f64::from(u32::try_from(m.ctime_nsec()).unwrap_or(0)) / 1e9;
            [
                Some(changed),
                m.modified().ok().and_then(secs),
                m.created().ok().and_then(secs),
            ]
            .into_iter()
            .flatten()
            .find(|&t| inside(t))
        });
        if let Some(t) = hit {
            out.push((f.clone(), t));
        }
    }
    out
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
