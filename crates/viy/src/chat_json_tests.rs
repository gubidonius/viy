//! `viy chat --json` (WP 9.32, kb d180): the caller's lines, turns one
//! after another, and who answers a safeguard's question.

use std::io;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::json;
use viy_testkit::mock_llm::MockLlm;
use viy_testkit::scratch::ScratchDir;

use super::*;

#[test]
fn the_callers_lines() {
    assert_eq!(
        parse(r#"{"type":"user","text":"hi"}"#),
        Ok(Input::Submit(Op::UserTurn {
            text: "hi".into(),
            images: Vec::new()
        }))
    );
    assert_eq!(
        parse(r#"{"type":"command","text":"/compact"}"#),
        Ok(Input::Submit(Op::Command {
            text: "/compact".into()
        }))
    );
    assert_eq!(
        parse(r#"{"type":"answer","question_id":"q1","answer":"yes"}"#),
        Ok(Input::Answer {
            question_id: "q1".into(),
            answer: "yes".into()
        })
    );
    assert_eq!(parse(r#"{"type":"interrupt"}"#), Ok(Input::Interrupt));
    assert_eq!(parse(r#"{"type":"shutdown"}"#), Ok(Input::End));
    for (line, why) in [
        ("hi", "not JSON"),
        (r#"{"text":"hi"}"#, "no \"type\""),
        (r#"{"type":"user"}"#, "needs \"text\""),
        (r#"{"type":"user","text":"   "}"#, "needs some text"),
        (r#"{"type":"command","text":"compact"}"#, "starts with /"),
        (
            r#"{"type":"answer","answer":"yes"}"#,
            "needs \"question_id\"",
        ),
        (r#"{"type":"dance"}"#, "unknown type"),
    ] {
        let got = parse(line).unwrap_err();
        assert!(got.contains(why), "{line}: {got}");
    }
}

/// Lines written to `out` as they are completed, for the test to read
/// while the chat runs.
struct Tap(mpsc::UnboundedSender<Value>, Vec<u8>);

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.1.extend_from_slice(buf);
        while let Some(end) = self.1.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.1.drain(..=end).collect();
            let value = serde_json::from_slice(&line).map_err(io::Error::other)?;
            let _ = self.0.send(value);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Rig {
    paths: Paths,
    project: std::path::PathBuf,
    _scratch: ScratchDir,
}

fn rig() -> Rig {
    let scratch = ScratchDir::new("chat-json").unwrap();
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

impl Rig {
    /// The chat against a server answering `responses`, with `caller`
    /// writing the input: it gets each output line and the input sender,
    /// and returns once it has said all it will. Every output line.
    fn chat<F>(&self, answer_questions: bool, responses: Value, caller: F) -> Vec<Value>
    where
        F: FnMut(&Value, &mpsc::UnboundedSender<Result<Input, String>>) + Send,
    {
        self.chat_with(&[], answer_questions, responses, caller)
    }

    /// As [`Rig::chat`], with `settings` on the config.
    fn chat_with<F>(
        &self,
        settings: &[(&str, &str)],
        answer_questions: bool,
        responses: Value,
        mut caller: F,
    ) -> Vec<Value>
    where
        F: FnMut(&Value, &mpsc::UnboundedSender<Result<Input, String>>) + Send,
    {
        let mock = MockLlm::start(&json!({ "responses": responses })).unwrap();
        let args = ChatArgs {
            url: Some(mock.uri()),
            system: None,
            json: true,
            answer_questions,
        };
        let mut cfg = dev::scripted_base();
        cfg.set("CHAT_AUTO_TITLE", "0");
        for (key, value) in settings {
            cfg.set(key, value);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut err = Vec::new();
        let mut seen = Vec::new();
        runtime.block_on(async {
            let chat = serve(
                &args,
                &self.paths,
                cfg,
                &self.project,
                in_rx,
                Tap(out_tx, Vec::new()),
                &mut err,
            );
            let watch = async {
                // The caller's first lines go in before any output.
                caller(&json!({"type": "begin"}), &in_tx);
                while let Some(line) = out_rx.recv().await {
                    caller(&line, &in_tx);
                    seen.push(line);
                }
            };
            let (result, ()) =
                tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(chat, watch) })
                    .await
                    .expect("the chat ended");
            result.unwrap();
        });
        seen
    }
}

fn user(text: &str) -> Result<Input, String> {
    Ok(Input::Submit(Op::UserTurn {
        text: text.into(),
        images: Vec::new(),
    }))
}

fn kinds(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|l| match l["item"]["type"].as_str() {
            Some(item) => format!("{}:{item}", l["type"].as_str().unwrap()),
            None => l["type"].as_str().unwrap().to_string(),
        })
        .collect()
}

/// Two turns in one chat, one after the other, then the end of input
/// closes it: the saved chat holds both.
#[test]
fn two_turns_in_order_then_the_end() {
    let r = rig();
    let lines = r.chat(
        false,
        json!([{"stream": [{"content": "4"}]}, {"stream": [{"content": "6"}]}]),
        |line, tx| {
            if line["type"] == "begin" {
                let _ = tx.send(user("2+2?"));
                let _ = tx.send(user("3+3?"));
                let _ = tx.send(Ok(Input::End));
            }
        },
    );
    assert_eq!(
        kinds(&lines),
        [
            "thread.started",
            "turn.started",
            "item.completed:agent_message",
            "turn.completed",
            "turn.started",
            "item.completed:agent_message",
            "turn.completed",
        ]
    );
    assert_eq!(lines[2]["item"]["text"], "4");
    assert_eq!(lines[5]["item"]["text"], "6");
    let id = lines[0]["thread_id"].as_str().unwrap().to_string();
    let store = viy_agent::sessions::Sessions::new(
        r.paths.sessions_dir(),
        Arc::new(viy_time::SystemTimeProvider),
    );
    assert_eq!(store.list(None)[0].id, id);
}

fn destructive() -> Value {
    json!([
        {"stream": [
            {"tool_call": {"id": "c1", "name": "run_shell",
                           "arguments": "{\"command\": \"rm -rf build\"}"}},
            {"finish": "tool_calls"}
        ]},
        {"stream": [{"content": "done"}]}
    ])
}

/// Without --answer-questions the caller cannot answer: the question takes
/// its unattended no, and an answer line is refused.
#[test]
fn without_the_flag_questions_take_no() {
    let r = rig();
    std::fs::create_dir_all(r.project.join("build")).unwrap();
    let lines = r.chat(false, destructive(), |line, tx| {
        if line["type"] == "begin" {
            let _ = tx.send(user("clean up"));
        }
        if line["type"] == "item.completed" && line["item"]["type"] == "question" {
            let _ = tx.send(Ok(Input::Answer {
                question_id: "anything".into(),
                answer: "yes".into(),
            }));
        }
        if line["type"] == "turn.completed" {
            let _ = tx.send(Ok(Input::End));
        }
    });
    let question = lines
        .iter()
        .find(|l| l["item"]["type"] == "question")
        .unwrap();
    assert_eq!(question["item"]["answer"], "no");
    assert_eq!(question["item"]["answered_by"], "unattended");
    assert!(lines.iter().all(|l| l["type"] != "question.requested"));
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "error" && l["message"].as_str().unwrap().contains("waiting")),
        "{lines:?}"
    );
    assert!(r.project.join("build").exists(), "nothing ran");
}

/// With the flag the caller is asked, with the answers the question takes,
/// and its yes runs the command.
#[test]
fn with_the_flag_the_caller_answers() {
    let r = rig();
    std::fs::create_dir_all(r.project.join("build")).unwrap();
    let lines = r.chat(true, destructive(), |line, tx| {
        if line["type"] == "begin" {
            let _ = tx.send(user("clean up"));
        }
        if line["type"] == "question.requested" {
            // Not an answer it takes, then the real one.
            let id = line["question_id"].as_str().unwrap().to_string();
            let _ = tx.send(Ok(Input::Answer {
                question_id: id.clone(),
                answer: "option 3".into(),
            }));
            let _ = tx.send(Ok(Input::Answer {
                question_id: id,
                answer: "yes".into(),
            }));
        }
        if line["type"] == "turn.completed" {
            let _ = tx.send(Ok(Input::End));
        }
    });
    let asked = lines
        .iter()
        .find(|l| l["type"] == "question.requested")
        .unwrap();
    assert!(
        asked["prompt"].as_str().unwrap().contains("run it?"),
        "{asked}"
    );
    assert_eq!(asked["answers"], json!(["yes", "no"]));
    assert_eq!(asked["default"], "no");
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "error"
                && l["message"].as_str().unwrap().contains("is not an answer")),
        "{lines:?}"
    );
    let answered = lines
        .iter()
        .find(|l| l["item"]["type"] == "question")
        .unwrap();
    assert_eq!(answered["item"]["answer"], "yes");
    assert_eq!(answered["item"]["answered_by"], "caller");
    let call = lines
        .iter()
        .find(|l| l["type"] == "item.completed" && l["item"]["type"] == "command_execution")
        .unwrap();
    assert_eq!(call["item"]["status"], "completed");
    assert!(!r.project.join("build").exists(), "the yes ran it");
}

/// A question still waiting when the input ends takes its unattended no.
#[test]
fn the_end_of_input_answers_a_waiting_question_no() {
    let r = rig();
    std::fs::create_dir_all(r.project.join("build")).unwrap();
    let lines = r.chat(true, destructive(), |line, tx| {
        if line["type"] == "begin" {
            let _ = tx.send(user("clean up"));
        }
        if line["type"] == "question.requested" {
            let _ = tx.send(Ok(Input::End));
        }
    });
    let answered = lines
        .iter()
        .find(|l| l["item"]["type"] == "question")
        .unwrap();
    assert_eq!(answered["item"]["answer"], "no");
    assert_eq!(answered["item"]["answered_by"], "unattended");
    assert!(r.project.join("build").exists(), "nothing ran");
    assert_eq!(lines.last().unwrap()["type"], "turn.completed");
}

/// A line that is not taken is an error line, and the chat goes on.
#[test]
fn a_bad_line_is_refused_and_the_chat_goes_on() {
    let r = rig();
    let lines = r.chat(
        false,
        json!([{"stream": [{"content": "ok"}]}]),
        |line, tx| {
            if line["type"] == "begin" {
                let _ = tx.send(Err("line 1: not JSON: expected value".into()));
                let _ = tx.send(user("hi"));
                let _ = tx.send(Ok(Input::End));
            }
        },
    );
    assert_eq!(
        lines[1],
        json!({"type": "error", "message": "line 1: not JSON: expected value"})
    );
    assert_eq!(lines.last().unwrap()["type"], "turn.completed");
}

/// Under --answer-questions the chat mode answers first, as for the person
/// at the terminal: auto-yes takes the destructive command itself, and the
/// caller is not asked.
#[test]
fn with_the_flag_the_chat_mode_answers_first() {
    let r = rig();
    std::fs::create_dir_all(r.project.join("build")).unwrap();
    let lines = r.chat_with(
        &[("CHAT_MODE", "auto-yes")],
        true,
        destructive(),
        |line, tx| {
            if line["type"] == "begin" {
                let _ = tx.send(user("clean up"));
            }
            if line["type"] == "turn.completed" {
                let _ = tx.send(Ok(Input::End));
            }
        },
    );
    assert!(
        lines.iter().all(|l| l["type"] != "question.requested"),
        "{lines:?}"
    );
    assert!(!r.project.join("build").exists(), "the mode said yes");
}

/// Without --answer-questions nobody is there, so a mode that says yes at
/// the terminal does not here: the unattended no, as in `viy exec`.
#[test]
fn without_the_flag_the_chat_mode_does_not_say_yes() {
    let r = rig();
    std::fs::create_dir_all(r.project.join("build")).unwrap();
    r.chat_with(
        &[("CHAT_MODE", "auto-yes")],
        false,
        destructive(),
        |line, tx| {
            if line["type"] == "begin" {
                let _ = tx.send(user("clean up"));
            }
            if line["type"] == "turn.completed" {
                let _ = tx.send(Ok(Input::End));
            }
        },
    );
    assert!(r.project.join("build").exists(), "nothing ran");
}

#[test]
fn an_interrupt_without_a_turn_is_refused() {
    let r = rig();
    let lines = r.chat(false, json!([]), |line, tx| {
        if line["type"] == "begin" {
            let _ = tx.send(Ok(Input::Interrupt));
            let _ = tx.send(Ok(Input::End));
        }
    });
    assert_eq!(
        lines[1],
        json!({"type": "error", "message": "no turn is running"})
    );
}

/// A read error ends the input instead of repeating; a line that is not
/// UTF-8 is refused and the rest is read.
#[test]
fn stdin_errors_end_the_input() {
    struct Broken;
    impl io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("gone"))
        }
    }
    let mut rx = lines(Box::new(io::BufReader::new(Broken)));
    assert_eq!(rx.blocking_recv(), Some(Err("stdin: gone".into())));
    assert_eq!(rx.blocking_recv(), Some(Ok(Input::End)));
    assert_eq!(rx.blocking_recv(), None);
    let input = b"\xff\xfe\n{\"type\":\"shutdown\"}\n".to_vec();
    let mut rx = lines(Box::new(io::Cursor::new(input)));
    assert_eq!(rx.blocking_recv(), Some(Err("line 1: not UTF-8".into())));
    assert_eq!(rx.blocking_recv(), Some(Ok(Input::End)));
}
