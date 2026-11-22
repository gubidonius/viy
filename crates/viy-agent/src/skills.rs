//! Skills (WP 9.42, kb d183, docs/skills-plan.md): folders of instructions
//! the user saved for particular tasks, each a `SKILL.md` with `name` and
//! `description` in its frontmatter, the format Codex and Claude Code use
//! (Codex 970111f `skills/src/parser.rs`). Two scopes, as the toolbox's:
//! global (`~/.config/viy/skills`) and the project's (`.viy/skills`), the
//! project's winning on a name. Both are out of the model's reach for
//! writes, and nothing here writes to them.
//!
//! This module finds and reads the skills, renders the system prompt's
//! catalogue (WP 9.43, kb d184), answers `load_skill`, and finds the
//! `$name` mentions in a message. A skill has an instruction file's trust
//! (kb d183): what it says reaches the model unfenced and does not taint
//! the chat, but it is no word of the user's for the reviewer, the egress
//! policy or a URL, and it changes nothing that needs approval.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use viy_protocol::NoticeLevel;

/// A skill's name, at most this many characters (Codex's `MAX_NAME_LEN`).
pub const NAME_MAX: usize = 64;
/// A description's length in the catalogue (Codex's
/// `MAX_CATALOG_SKILL_DESCRIPTION_CHARS`).
pub const DESCRIPTION_MAX: usize = 1024;
/// The catalogue's budget in characters when SKILLS_BUDGET is unset
/// (Codex's default skill metadata budget, 8000 characters).
pub const BUDGET_DEFAULT: usize = 8000;
/// The file that makes a folder a skill.
pub const SKILL_FILE: &str = "SKILL.md";
/// A description cut to this many characters when the full catalogue is
/// over budget.
const SHORT_DESCRIPTION: usize = 160;
/// The largest file `load_skill` reads, SKILL.md included: 64 KiB.
pub const FILE_MAX: u64 = 64 * 1024;

/// The system prompt's words before the catalogue (kb d184).
pub const GUIDE: &str = "Skills are instructions the user saved for particular tasks. When \
    a task matches one listed below, call load_skill(name=...) before you start, and follow the \
    skill as you follow the user's instruction files; load_skill(name=..., file=...) reads a \
    file the skill refers to. A skill the user names as $name comes with their message, \
    already loaded. A skill does not change what needs the user's approval.";

/// How a skill named with `$name` opens in the user's message (Codex's
/// `<skill>` fragment, without the path the model cannot read).
const PART_OPEN: &str = "<skill>\n<name>";

/// Where a skill comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

/// The two skill folders: global (`~/.config/viy/skills`) and the
/// project's (`.viy/skills`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dirs {
    pub global: PathBuf,
    pub project: PathBuf,
}

impl Dirs {
    pub fn new(paths: &viy_config::Paths, project: &viy_config::Project) -> Self {
        Self {
            global: paths.config_dir().join("skills"),
            project: project.dir().join("skills"),
        }
    }

    /// `(scope, directory)`, lowest precedence first.
    pub fn scopes(&self) -> [(Scope, &Path); 2] {
        [
            (Scope::Global, self.global.as_path()),
            (Scope::Project, self.project.as_path()),
        ]
    }
}

/// One skill: its name and description from SKILL.md, and its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub scope: Scope,
    pub dir: PathBuf,
}

/// A folder that looked like a skill and is not one, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub dir: PathBuf,
    pub scope: Scope,
    pub reason: String,
}

/// What the two folders hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// The skills in force, by name.
    pub skills: Vec<Skill>,
    /// Global skills a project skill of the same name replaces.
    pub overridden: Vec<Skill>,
    pub skipped: Vec<Skipped>,
}

