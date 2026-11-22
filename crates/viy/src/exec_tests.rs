//! `viy exec` (kb d083): rpctl test_features.py AskSaveTests turned
//! around, as Codex's `exec` saves unless `--ephemeral`; `exec resume` and
//! `exec fork`.

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::scratch::ScratchDir;

use super::*;
use crate::cli::ExecForkArgs;
use crate::cli::ExecResumeArgs;

struct Rig {
    paths: Paths,
    project: std::path::PathBuf,
    _scratch: ScratchDir,
}

fn rig() -> Rig {
    let scratch = ScratchDir::new("exec").unwrap();
    let paths = Paths::under(scratch.path().join("home"));
    let project = std::fs::canonicalize(scratch.path())
        .unwrap()
        .join("project");
    std::fs::create_dir_all(&project).unwrap();
    Rig {
        paths,
        project,
        _scratch: scratch,
    }
}

fn args(prompt: &str) -> ExecArgs {
    ExecArgs {
        command: None,
        prompt: Some(prompt.into()),
        url: None,
        system: None,
        think: false,
        ephemeral: false,
        json: false,
    }
}

impl Rig {
    /// Runs `args` against a server streaming `stream`: (stdout, stderr).
    fn exec(&self, mut args: ExecArgs, stream: Value) -> (String, String) {
        let mock = MockLlm::start(&json!({"responses": [{"stream": stream}]})).unwrap();
        args.url = Some(mock.uri());
        let mut cfg = dev::scripted_base();
        cfg.set("CHAT_AUTO_TITLE", "0");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        cmd_exec(&args, &self.paths, cfg, &self.project, &mut out, &mut err).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn store(&self) -> Sessions {
        Sessions::new(self.paths.sessions_dir(), Arc::new(SystemTimeProvider))
    }

    fn contents(&self, id: &str) -> Vec<Value> {
        sessions::replay(&self.store().path_of(id))
            .unwrap()
            .messages
            .iter()
            .map(|m| m["content"].clone())
            .collect()
    }
}

#[test]
fn exec_saves_a_one_shot_chat_and_says_its_id_on_stderr() {
    let r = rig();
    let (out, err) = r.exec(args("what is 2+2"), json!([{"content": "4"}]));
    assert_eq!(out, "4\n");
    let rows = r.store().list(None);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].exec);
    assert_eq!(err, format!("session id: {}\n", rows[0].id));
    assert_eq!(r.contents(&rows[0].id), [json!("what is 2+2"), json!("4")]);
    let events = sessions::read_events(&r.store().path_of(&rows[0].id)).unwrap();
    assert_eq!(events.last().unwrap()["reason"], "exec");
}

#[test]
fn ephemeral_keeps_nothing() {
    let r = rig();
    let mut a = args("what is 2+2");
    a.ephemeral = true;
    let (out, err) = r.exec(a, json!([{"content": "4"}]));
    assert_eq!((out.as_str(), err.as_str()), ("4\n", ""));
    assert_eq!(r.store().list(None), vec![]);
}

/// Not in rp's tests: `--think` writes the trace before the answer, as
/// rp's chat does off a terminal.
#[test]
fn think_writes_the_trace_first() {
    let r = rig();
    let mut a = args("what is 2+2");
    a.think = true;
    let (out, _) = r.exec(a, json!([{"reasoning": "add them"}, {"content": "4"}]));
    assert_eq!(out, "[thinking] add them\n\n4\n");
}

