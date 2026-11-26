//! The input history (ui-plan 5): what was sent from the input box, per
//! project, recalled with Up and Down.
//!
//! rp keeps readline's history in `.rpctl/history`, read at chat start and
//! written whole at exit, 2000 entries. viy keeps its own,
//! `.viy/history` ([`viy_config::Project::history_file`] gives the path),
//! as Codex keeps `history.jsonl`
//! (`codex-rs/message-history/src/lib.rs`): one JSON object per line,
//! `{"ts": <unix seconds>, "text": "..."}`, so a message with newlines is
//! one entry, and a shell command's has `"shell": "!"` or `"!!"` (kb d149); each entry appended as it is sent, in one write under an
//! exclusive lock, owner-only, so two chats in one project both keep
//! theirs. The newest 2000 are kept, rp's length. rp's file is copied in by
//! `viy migrate --finish` at cutover ([`import`], kb d043, d135), not read
//! by the chat.
//!
//! Recall follows Codex's `chat_composer_history.rs`: Up and Down browse
//! when the box is empty, or still holds the entry they last recalled with
//! the cursor at its start or end; otherwise they move the cursor.

use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;
use viy_protocol::ImageInput;

/// Entries kept: rp's `readline.set_history_length(2000)`.
pub const MAX_ENTRIES: usize = 2000;

/// The file is cut back to [`MAX_ENTRIES`] once it holds this many more,
/// so a cut is not a rewrite on every message.
const SLACK: usize = 200;

/// What a leading `!` made of a line (kb d149): a command run in the
/// shell whose output the model sees (`!`), or only the user (`!!`). An
/// entry keeps it in the file's `shell` field, so recalling one brings the
/// mode back, and a message that merely starts with `!` stays a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Shared,
    Private,
}

impl Shell {
    /// How it is typed.
    pub fn bang(self) -> &'static str {
        match self {
            Self::Shared => "!",
            Self::Private => "!!",
        }
    }

    fn from_bang(bang: &str) -> Option<Self> {
        match bang {
            "!" => Some(Self::Shared),
            "!!" => Some(Self::Private),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    text: String,
    shell: Option<Shell>,
    /// The images it was sent with, in this chat only: the file keeps the
    /// text (kb d191, as Codex's `history.jsonl`).
    images: Vec<ImageInput>,
}

#[derive(Debug, Default)]
pub struct History {
    /// Where entries are appended; none keeps them in memory only.
    path: Option<PathBuf>,
    /// Oldest first.
    entries: Vec<Entry>,
    /// The entry being shown, while browsing.
    at: Option<usize>,
    /// What the last recall put in the box.
    recalled: Option<String>,
}

impl History {
    /// The history in `path`: its newest [`MAX_ENTRIES`] entries; empty
    /// when it is missing or unreadable, and lines that are not entries are
    /// skipped.
    pub fn load(path: PathBuf) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut entries: Vec<Entry> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|v| {
                Some(Entry {
                    text: v.get("text")?.as_str()?.to_string(),
                    shell: v
                        .get("shell")
                        .and_then(Value::as_str)
                        .and_then(Shell::from_bang),
                    images: Vec::new(),
                })
            })
            .collect();
        let extra = entries.len().saturating_sub(MAX_ENTRIES);
        entries.drain(..extra);
        Self {
            path: Some(path),
            entries,
            at: None,
            recalled: None,
        }
    }

    /// The entries as typed, oldest first: a shell command with its `!`.
    pub fn entries(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| match e.shell {
                Some(shell) => format!("{}{}", shell.bang(), e.text),
                None => e.text.clone(),
            })
            .collect()
    }

    /// Adds a message that was sent, unless it repeats the newest entry,
    /// and appends it to the file. Browsing starts again from the newest.
    pub fn record(&mut self, text: &str) -> io::Result<()> {
        self.record_entry(text, None, Vec::new())
    }

    /// [`History::record`] for a message with images, which the entry
    /// keeps for this chat; the file gets the text.
    pub fn record_with_images(&mut self, text: &str, images: Vec<ImageInput>) -> io::Result<()> {
        self.record_entry(text, None, images)
    }

    /// [`History::record`] for a command run with `!` or `!!`.
    pub fn record_shell(&mut self, command: &str, shell: Shell) -> io::Result<()> {
        self.record_entry(command, Some(shell), Vec::new())
    }

    fn record_entry(
        &mut self,
        text: &str,
        shell: Option<Shell>,
        images: Vec<ImageInput>,
    ) -> io::Result<()> {
        self.at = None;
        self.recalled = None;
        let entry = Entry {
            text: text.to_string(),
            shell,
            images,
        };
        if text.trim().is_empty() || self.entries.last() == Some(&entry) {
            return Ok(());
        }
        self.entries.push(entry);
        let extra = self.entries.len().saturating_sub(MAX_ENTRIES);
        self.entries.drain(..extra);
        match &self.path {
            Some(path) => append(path, text, shell),
            None => Ok(()),
        }
    }

    /// The mode of the entry last recalled: `None` for a message, or with
    /// nothing recalled.
    pub fn shown_shell(&self) -> Option<Shell> {
        self.at
            .and_then(|i| self.entries.get(i))
            .and_then(|e| e.shell)
    }

    /// The images of the entry last recalled.
    pub fn shown_images(&self) -> Vec<ImageInput> {
        self.at
            .and_then(|i| self.entries.get(i))
            .map(|e| e.images.clone())
            .unwrap_or_default()
    }

    /// Whether Up and Down browse, for the box's `text` and `cursor`.
    pub fn browses(&self, text: &str, cursor: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        if text.is_empty() {
            return true;
        }
        (cursor == 0 || cursor == text.len()) && self.recalled.as_deref() == Some(text)
    }

    /// The entry before the one shown, or the newest; `None` at the oldest.
    pub fn older(&mut self) -> Option<String> {
        let next = match self.at {
            None => self.entries.len().checked_sub(1)?,
            Some(i) => i.checked_sub(1)?,
        };
        self.show(Some(next))
    }

    /// The entry after the one shown; past the newest, an empty box.
    pub fn newer(&mut self) -> Option<String> {
        let i = self.at?;
        if i + 1 < self.entries.len() {
            self.show(Some(i + 1))
        } else {
            self.show(None)
        }
    }

    fn show(&mut self, at: Option<usize>) -> Option<String> {
        self.at = at;
        let text = at
            .and_then(|i| self.entries.get(i))
            .map(|e| e.text.clone())
            .unwrap_or_default();
        self.recalled = Some(text.clone());
        Some(text)
    }
}

