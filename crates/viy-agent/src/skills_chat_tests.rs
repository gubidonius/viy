//! Skills in a chat (WP 9.43, kb d183, d184): the catalogue frozen in the
//! system prompt, load_skill, `$name`, and what a skill is not: a taint,
//! fenced text, or the user's word to the reviewer.

use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::chat::tests::Direct;
use crate::chat::tests::shell_chat;
use crate::chat::tests::tooled;

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

/// The chat's skill folders in `home`, holding the global skill `notes`,
/// frozen as a chat starting now would freeze them.
fn with_notes(d: &mut Direct, home: &ScratchDir) -> crate::skills::Dirs {
    let dirs = crate::skills::Dirs {
        global: home.path().join("config/skills"),
        project: home.path().join("proj/.viy/skills"),
    };
    write_skill(
        &dirs.global,
        "notes",
        "Release notes.",
        "# Notes\n\nShort lines.",
    );
    d.chat.places.skills = dirs.clone();
    d.chat.snapshot_skills();
    dirs
}

fn names(c: &Chat) -> Vec<String> {
    c.tool_specs()
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

fn system(c: &Chat) -> String {
    c.system_prompt().unwrap_or_default()
}

fn call(name: &str, args: Value) -> crate::stream::ToolCall {
    crate::stream::ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

fn call_ev(name: &str, args: Value) -> Value {
    json!({"tool_call": {"id": "c1", "name": name, "arguments": args.to_string()}})
}

#[test]
fn no_skills_no_section_and_no_tool() {
    let (mut d, _ws) = shell_chat(json!([]), &[]);
    let home = ScratchDir::new("skills-home").unwrap();
    d.chat.places.skills = crate::skills::Dirs {
        global: home.path().join("none"),
        project: home.path().join("none-either"),
    };
    d.chat.snapshot_skills();
    assert!(!names(&d.chat).contains(&"load_skill".to_string()));
    assert!(!system(&d.chat).contains("<skills>"));
}

#[tokio::test]
async fn the_catalogue_is_frozen_until_a_new_history_or_a_reload() {
    let (mut d, _ws) = shell_chat(json!([]), &[]);
    let home = ScratchDir::new("skills-home").unwrap();
    let dirs = with_notes(&mut d, &home);
    let before = system(&d.chat);
    assert!(
        before.ends_with(&format!(
            "<skills>\n{}\n- notes: Release notes. [global]\n</skills>",
            crate::skills::GUIDE
        )),
        "{before}"
    );
    assert_eq!(
        names(&d.chat).last().map(String::as_str),
        Some("load_skill")
    );
    write_skill(&dirs.project, "pdf-forms", "Fill PDF forms.", "Use pdftk.");
    assert_eq!(system(&d.chat), before); // the prefix cache holds
    let ctx = d.ctx.clone();
    let (r, _) = d
        .chat
        .dispatch(&ctx, &call("load_skill", json!({"name": "pdf-forms"})))
        .await;
    assert_eq!(r["content"], "Use pdftk."); // but it can be read
    d.said();
    d.chat.command(&ctx, "/skills").await;
    let said = d.said();
    assert!(
        said.iter()
            .any(|l| l.starts_with("not in this chat's system prompt yet")
                && l.ends_with("adds them): pdf-forms")),
        "{said:?}"
    );
    d.chat.command(&ctx, "/skills reload").await;
    assert!(system(&d.chat).contains("- pdf-forms: Fill PDF forms. [project]"));
    assert_eq!(
        d.said(),
        [
            "2 skill(s) in the system prompt (it changes, so the next message re-reads the whole \
          history)"
        ]
    );
    write_skill(&dirs.global, "later", "Saved later.", "Later.");
    d.chat.new_session(&ctx, None).await;
    assert!(system(&d.chat).contains("- later: Saved later. [global]"));
}

#[tokio::test]
async fn with_build_tools_off_the_model_is_not_told() {
    let (mut d, _ws) = shell_chat(json!([]), &[("TOOLS_ENABLED", "0")]);
    let home = ScratchDir::new("skills-home").unwrap();
    with_notes(&mut d, &home);
    assert!(!names(&d.chat).contains(&"load_skill".to_string()));
    assert!(!system(&d.chat).contains("<skills>"));
    let ctx = d.ctx.clone();
    let (r, _) = d
        .chat
        .dispatch(&ctx, &call("load_skill", json!({"name": "notes"})))
        .await;
    assert_eq!(r, json!({"error": "the load_skill tool is off"}));
}

/// A turn whose model loads `notes` and answers; the tool message's text
/// and the chat's taint after it.
async fn turn_loading_notes(taint: Option<&str>) -> (Value, Option<String>, Value) {
    let (mut d, _ws) = tooled(
        json!([
            {"stream": [call_ev("load_skill", json!({"name": "notes"})), {"finish": "tool_calls"}]},
            {"stream": [{"content": "done"}]},
        ]),
        &[],
    );
    let home = ScratchDir::new("skills-home").unwrap();
    with_notes(&mut d, &home);
    if let Some(why) = taint {
        d.chat.taint(why.into());
    }
    d.send("write the release notes").await;
    let tool = d.chat.messages()[2].clone();
    assert_eq!(tool["role"], "tool");
    let first = d.mock.chat_bodies()[1].clone();
    (tool, d.chat.web_taint().map(str::to_string), first)
}

#[tokio::test]
async fn a_skill_is_not_fenced_and_does_not_taint() {
    let (tool, taint, sent) = turn_loading_notes(None).await;
    let text = tool["content"].as_str().unwrap();
    assert!(!text.contains("UNTRUSTED"), "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        json!({"skill": "notes", "scope": "global", "file": "SKILL.md",
               "content": "# Notes\n\nShort lines."})
    );
    assert_eq!(taint, None);
    assert!(
        sent["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("- notes: Release notes. [global]")
    );
    assert!(
        sent["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "load_skill")
    );
}

#[tokio::test]
async fn fetched_text_stays_untrusted_and_the_skill_stays_unfenced() {
    let (tool, taint, _) = turn_loading_notes(Some("a fetched web page")).await;
    let text = tool["content"].as_str().unwrap();
    assert!(!text.contains("UNTRUSTED"), "{text}");
    // A skill read after a fetch does not make the chat trusted again.
    assert_eq!(taint.as_deref(), Some("a fetched web page"));
}

#[tokio::test]
async fn dollar_name_sends_the_skill_with_the_message() {
    let (mut d, _ws) = tooled(json!([{"stream": [{"content": "ok"}]}]), &[]);
    let home = ScratchDir::new("skills-home").unwrap();
    with_notes(&mut d, &home);
    let ctx = d.ctx.clone();
    let line = "draft them with $notes, not $nope; $HOME stays".to_string();
    d.chat.attach_skills(&ctx, &line);
    assert_eq!(
        d.said(),
        [
            "skill notes [global] goes with this message",
            "no skill named nope: $nope goes as you wrote it (/skills lists them)",
        ]
    );
    let part = "<skill>\n<name>notes</name>\n# Notes\n\nShort lines.\n</skill>";
    let content = d.chat.user_content(line.clone());
    assert_eq!(
        content,
        json!([{"type": "text", "text": part}, {"type": "text", "text": line}])
    );
    assert_eq!(crate::shell::typed_parts(&content), [line.as_str()]);
    d.chat.send(&ctx, content).await;
    let sent = d.mock.chat_bodies().last().unwrap().clone();
    let msgs = sent["messages"].as_array().unwrap();
    assert_eq!(msgs[msgs.len() - 1]["content"][0]["text"], part);
    // Not the user's request to the reviewer, nor what the chat is about,
    // nor what the transcript shows as typed; and no taint on resume.
    let asked = d.chat.recent_user_requests();
    assert!(
        !asked.is_empty() && asked.iter().all(|r| *r == line),
        "{asked:?}"
    );
    let messages = d.chat.messages().to_vec();
    assert_eq!(
        crate::sessions::default_title(&messages).as_deref(),
        Some("draft them with $notes, not $nope; $HOME stays")
    );
    let shown: Vec<_> = history_items(&messages, &[])
        .into_iter()
        .filter_map(|i| match i {
            viy_protocol::HistoryItem::User { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(shown, [line]);
    d.chat.reset_web(&messages);
    assert_eq!(d.chat.web_taint(), None);
    // Nothing waits for the next message.
    assert_eq!(d.chat.user_content("next".into()), json!("next"));
    // Named again, unchanged: not sent a second time.
    d.said();
    d.chat.attach_skills(&ctx, "again with $notes");
    assert_eq!(
        d.said(),
        ["skill notes is already in this chat as it reads now; not sent again"]
    );
    assert_eq!(
        d.chat.user_content("again with $notes".into()),
        json!("again with $notes")
    );
}

#[test]
fn load_skill_runs_in_parallel_batches() {
    assert!(super::tools::parallel_safe("load_skill"));
    assert!(super::tools::parallel_safe("find_tool"));
    assert!(!super::tools::parallel_safe("run_shell"));
}

#[tokio::test]
async fn a_skill_saved_into_a_chat_without_skills_is_only_for_dollar_name() {
    let (mut d, _ws) = shell_chat(json!([]), &[]);
    let home = ScratchDir::new("skills-home").unwrap();
    let dirs = crate::skills::Dirs {
        global: home.path().join("config/skills"),
        project: home.path().join("proj/.viy/skills"),
    };
    d.chat.places.skills = dirs.clone();
    d.chat.snapshot_skills();
    write_skill(&dirs.global, "notes", "Release notes.", "Short lines.");
    let ctx = d.ctx.clone();
    d.chat.command(&ctx, "/skills").await;
    let said = d.said();
    assert!(
        said.contains(
            &"not in this chat's system prompt yet ($name finds them; /skills reload adds \
              them): notes"
                .to_string()
        ),
        "{said:?}"
    );
    let (r, _) = d
        .chat
        .dispatch(&ctx, &call("load_skill", json!({"name": "notes"})))
        .await;
    assert_eq!(r, json!({"error": "the load_skill tool is off"}));
}