impl Loaded {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// Why a project's skill is not read when a link leads to it: a cloned
/// repository's link could point into the workspace, where the model
/// writes, as the doctrines' rule has it (kb d109).
const LINKED: &str = "a symbolic link: a project's skills are read only from folders of its own";

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Reads both folders: every subfolder (not hidden) with a SKILL.md.
/// Within a scope, two folders naming the same skill keep the first by
/// folder name; across scopes the project's wins. In the project's scope
/// no link is followed to a skill: not `.viy`, not `.viy/skills`, not a
/// skill's folder (the user's own config folder may hold links).
pub fn load(dirs: &Dirs) -> Loaded {
    let mut loaded = Loaded::default();
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    for (scope, root) in dirs.scopes() {
        let project = scope == Scope::Project;
        if project && (is_link(root) || root.parent().is_some_and(is_link)) {
            if root.is_dir() {
                loaded.skipped.push(Skipped {
                    dir: root.to_path_buf(),
                    scope,
                    reason: LINKED.into(),
                });
            }
            continue;
        }
        let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
        for dir in folders(root) {
            if project && is_link(&dir) {
                loaded.skipped.push(Skipped {
                    dir,
                    scope,
                    reason: LINKED.into(),
                });
                continue;
            }
            let skill = match read(&dir, scope) {
                Ok(skill) => skill,
                Err(reason) => {
                    loaded.skipped.push(Skipped { dir, scope, reason });
                    continue;
                }
            };
            if let Some(first) = seen.get(&skill.name) {
                loaded.skipped.push(Skipped {
                    reason: format!("the name {} is taken by {}", skill.name, first.display()),
                    dir,
                    scope,
                });
                continue;
            }
            seen.insert(skill.name.clone(), dir.clone());
            if let Some(global) = by_name.insert(skill.name.clone(), skill) {
                loaded.overridden.push(global);
            }
        }
    }
    loaded.skills = by_name.into_values().collect();
    loaded
}

/// A root's subfolders holding a SKILL.md, by name; none when the root is
/// missing.
fn folders(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir() && fs::symlink_metadata(p.join(SKILL_FILE)).is_ok())
        .collect();
    dirs.sort();
    dirs
}

fn read(dir: &Path, scope: Scope) -> Result<Skill, String> {
    let text = read_skill_md(dir)?;
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (name, description) = parse(&text, &folder)?;
    Ok(Skill {
        name,
        description,
        scope,
        dir: dir.to_path_buf(),
    })
}

/// SKILL.md's text, read as every file of a skill is ([`read_inside`]):
/// the instructions `load_skill` gives must fit in a request.
fn read_skill_md(dir: &Path) -> Result<String, String> {
    let root = dir
        .canonicalize()
        .map_err(|e| format!("the skill's folder cannot be read: {e}"))?;
    read_inside(&root, &dir.join(SKILL_FILE), SKILL_FILE, || {
        format!("{SKILL_FILE} cannot be read: it is missing")
    })
}

/// A file of a skill: `path` resolved and still under `root`, the skill's
/// resolved folder; a regular file (a FIFO would hang the read) of at
/// most [`FILE_MAX`] bytes; not named as a secret, as the file tools
/// would ask before reading it; UTF-8 without NUL. `shown` names it in
/// the reason, and `missing` is the reason when there is no such file.
fn read_inside(
    root: &Path,
    path: &Path,
    shown: &str,
    missing: impl FnOnce() -> String,
) -> Result<String, String> {
    let cannot = |e: std::io::Error| format!("{shown} cannot be read: {e}");
    let real = match path.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(e) => return Err(cannot(e)),
    };
    if !real.starts_with(root) {
        return Err(format!("{shown} leads outside the skill's folder"));
    }
    let meta = fs::metadata(&real).map_err(cannot)?;
    if meta.is_dir() {
        return Err(format!("{shown} is a folder; name a file in it"));
    }
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if meta.len() > FILE_MAX {
        return Err(format!("{shown} is over 64 KiB"));
    }
    if let Some(why) = viy_tools::guard::secret_reason(&real) {
        return Err(format!("{shown} is not read: {why}"));
    }
    // Non-blocking, and checked again once open: what was a file a moment
    // ago may not be now.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&real)
        .map_err(cannot)?;
    if !file.metadata().map_err(cannot)?.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(cannot)?;
    if bytes.len() as u64 > FILE_MAX {
        return Err(format!("{shown} is over 64 KiB"));
    }
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(text),
        _ => Err(format!("{shown} is not a text file")),
    }
}

