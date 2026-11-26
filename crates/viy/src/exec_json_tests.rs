//! `viy exec --json`'s lines for each kind of event (kb d179).

use pretty_assertions::assert_eq;
use viy_protocol::DeltaEvent;
use viy_protocol::NoticeEvent;
use viy_protocol::Question;
use viy_protocol::QuestionKind;
use viy_protocol::QuestionRequestEvent;
use viy_protocol::RoundKind;
use viy_protocol::RoundStartedEvent;
use viy_protocol::TokenCountEvent;
use viy_protocol::ToolCallBeginEvent;
use viy_protocol::ToolCallEndEvent;
use viy_protocol::ToolCallOutputDeltaEvent;
use viy_protocol::TurnAbortReason;
use viy_protocol::TurnAbortedEvent;
use viy_protocol::TurnCompleteEvent;
use viy_protocol::TurnStartedEvent;

use super::*;

/// The lines `events` make, and the answers given.
fn lines(events: &[EventMsg]) -> (Vec<Value>, Vec<Op>) {
    let mut json = JsonEvents::new(Vec::new());
    json.thread_started(Some("abc123")).unwrap();
    let ops: Vec<Op> = events
        .iter()
        .filter_map(|e| json.event(e).unwrap())
        .collect();
    let text = String::from_utf8(json.into_inner()).unwrap();
    (
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        ops,
    )
}

fn started() -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "t1".into(),
        user_text: "hi".into(),
    })
}

fn round(n: u32) -> EventMsg {
    EventMsg::RoundStarted(RoundStartedEvent {
        round: n,
        kind: RoundKind::Turn,
    })
}

fn text(s: &str) -> EventMsg {
    EventMsg::AgentMessageContentDelta(DeltaEvent { delta: s.into() })
}

fn think(s: &str) -> EventMsg {
    EventMsg::ReasoningContentDelta(DeltaEvent { delta: s.into() })
}

fn begin(call: &str, tool: &str, summary: &str) -> EventMsg {
    EventMsg::ToolCallBegin(ToolCallBeginEvent {
        call_id: call.into(),
        tool: tool.into(),
        summary: summary.into(),
    })
}

fn output(call: &str, chunk: &str) -> EventMsg {
    EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
        call_id: call.into(),
        chunk: chunk.into(),
    })
}

fn end(call: &str, status: ToolCallStatus, detail: Option<&str>) -> EventMsg {
    EventMsg::ToolCallEnd(ToolCallEndEvent {
        call_id: call.into(),
        status,
        detail: detail.map(str::to_string),
        duration_ms: 12,
        sandboxed: true,
        network: false,
    })
}

fn complete(outcome: TurnOutcome) -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "t1".into(),
        outcome,
    })
}

#[test]
fn an_answer_with_its_reasoning_and_a_shell_call() {
    let (lines, ops) = lines(&[
        started(),
        round(1),
        think("Look first."),
        begin("c1", "run_shell", "ls /x"),
        output("c1", "a\n"),
        output("c1", "b\n"),
        end("c1", ToolCallStatus::Failed, Some("exit 2")),
        round(2),
        text("It is "),
        text("empty."),
        EventMsg::TokenCount(TokenCountEvent {
            estimated: false,
            used: 1200,
            budget: 262144,
            auto_compact_at: None,
        }),
        complete(TurnOutcome::Answered),
    ]);
    assert!(ops.is_empty());
    assert_eq!(
        lines,
        [
            json!({"type": "thread.started", "thread_id": "abc123"}),
            json!({"type": "turn.started"}),
            json!({"type": "item.completed",
                   "item": {"id": "item_0", "type": "reasoning", "text": "Look first."}}),
            json!({"type": "item.started",
                   "item": {"id": "item_1", "type": "command_execution", "command": "ls /x",
                            "aggregated_output": "", "exit_code": null,
                            "status": "in_progress"}}),
            json!({"type": "item.completed",
                   "item": {"id": "item_1", "type": "command_execution", "command": "ls /x",
                            "aggregated_output": "a\nb\n", "exit_code": 2,
                            "status": "failed"}}),
            json!({"type": "item.completed",
                   "item": {"id": "item_2", "type": "agent_message", "text": "It is empty."}}),
            json!({"type": "turn.completed", "capped": false,
                   "usage": {"context_tokens": 1200, "context_window": 262144}}),
        ]
    );
}

