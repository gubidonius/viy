//! Skills' folders, SKILL.md's frontmatter and the catalogue (WP 9.42).

use std::fs;

use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

fn skill(root: &Path, folder: &str, text: &str) {
    let dir = root.join(folder);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(SKILL_FILE), text).unwrap();
}

fn md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nDo it well.\n")
}

fn dirs(scratch: &ScratchDir) -> Dirs {
    Dirs {
        global: scratch.path().join("config/viy/skills"),
        project: scratch.path().join("work/.viy/skills"),
    }
}

#[test]
fn the_frontmatter_reads_as_codexs() {
    assert_eq!(
        parse(&md("pdf-forms", "Fill PDF forms."), "x").unwrap(),
        ("pdf-forms".into(), "Fill PDF forms.".into())
    );
    // No name: the folder's. Quotes, a colon in the value, whitespace folded.
    assert_eq!(
        parse(
            "---\ndescription: \"Build for AWS:  ECS\n  and Lambda\"\n---\nbody",
            "aws"
        )
        .unwrap(),
        ("aws".into(), "Build for AWS: ECS and Lambda".into())
    );
    // A block scalar, a continued plain value, and keys that are not ours.
    let text = "---\nname: notes\nallowed-tools:\n  - Read\nmetadata:\n  short-description: x\n\
                description: >\n  Write release notes\n  from the git log.\nlicense: MIT\n---\n";
    assert_eq!(
        parse(text, "f").unwrap(),
        (
            "notes".into(),
            "Write release notes from the git log.".into()
        )
    );
    let plain = "---\nname: n\ndescription: one\n  two\n---\n";
    assert_eq!(parse(plain, "f").unwrap().1, "one two");
}

#[test]
fn what_is_not_a_skill_says_why() {
    let refused = |text: &str| parse(text, "folder").unwrap_err();
    assert_eq!(
        refused("# no frontmatter"),
        "missing YAML frontmatter delimited by ---"
    );
    assert_eq!(
        refused("---\nname: a\n"),
        "the frontmatter is not closed with ---"
    );
    assert_eq!(
        refused("---\nname: a\n---\n"),
        "missing field `description`"
    );
    assert_eq!(
        refused("---\nname: a\ndescription:  \n---\n"),
        "missing field `description`"
    );
    let long = "n".repeat(NAME_MAX + 1);
    assert_eq!(
        refused(&md(&long, "d")),
        "invalid name: longer than 64 characters"
    );
    assert!(refused(&md("two words", "d")).starts_with("invalid name \"two words\""));
    assert_eq!(
        parse(&md(&"n".repeat(NAME_MAX), "d"), "f").unwrap().0.len(),
        NAME_MAX
    );
}

#[test]
fn both_folders_load_and_the_project_wins() {
    let scratch = ScratchDir::new("skills").unwrap();
    let dirs = dirs(&scratch);
    skill(&dirs.global, "notes", &md("release-notes", "Global notes."));
    skill(&dirs.global, "pdf", &md("pdf-forms", "Fill PDF forms."));
    skill(
        &dirs.project,
        "notes",
        &md("release-notes", "This project's notes."),
    );
    skill(&dirs.project, ".hidden", &md("hidden", "Never read."));
    fs::create_dir_all(dirs.project.join("empty")).unwrap();
    let loaded = load(&dirs);
    let names: Vec<(&str, Scope)> = loaded
        .skills
        .iter()
        .map(|s| (s.name.as_str(), s.scope))
        .collect();
    assert_eq!(
        names,
        [
            ("pdf-forms", Scope::Global),
            ("release-notes", Scope::Project)
        ]
    );
    assert_eq!(
        loaded.get("release-notes").unwrap().description,
        "This project's notes."
    );
    assert_eq!(loaded.overridden.len(), 1);
    assert_eq!(loaded.overridden[0].scope, Scope::Global);
    assert!(loaded.skipped.is_empty());
}

#[test]
fn a_broken_or_doubled_skill_is_left_out_with_its_reason() {
    let scratch = ScratchDir::new("skills").unwrap();
    let dirs = dirs(&scratch);
    skill(&dirs.global, "a-first", &md("same", "First."));
    skill(&dirs.global, "b-second", &md("same", "Second."));
    skill(&dirs.project, "broken", "no frontmatter here");
    let loaded = load(&dirs);
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.skills[0].description, "First.");
    let reasons: Vec<String> = loaded.skipped.iter().map(|s| s.reason.clone()).collect();
    assert_eq!(
        reasons,
        [
            format!(
                "the name same is taken by {}",
                dirs.global.join("a-first").display()
            ),
            "missing YAML frontmatter delimited by ---".to_string(),
        ]
    );
    let listed: Vec<String> = cmd_list(&dirs).into_iter().map(|l| l.text).collect();
    assert!(
        listed
            .iter()
            .any(|l| l.ends_with("[project]: missing YAML frontmatter delimited by ---")),
        "{listed:?}"
    );
}

#[test]
fn no_folders_no_skills() {
    let scratch = ScratchDir::new("skills").unwrap();
    let dirs = dirs(&scratch);
    assert_eq!(load(&dirs), Loaded::default());
    assert_eq!(catalogue(&[], BUDGET_DEFAULT), None);
    assert!(cmd_list(&dirs)[0].text.starts_with("no skills ("));
}

#[test]
fn the_catalogue_fits_its_budget() {
    let skills: Vec<Skill> = (0..30)
        .map(|i| Skill {
            name: format!("skill-{i:02}"),
            description: "x".repeat(300),
            scope: Scope::Global,
            dir: PathBuf::from("/s"),
        })
        .collect();
    let full = catalogue(&skills[..2], BUDGET_DEFAULT).unwrap();
    assert_eq!(full.lines().count(), 2);
    assert!(full.starts_with(&format!("- skill-00: {} [global]", "x".repeat(300))));
    // 30 x 300 characters is over 8000: descriptions cut to 160.
    let cut = catalogue(&skills, BUDGET_DEFAULT).unwrap();
    assert!(cut.chars().count() <= BUDGET_DEFAULT);
    assert!(cut.lines().all(|l| l.contains('…')), "{cut}");
    // Smaller: names alone, then as many as fit and a count.
    let names = catalogue(&skills, 600).unwrap();
    assert_eq!(names.lines().next(), Some("- skill-00 [global]"));
    let few = catalogue(&skills, 100).unwrap();
    assert!(few.chars().count() <= 100, "{few}");
    assert!(few.ends_with("more (/skills lists them)"), "{few}");
    let long = Skill {
        description: "y".repeat(2000),
        ..skills[0].clone()
    };
    let one = catalogue(&[long], 100_000).unwrap();
    assert_eq!(one.matches('y').count(), DESCRIPTION_MAX - 1);
}

#[test]
fn the_budget_key() {
    let mut cfg = viy_config::Config::defaults();
    assert_eq!(budget(&cfg), BUDGET_DEFAULT);
    cfg.set("SKILLS_BUDGET", " 3000 ");
    assert_eq!(budget(&cfg), 3000);
    cfg.set("SKILLS_BUDGET", "lots");
    assert_eq!(budget(&cfg), BUDGET_DEFAULT);
}