/// A name the user can write as `$name`: letters, digits, `-`, `_` and
/// `.`, starting with a letter or digit and not ending in `.`, which
/// [`mentions`] takes for the end of a sentence.
fn mentionable(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `(name, description)` from SKILL.md's frontmatter, as Codex reads them:
/// both folded to one line, `name` the folder's when absent, at most 64
/// characters, `description` required. The frontmatter is the YAML Codex's
/// skills use, read as far as these two keys need: `key: value`, quoted
/// or not, a value continued on indented lines, and `|` or `>` blocks;
/// other keys are skipped with what is indented under them.
pub fn parse(text: &str, folder: &str) -> Result<(String, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().is_none_or(|l| l.trim_end() != "---") {
        return Err("missing YAML frontmatter delimited by ---".into());
    }
    let Some(end) = (1..lines.len()).find(|&i| lines[i].trim_end() == "---") else {
        return Err("the frontmatter is not closed with ---".into());
    };
    let fields = top_level(&lines[1..end]);
    let name = fields
        .get("name")
        .map(|v| one_line(v))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| folder.to_string());
    if name.chars().count() > NAME_MAX {
        return Err(format!("invalid name: longer than {NAME_MAX} characters"));
    }
    if !mentionable(&name) {
        return Err(format!(
            "invalid name {name:?}: letters, digits, '-', '_' and '.', starting with a letter or \
             digit and not ending in '.', so that $name can name it"
        ));
    }
    let description = fields.get("description").map(|v| one_line(v));
    match description.filter(|d| !d.is_empty()) {
        Some(description) => Ok((name, description)),
        None => Err("missing field `description`".into()),
    }
}

/// SKILL.md without its frontmatter: the instructions. All of `text` when
/// it has none (a file that [`parse`] refuses never gets here).
pub fn body(text: &str) -> &str {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().is_none_or(|l| l.trim_end() != "---") {
        return text;
    }
    let mut at = text.find('\n').map_or(text.len(), |i| i + 1);
    for line in lines {
        at += line.len();
        if line.trim_end() == "---" {
            return text[at..].trim_start_matches(['\n', '\r']).trim_end();
        }
    }
    text
}

/// The frontmatter's top-level scalar values, by key.
fn top_level(lines: &[&str]) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // What is indented under the key: a block, a continued scalar, or
        // a nested map or list, which is not a scalar.
        let start = i;
        while i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with([' ', '\t'])) {
            i += 1;
        }
        let under: Vec<&str> = lines[start..i].iter().map(|l| l.trim()).collect();
        let text = if value.starts_with('|') || value.starts_with('>') {
            under.join(" ")
        } else if value.is_empty() {
            // A nested map or list: not one of ours.
            continue;
        } else {
            // A quoted value may close on a later line.
            let mut words = vec![value];
            words.extend(under.iter().copied());
            unquote(words.join(" ").trim())
        };
        fields.insert(key.trim().to_string(), text);
    }
    fields
}

fn unquote(value: &str) -> String {
    let mut ends = value.chars();
    match (ends.next(), ends.next_back()) {
        (Some('"'), Some('"')) => value[1..value.len() - 1].replace("\\\"", "\""),
        (Some('\''), Some('\'')) => value[1..value.len() - 1].replace("''", "'"),
        _ => value.to_string(),
    }
}

