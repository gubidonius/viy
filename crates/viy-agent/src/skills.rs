//! Skills (WP 9.42, kb d183, docs/skills-plan.md): folders of instructions
//! the user saved for particular tasks, each a `SKILL.md` with `name` and
//! `description` in its frontmatter, the format Codex and Claude Code use
//! (Codex 970111f `skills/src/parser.rs`). Two scopes, as the toolbox's:
//! global (`~/.config/viy/skills`) and the project's (`.viy/skills`), the
//! project's winning on a name. Both are out of the model's reach for
//! writes, and nothing here writes to them.
//!
//! This module finds and reads the skills and renders their catalogue;
//! the system prompt and the `load_skill` tool take it in WP 9.43.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

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

/// Reads both folders: every subfolder (not hidden) with a SKILL.md.
/// Within a scope, two folders naming the same skill keep the first by
/// folder name; across scopes the project's wins.
pub fn load(dirs: &Dirs) -> Loaded {
    let mut loaded = Loaded::default();
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    for (scope, root) in dirs.scopes() {
        let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
        for dir in folders(root) {
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
        .filter(|p| p.is_dir() && p.join(SKILL_FILE).is_file())
        .collect();
    dirs.sort();
    dirs
}

fn read(dir: &Path, scope: Scope) -> Result<Skill, String> {
    let text = fs::read_to_string(dir.join(SKILL_FILE))
        .map_err(|e| format!("{SKILL_FILE} cannot be read: {e}"))?;
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

/// A name the user can write as `$name`: letters, digits, `-`, `_`, `.`.
fn mentionable(name: &str) -> bool {
    name.chars()
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
            "invalid name {name:?}: only letters, digits, '-', '_' and '.', so that $name can name it"
        ));
    }
    let description = fields.get("description").map(|v| one_line(v));
    match description.filter(|d| !d.is_empty()) {
        Some(description) => Ok((name, description)),
        None => Err("missing field `description`".into()),
    }
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