#[test]
fn exec_resume_goes_on_in_the_last_chat_or_one_named() {
    let r = rig();
    r.exec(args("first"), json!([{"content": "one"}]));
    let id = r.store().list(None)[0].id.clone();
    let mut resume = args("");
    resume.prompt = None;
    resume.command = Some(ExecCommand::Resume(ExecResumeArgs {
        session_id: Some("second".into()),
        prompt: None,
        last: true,
        all: false,
    }));
    let (out, err) = r.exec(resume, json!([{"content": "two"}]));
    assert_eq!(out, "two\n");
    assert_eq!(err, format!("session id: {id}\n"));
    assert_eq!(
        r.contents(&id),
        [json!("first"), json!("one"), json!("second"), json!("two")]
    );
    assert_eq!(r.store().list(None).len(), 1);
    let mut by_id = args("");
    by_id.prompt = None;
    by_id.command = Some(ExecCommand::Resume(ExecResumeArgs {
        session_id: Some(id.clone()),
        prompt: Some("third".into()),
        last: false,
        all: false,
    }));
    r.exec(by_id, json!([{"content": "three"}]));
    assert_eq!(r.contents(&id).len(), 6);
}

#[test]
fn exec_fork_goes_on_in_a_one_shot_copy() {
    let r = rig();
    r.exec(args("first"), json!([{"content": "one"}]));
    let id = r.store().list(None)[0].id.clone();
    let mut fork = args("");
    fork.prompt = None;
    fork.command = Some(ExecCommand::Fork(ExecForkArgs {
        session_id: id.clone(),
        prompt: Some("second".into()),
    }));
    let (out, err) = r.exec(fork, json!([{"content": "two"}]));
    assert_eq!(out, "two\n");
    let rows = r.store().list(None);
    assert_eq!(rows.len(), 2);
    let twin = rows.iter().find(|row| row.id != id).unwrap();
    assert!(twin.exec);
    assert_eq!(err, format!("session id: {}\n", twin.id));
    assert_eq!(r.contents(&id).len(), 2);
    assert_eq!(r.contents(&twin.id).len(), 4);
}

#[test]
fn exec_resume_needs_a_chat() {
    let r = rig();
    let mut resume = args("x");
    resume.command = Some(ExecCommand::Resume(ExecResumeArgs {
        session_id: Some("nothing-like-it".into()),
        prompt: Some("hi".into()),
        last: false,
        all: false,
    }));
    let mock = MockLlm::start(&json!({"responses": []})).unwrap();
    resume.url = Some(mock.uri());
    let failed = cmd_exec(
        &resume,
        &r.paths,
        dev::scripted_base(),
        &r.project,
        &mut Vec::new(),
        &mut Vec::new(),
    );
    assert_eq!(
        failed.err().and_then(|f| f.message),
        Some("No session found matching 'nothing-like-it'.".into())
    );
}

// ------------------------------------------------------------ --json (9.31)

impl Rig {
    /// Runs `args` with `--json` against a server answering `responses` in
    /// turn: the lines on stdout, and stderr.
    fn exec_json(&self, args: ExecArgs, responses: Value) -> (Vec<Value>, String) {
        let (lines, err, status) = self.exec_json_status(args, responses);
        assert!(status.is_ok(), "{lines:?}");
        (lines, err)
    }

    /// As [`Rig::exec_json`], with how `cmd_exec` ended.
    fn exec_json_status(
        &self,
        mut args: ExecArgs,
        responses: Value,
    ) -> (Vec<Value>, String, Result<(), Failure>) {
        let mock = MockLlm::start(&json!({ "responses": responses })).unwrap();
        args.url = Some(mock.uri());
        args.json = true;
        let mut cfg = dev::scripted_base();
        cfg.set("CHAT_AUTO_TITLE", "0");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = cmd_exec(&args, &self.paths, cfg, &self.project, &mut out, &mut err);
        let out = String::from_utf8(out).unwrap();
        (
            out.lines()
                .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
                .collect(),
            String::from_utf8(err).unwrap(),
            status,
        )
    }
}