#[test]
fn other_tools_are_tool_calls() {
    let (lines, _) = lines(&[
        started(),
        begin("c1", "read_file", "src/main.rs"),
        output("c1", "fn main() {}\n"),
        end("c1", ToolCallStatus::Completed, Some("13 bytes")),
        begin("c2", "run_shell", "true"),
        end("c2", ToolCallStatus::Completed, None),
    ]);
    assert_eq!(
        lines[3],
        json!({"type": "item.completed",
               "item": {"id": "item_0", "type": "tool_call", "tool": "read_file",
                        "arguments": "src/main.rs", "output": "fn main() {}\n",
                        "detail": "13 bytes", "status": "completed"}})
    );
    // A shell call that completed without an exit line exited 0.
    assert_eq!(lines[5]["item"]["exit_code"], 0);
}

#[test]
fn a_question_takes_its_unattended_answer_and_says_so() {
    let mut question = Question::yes_no(Some(QuestionKind::Destructive), "Run rm -rf build?");
    question.reason.push(NoticeEvent {
        level: NoticeLevel::Warn,
        code: "review".into(),
        message: "it deletes files".into(),
    });
    let (lines, ops) = lines(&[EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: "q1".into(),
        question,
    })]);
    assert_eq!(
        ops,
        [Op::QuestionAnswer {
            id: "q1".into(),
            answer: Answer::No
        }]
    );
    assert_eq!(
        lines[1],
        json!({"type": "item.completed",
               "item": {"id": "item_0", "type": "question", "prompt": "Run rm -rf build?",
                        "reason": ["it deletes files"], "options": [], "answer": "no",
                        "answered_by": "unattended"}})
    );
}

#[test]
fn failures_and_interruptions_fail_the_turn() {
    let (lines, _) = lines(&[
        started(),
        EventMsg::Notice(NoticeEvent {
            level: NoticeLevel::Error,
            code: "stream".into(),
            message: "the server closed the stream".into(),
        }),
        complete(TurnOutcome::Failed {
            message: "no answer".into(),
        }),
        EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: "t2".into(),
            reason: TurnAbortReason::Interrupted,
        }),
    ]);
    assert_eq!(
        &lines[2..],
        [
            json!({"type": "item.completed",
                   "item": {"id": "item_0", "type": "error", "code": "stream",
                            "message": "the server closed the stream"}}),
            json!({"type": "turn.failed", "error": {"message": "no answer"}}),
            json!({"type": "turn.failed", "error": {"message": "interrupted"}}),
        ]
    );
}

#[test]
fn control_characters_stay_escaped() {
    let mut json = JsonEvents::new(Vec::new());
    json.event(&started()).unwrap();
    json.event(&text("\u{1b}[2Jgone")).unwrap();
    json.event(&complete(TurnOutcome::Answered)).unwrap();
    let raw = String::from_utf8(json.into_inner()).unwrap();
    assert!(!raw.contains('\u{1b}'), "{raw:?}");
    assert!(raw.contains("\\u001b[2Jgone"), "{raw:?}");
}

#[test]
fn hidden_and_c1_characters_are_escaped_and_come_back_whole() {
    let mut json = JsonEvents::new(Vec::new());
    let tricky = "a\u{9b}2J\u{202e}b\u{200b}c\u{7f}d\u{e0101}";
    json.event(&started()).unwrap();
    json.event(&text(tricky)).unwrap();
    json.event(&complete(TurnOutcome::Answered)).unwrap();
    let raw = String::from_utf8(json.into_inner()).unwrap();
    assert!(raw.is_ascii(), "{raw:?}");
    for escape in ["\\u009b", "\\u202e", "\\u200b", "\\u007f", "\\udb40\\udd01"] {
        assert!(raw.contains(escape), "{escape} in {raw:?}");
    }
    let item: Value = serde_json::from_str(raw.lines().nth(1).unwrap()).unwrap();
    assert_eq!(item["item"]["text"], tricky);
}