/// Whitespace folded to single spaces (Codex's `sanitize_single_line`).
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` cut to `limit` characters, `…` marking the cut.
fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// The catalogue's lines, one a skill (`- name: description [scope]`), in
/// name order, within `budget` characters: the descriptions in full (each
/// at most 1024), else cut to 160, else the names alone, else as many
/// names as fit and a line saying how many more there are. `None` without
/// skills.
pub fn catalogue(skills: &[Skill], budget: usize) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let line = |s: &Skill, limit: Option<usize>| match limit {
        Some(limit) => format!(
            "- {}: {} [{}]",
            s.name,
            cut(&s.description, limit),
            s.scope.as_str()
        ),
        None => format!("- {} [{}]", s.name, s.scope.as_str()),
    };
    for limit in [Some(DESCRIPTION_MAX), Some(SHORT_DESCRIPTION), None] {
        let lines: Vec<String> = skills.iter().map(|s| line(s, limit)).collect();
        let text = lines.join("\n");
        if text.chars().count() <= budget {
            return Some(text);
        }
    }
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0;
    for (i, skill) in skills.iter().enumerate() {
        let next = line(skill, None);
        let more = skills.len() - i;
        let tail = format!("- … {more} more (/skills lists them)");
        if used + next.chars().count() + 1 + tail.chars().count() > budget {
            kept.push(tail);
            return Some(kept.join("\n"));
        }
        used += next.chars().count() + 1;
        kept.push(next);
    }
    Some(kept.join("\n"))
}

/// SKILLS_BUDGET: the catalogue's budget in characters; the default when
/// unset or not a whole number.
pub fn budget(cfg: &viy_config::Config) -> usize {
    cfg.get("SKILLS_BUDGET")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(BUDGET_DEFAULT)
}

/// The system prompt's section: the guide and the catalogue, fenced as
/// `<skills>`; `None` without skills. The descriptions are the user's,
/// but a control token's spelling in one is defused all the same.
pub fn prompt_section(skills: &[Skill], budget: usize) -> Option<String> {
    let lines = catalogue(skills, budget)?;
    let (lines, _) = viy_tools::untrusted::neutralize(&lines);
    let lines = defuse_tags(&lines, "skills");
    Some(format!("<skills>\n{GUIDE}\n{lines}\n</skills>"))
}

/// `text` with `<tag>` and `</tag>` in look-alike brackets, so the user's
/// text cannot close the section or part it stands in.
fn defuse_tags(text: &str, tag: &str) -> String {
    text.replace(&format!("</{tag}>"), &format!("\u{2039}/{tag}\u{203a}"))
        .replace(&format!("<{tag}>"), &format!("\u{2039}{tag}\u{203a}"))
}

/// The load_skill tool (kb d184).
pub fn tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "load_skill",
            "description": "Read one of the user's skills listed in the system prompt. name \
                alone: the skill's instructions (its SKILL.md). name and file: a text file in \
                the skill's folder that the instructions refer to.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {"type": "string",
                             "description": "The skill's name, as listed."},
                    "file": {"type": "string",
                             "description": "A path relative to the skill's folder, e.g. \
                                 references/forms.md."},
                },
                "required": ["name"],
            },
        },
    })
}

/// load_skill's result: `{skill, scope, file, content}`, or `{error}` with
/// the reason. `loaded` is read when the call comes, so a skill edited
/// mid-chat is read as it is now.
pub fn run_load(loaded: &Loaded, args: &Map<String, Value>) -> Value {
    let error = |e: String| json!({"error": e});
    let name = match args.get("name") {
        Some(Value::String(n)) if !n.trim().is_empty() => n.trim(),
        _ => return error("name is required: one of the skills the system prompt lists".into()),
    };
    let Some(skill) = loaded.get(name) else {
        return error(format!(
            "no skill named {name}; the system prompt lists the skills"
        ));
    };
    let file = match args.get("file") {
        None | Some(Value::Null) => None,
        Some(Value::String(f)) if f.trim().is_empty() => None,
        Some(Value::String(f)) => Some(f.trim()),
        Some(_) => return error("file must be a string: a path in the skill's folder".into()),
    };
    let got = match file {
        None => read_body(skill).map(|text| (SKILL_FILE.to_string(), text)),
        Some(rel) => read_file(skill, rel).map(|text| (rel.to_string(), text)),
    };
    match got {
        Ok((file, content)) => json!({
            "skill": skill.name,
            "scope": skill.scope.as_str(),
            "file": file,
            "content": content,
        }),
        Err(e) => error(e),
    }
}

/// A skill's instructions as they are now: SKILL.md without its
/// frontmatter.
pub fn read_body(skill: &Skill) -> Result<String, String> {
    let text = read_skill_md(&skill.dir)?;
    Ok(body(&text).to_string())
}

/// A text file in `skill`'s folder: `rel` relative, without `..`, and
/// read as [`read_inside`] reads, inside the folder once symlinks are
/// resolved. `SKILL.md` gives the instructions, as without a file.
pub fn read_file(skill: &Skill, rel: &str) -> Result<String, String> {
    let parts: Option<Vec<&std::ffi::OsStr>> = Path::new(rel)
        .components()
        .filter(|c| *c != Component::CurDir)
        .map(|c| match c {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    let Some(parts) = parts else {
        return Err(format!(
            "file must be a path inside the skill's folder, without ..: {rel}"
        ));
    };
    if parts == [std::ffi::OsStr::new(SKILL_FILE)] {
        return read_body(skill);
    }
    let root = skill
        .dir
        .canonicalize()
        .map_err(|e| format!("the skill's folder cannot be read: {e}"))?;
    read_inside(&root, &root.join(rel), rel, || {
        format!("no file {rel} in the skill {}", skill.name)
    })
}

/// The `$name` mentions in `text`, in order and once each: `$` at the
/// start or after a character that cannot end a word, then a name's
/// characters, a final `.` not counted (the end of a sentence): every
/// name [`parse`] takes can be named so. `${x}`, `a$b` and `$-x` are not
/// mentions; `$1` is one, which [`looks_like_a_skill`] keeps quiet.
pub fn mentions(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        let starts =
            c == '$' && !prev.is_some_and(|p| p.is_alphanumeric() || matches!(p, '_' | '$' | '\\'));
        prev = Some(c);
        if !starts {
            continue;
        }
        let rest = &text[i + 1..];
        let end = rest
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
            .unwrap_or(rest.len());
        let name = rest[..end].trim_end_matches('.');
        if name.starts_with(|ch: char| ch.is_ascii_alphanumeric()) && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Whether an unknown `$name` is worth a notice: not a shell variable
/// such as `$HOME`, `$XDG_CONFIG_HOME` or `$1`, which has no lowercase
/// letter.
pub fn looks_like_a_skill(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_lowercase())
}

/// The text part a `$name` puts in the user's message: the skill's
/// instructions, a control token's spelling defused as in a tool result.
pub fn part(skill: &Skill, body: &str) -> String {
    let (body, _) = viy_tools::untrusted::neutralize(body);
    let body = defuse_tags(&body, "skill");
    format!("{PART_OPEN}{}</name>\n{body}\n</skill>", skill.name)
}

/// Whether a user message's text part is a skill a `$name` put there, not
/// something the user typed.
pub fn is_part(text: &str) -> bool {
    text.starts_with(PART_OPEN)
}

/// One line the skills commands say, with its level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub level: NoticeLevel,
    pub text: String,
}

fn line(level: NoticeLevel, text: impl Into<String>) -> Line {
    Line {
        level,
        text: text.into(),
    }
}

/// `/skills` and `viy skills`: the skills in force with their scope and
/// folder, those a project skill replaces, and the folders left out with
/// why.
pub fn cmd_list(dirs: &Dirs) -> Vec<Line> {
    let loaded = load(dirs);
    let mut out = Vec::new();
    if loaded.skills.is_empty() && loaded.skipped.is_empty() {
        out.push(line(
            NoticeLevel::Info,
            format!(
                "no skills ({}, {}): a folder with a SKILL.md in either makes one",
                dirs.global.display(),
                dirs.project.display()
            ),
        ));
        return out;
    }
    for skill in &loaded.skills {
        out.push(line(
            NoticeLevel::Info,
            format!(
                "  {}  [{}]  {}",
                skill.name,
                skill.scope.as_str(),
                skill.dir.display()
            ),
        ));
        out.push(line(
            NoticeLevel::Info,
            format!("      {}", cut(&skill.description, 100)),
        ));
    }
    for skill in &loaded.overridden {
        out.push(line(
            NoticeLevel::Info,
            format!(
                "  {}  [global] overridden by the project's {}",
                skill.name, skill.name
            ),
        ));
    }
    for skipped in &loaded.skipped {
        out.push(line(
            NoticeLevel::Warn,
            format!(
                "  ! {}  [{}]: {}",
                skipped.dir.display(),
                skipped.scope.as_str(),
                skipped.reason
            ),
        ));
    }
    out
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