/// Codex's envelope: the saved chat's id first (what `viy exec resume`
/// takes), the turn, the answer as one item, the turn's end (kb d179).
#[test]
fn json_tells_the_run_as_codex_exec_does() {
    let r = rig();
    let (lines, err) = r.exec_json(
        args("what is 2+2"),
        json!([{"stream": [{"reasoning": "add them"}, {"content": "4"}]}]),
    );
    let id = r.store().list(None)[0].id.clone();
    assert_eq!(err, format!("session id: {id}\n"));
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "thread.started",
            "turn.started",
            "item.completed",
            "item.completed",
            "turn.completed"
        ]
    );
    assert_eq!(lines[0]["thread_id"], json!(id));
    assert_eq!(
        lines[2]["item"],
        json!({"id": "item_0", "type": "reasoning", "text": "add them"})
    );
    assert_eq!(
        lines[3]["item"],
        json!({"id": "item_1", "type": "agent_message", "text": "4"})
    );
    assert!(lines[4]["usage"]["context_tokens"].as_u64().unwrap() > 0);
}

/// A tool call starts and completes as an item, before the answer that
/// follows it; an ephemeral run has no thread id.
#[test]
fn json_tells_a_tool_call_with_its_output() {
    let r = rig();
    std::fs::write(r.project.join("notes.txt"), "hello\n").unwrap();
    let mut a = args("what is here");
    a.ephemeral = true;
    let (lines, err) = r.exec_json(
        a,
        json!([
            {"stream": [
                {"tool_call": {"id": "c1", "name": "list_dir", "arguments": "{\"path\": \".\"}"}},
                {"finish": "tool_calls"}
            ]},
            {"stream": [{"content": "notes.txt"}]}
        ]),
    );
    assert_eq!(err, "");
    assert_eq!(
        lines[0],
        json!({"type": "thread.started", "thread_id": null})
    );
    let started = lines.iter().find(|l| l["type"] == "item.started").unwrap();
    assert_eq!(started["item"]["type"], "tool_call");
    assert_eq!(started["item"]["tool"], "list_dir");
    assert_eq!(started["item"]["status"], "in_progress");
    let done = lines
        .iter()
        .find(|l| l["type"] == "item.completed" && l["item"]["type"] == "tool_call")
        .unwrap();
    assert_eq!(done["item"]["id"], started["item"]["id"]);
    assert_eq!(done["item"]["status"], "completed");
    // list_dir shows nothing while it runs; its listing goes to the model.
    assert_eq!(done["item"]["arguments"], ".");
    assert_eq!(done["item"]["output"], "");
    let last_two: Vec<&Value> = lines.iter().rev().take(2).collect();
    assert_eq!(last_two[1]["item"]["text"], "notes.txt");
    assert_eq!(last_two[0]["type"], "turn.completed");
}

/// A safeguard's question takes its unattended no, as in the plain exec,
/// and the caller is told: the question, then the declined call.
#[test]
fn json_tells_a_refused_question_and_the_declined_call() {
    let r = rig();
    std::fs::create_dir_all(r.project.join("build")).unwrap();
    let (lines, _) = r.exec_json(
        args("clean up"),
        json!([
            {"stream": [
                {"tool_call": {"id": "c1", "name": "run_shell",
                               "arguments": "{\"command\": \"rm -rf build\"}"}},
                {"finish": "tool_calls"}
            ]},
            {"stream": [{"content": "left it"}]}
        ]),
    );
    let question = lines
        .iter()
        .find(|l| l["item"]["type"] == "question")
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!(question["item"]["answer"], "no");
    assert_eq!(question["item"]["answered_by"], "unattended");
    let call = lines
        .iter()
        .find(|l| l["type"] == "item.completed" && l["item"]["type"] == "command_execution")
        .unwrap();
    assert_eq!(call["item"]["status"], "declined");
    assert_eq!(call["item"]["command"], "rm -rf build");
    assert!(r.project.join("build").exists(), "nothing ran");
}

/// A turn the server cannot answer fails: `turn.failed`, and exit status
/// 1 for a script that checks it, as `codex exec`.
#[test]
fn json_fails_a_turn_the_server_cannot_answer() {
    let r = rig();
    let (lines, _, status) = r.exec_json_status(args("hi"), json!([]));
    assert_eq!(lines.last().unwrap()["type"], "turn.failed", "{lines:?}");
    assert!(status.is_err());
}