#[test]
fn text_keeps_its_layout() {
    let (lines, _) = lines(&[
        started(),
        text("  key: v\n  other: w\n"),
        complete(TurnOutcome::Answered),
    ]);
    assert_eq!(lines[2]["item"]["text"], "  key: v\n  other: w\n");
}

/// Two calls the model gave one id each complete as themselves, and a call
/// still running when the turn ends completes as interrupted.
#[test]
fn calls_sharing_an_id_and_calls_left_running() {
    let (lines, _) = lines(&[
        started(),
        begin("dup", "read_file", "a.txt"),
        begin("dup", "read_file", "b.txt"),
        end("dup", ToolCallStatus::Completed, Some("1 bytes")),
        end("dup", ToolCallStatus::Failed, None),
        begin("c9", "run_shell", "sleep 99"),
        EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: "t1".into(),
            reason: TurnAbortReason::Interrupted,
        }),
    ]);
    let done: Vec<(&str, &str, &str)> = lines
        .iter()
        .filter(|l| l["type"] == "item.completed")
        .map(|l| {
            let item = &l["item"];
            let what = item["arguments"]
                .as_str()
                .or(item["command"].as_str())
                .unwrap();
            (
                item["id"].as_str().unwrap(),
                what,
                item["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        done,
        [
            ("item_0", "a.txt", "completed"),
            ("item_1", "b.txt", "failed"),
            ("item_2", "sleep 99", "interrupted"),
        ]
    );
    assert_eq!(lines.last().unwrap()["type"], "turn.failed");
}

/// Items are numbered in the order they began: text before a warning.
#[test]
fn a_notice_comes_after_the_text_before_it() {
    let (lines, _) = lines(&[
        started(),
        text("It is "),
        EventMsg::Notice(NoticeEvent {
            level: NoticeLevel::Warn,
            code: "length_cut".into(),
            message: "cut at the length limit".into(),
        }),
        complete(TurnOutcome::Answered),
    ]);
    assert_eq!(lines[2]["item"]["type"], "agent_message");
    assert_eq!(lines[2]["item"]["id"], "item_0");
    assert_eq!(lines[3]["item"]["type"], "error");
}

/// Under caller answers a pick may be declined, as `q` at the terminal,
/// and the unattended answer is always taken; waiting questions come back
/// oldest first.
#[test]
fn caller_answers_picks_and_order() {
    let mut json = JsonEvents::new(Vec::new()).caller_answers();
    let pick = Question {
        kind: None,
        prompt: "Which chat?".into(),
        choices: viy_protocol::Choices::Pick,
        default: Answer::Pick(0),
        unattended: Answer::No,
        reason: Vec::new(),
        options: vec!["a".into(), "b".into()],
        always_waits: false,
    };
    for id in ["q10", "q9"] {
        let none = json
            .event(&EventMsg::QuestionRequest(QuestionRequestEvent {
                question_id: id.into(),
                question: pick.clone(),
            }))
            .unwrap();
        assert_eq!(none, None, "waits for the caller");
    }
    assert_eq!(json.waiting(), ["q9", "q10"]);
    assert!(
        json.answer("q9", Some(Answer::Pick(5)), "caller")
            .unwrap()
            .is_err()
    );
    assert_eq!(
        json.answer("q9", Some(Answer::No), "caller").unwrap(),
        Ok(Op::QuestionAnswer {
            id: "q9".into(),
            answer: Answer::No
        })
    );
    assert_eq!(
        json.answer("q10", None, "unattended").unwrap(),
        Ok(Op::QuestionAnswer {
            id: "q10".into(),
            answer: Answer::No
        })
    );
    let raw = String::from_utf8(json.into_inner()).unwrap();
    let first: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    assert_eq!(first["type"], "question.requested");
    assert_eq!(first["answers"], json!(["option 1", "option 2", "no"]));
    assert_eq!(first["default"], "option 1");
}