/// Appends one entry in one write, under an exclusive lock, and cuts the
/// file back to the newest [`MAX_ENTRIES`] when it has grown [`SLACK`]
/// past them.
fn append(path: &PathBuf, text: &str, shell: Option<Shell>) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut entry = json!({"ts": ts, "text": text});
    if let Some(shell) = shell {
        entry["shell"] = shell.bang().into();
    }
    let mut line = entry.to_string();
    line.push('\n');
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.lock()?;
    file.write_all(line.as_bytes())?;
    let mut all = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut all)?;
    let count = all.lines().count();
    if count > MAX_ENTRIES + SLACK {
        let keep: String = all
            .lines()
            .skip(count - MAX_ENTRIES)
            .flat_map(|l| [l, "\n"])
            .collect();
        file.set_len(0)?;
        file.write_all(keep.as_bytes())?;
    }
    Ok(())
}

/// The entries of rp's history file, as readline reads them back: GNU
/// readline's format, one entry per line, so an entry rp sent with
/// newlines in it comes back as one entry per line, as rp recalled it.
/// Blank lines and an entry repeating the one before are left out, as
/// [`History::record`] leaves them out; bytes that are not UTF-8 are
/// replaced.
pub fn readline_entries(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || entries.last().is_some_and(|e| e == line) {
            continue;
        }
        entries.push(line.to_string());
    }
    entries
}

/// Puts `older` (oldest first) before the entries already in the history
/// at `path`, each stamped `ts`, keeping the newest [`MAX_ENTRIES`]: rp's
/// history imported by `viy migrate --finish` (kb d135), which runs with
/// no chat open. The new file is written beside the old one and renamed
/// over it under the lock [`History::record`] takes, so a failed write
/// leaves the old history whole. The newest of `older` is left out when
/// it repeats the oldest entry there, as `record` leaves out a repeat.
/// Nothing is done when the history already opens with what this would
/// put there (the end of it, if the cut took the oldest): an import run
/// twice imports once. Returns how many of `older` were kept.
pub fn import(path: &Path, older: &[String], ts: u64) -> io::Result<usize> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.lock()?;
    let mut ours = String::new();
    file.read_to_string(&mut ours)?;
    let ours: Vec<&str> = ours.lines().collect();
    let first_text = ours
        .first()
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v.get("text")?.as_str().map(str::to_string));
    let mut lines: Vec<String> = older
        .iter()
        .map(|text| json!({"ts": ts, "text": text}).to_string())
        .collect();
    // In already, with or without the repeat at the seam.
    let opens_with = |block: &[String]| {
        (0..block.len()).any(|k| {
            let tail = &block[k..];
            ours.len() >= tail.len() && ours.iter().zip(tail).all(|(a, b)| a == b)
        })
    };
    if opens_with(&lines) || opens_with(&lines[..lines.len().saturating_sub(1)]) {
        return Ok(0);
    }
    if older
        .last()
        .is_some_and(|last| first_text.as_ref() == Some(last))
    {
        lines.pop();
    }
    let imported = lines.len();
    lines.extend(ours.iter().map(ToString::to_string));
    let extra = lines.len().saturating_sub(MAX_ENTRIES);
    lines.drain(..extra);
    let keep: String = lines.iter().flat_map(|l| [l.as_str(), "\n"]).collect();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.import.{}", std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut new| {
            new.write_all(keep.as_bytes())?;
            new.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    Ok(imported.saturating_sub(extra))
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
