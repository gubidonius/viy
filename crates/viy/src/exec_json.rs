//! `viy exec --json`: the run as JSON lines on stdout, in Codex's
//! envelope (`codex exec --json`, codex-rs/exec `exec_events.rs`; kb
//! d179): `thread.started`, `turn.started`, `item.started` and
//! `item.completed`, `turn.completed` or `turn.failed`, and `error`. It is
//! a schema of its own, not the engine's protocol, so the protocol can
//! change without breaking a caller.
//!
//! Items (`item_0`, `item_1`, ... in the order they begin): `agent_message`
//! and `reasoning` come completed, once their text has ended; a tool call
//! starts when the engine begins it and completes with what it showed
//! while it ran, `command_execution` for run_shell as Codex's (its output
//! and exit code), `tool_call` for viy's other tools (their arguments as
//! the transcript shows them; most show no output, their result goes to
//! the model). A safeguard's question takes its unattended answer, as in
//! the plain `viy exec` (no for every approval), and is told as a
//! `question` item, so a caller sees what was refused. Warnings and errors
//! the chat says are `error` items. Every control char (C0, DEL, C1) and
//! every char that reorders text or draws nothing (the sanitizer's, ui-plan
//! 7) is written as a `\uXXXX` escape, so nothing in a line acts on a
//! terminal, and a JSON reader gets the text back whole.
//!
//! The lines, one JSON object each:
//!
//! ```text
//! {"type":"thread.started","thread_id":"<chat id, or null with --ephemeral>"}
//! {"type":"turn.started"}
//! {"type":"item.started","item":{...}}      a tool call, status in_progress
//! {"type":"item.completed","item":{...}}
//! {"type":"turn.completed","capped":false,"usage":{"context_tokens":N,"context_window":M}}
//! {"type":"turn.failed","error":{"message":"..."}}
//! {"type":"error","message":"..."}          the line was refused
//! ```
//!
//! Under `viy chat --json --answer-questions` ([`crate::chat_json`]) a
//! question waits for the caller instead:
//!
//! ```text
//! {"type":"question.requested","question_id":"q1","prompt":"...","reason":[...],
//!  "options":[...],"answers":["yes","no"],"default":"no"}
//! {"type":"question.resolved","question_id":"q1","by":"<mode, or null>"}
//! ```
//!
//! answered by the caller's `answer` line, or closed by the engine
//! (`question.resolved`: the chat mode answered it, or the turn that asked
//! is gone; a `question` item the caller's answer made before that still
//! stands in the output, but the engine took no answer).
//!
//! Every item has `id` and `type`:
//!
//! ```text
//! agent_message      text
//! reasoning          text
//! command_execution  command, aggregated_output, exit_code, status
//! tool_call          tool, arguments, output, detail, status
//! question           prompt, reason, options, answer, answered_by
//!                    (`unattended`, or `caller` under --answer-questions)
//! error              code, message
//! ```
//!
//! A tool's `status` is `in_progress`, then `completed`, `failed`,
//! `declined` (a safeguard or an answer said no) or `interrupted`; a
//! question's `answer` is `yes`, `no`, `always`, `cancel`, `option N`, or
//! `none` for a secret nobody here may type. `command_execution.command`
//! and `tool_call.arguments` are the call as the transcript shows it: cut
//! at 300 characters, controls drawn as escapes. A turn that fails or is
//! interrupted ends `viy exec --json` with exit status 1, as `codex exec`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::io::Write;

use serde_json::Value;
use serde_json::json;
use viy_protocol::Answer;
use viy_protocol::Choices;
use viy_protocol::EventMsg;
use viy_protocol::NoticeLevel;
use viy_protocol::Op;
use viy_protocol::Question;
use viy_protocol::ToolCallStatus;
use viy_protocol::TurnOutcome;
use viy_term::sanitize::needs_json_escape;

/// A tool call between its begin and its end.
#[derive(Clone)]
struct Running {
    item_id: String,
    tool: String,
    summary: String,
    output: String,
}

pub struct JsonEvents<W: Write> {
    out: W,
    next_item: u64,
    /// The answer's text since the last item, and the trace's.
    text: String,
    reasoning: String,
    /// By call id, in begin order: the model may give two calls one id.
    running: HashMap<String, VecDeque<Running>>,
    /// The context in use and its budget, from the last token count.
    context: Option<(u64, u64)>,
    /// Questions go to the caller (`viy chat --json --answer-questions`)
    /// instead of taking their unattended answers; by id, those waiting.
    caller_answers: bool,
    waiting: HashMap<String, Question>,
}

impl<W: Write> JsonEvents<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            next_item: 0,
            text: String::new(),
            reasoning: String::new(),
            running: HashMap::new(),
            context: None,
            caller_answers: false,
            waiting: HashMap::new(),
        }
    }

    /// Questions to the caller: a `question.requested` line, answered with
    /// [`JsonEvents::answer`] (kb d180).
    pub fn caller_answers(mut self) -> Self {
        self.caller_answers = true;
        self
    }

    /// The caller's (or, at the end of its input, the unattended) answer
    /// to a waiting question, told as a `question` item: the answer to give
    /// the engine, or why there is none.
    pub fn answer(
        &mut self,
        question_id: &str,
        answer: Option<Answer>,
        by: &str,
    ) -> io::Result<Result<Op, String>> {
        let Some(question) = self.waiting.get(question_id) else {
            return Ok(Err(format!("no question {question_id:?} is waiting")));
        };
        let answer = answer.unwrap_or(question.unattended);
        // The unattended answer is always taken: nobody left to ask.
        if by != "unattended" && !offers(question, answer) {
            return Ok(Err(format!(
                "{} is not an answer to question {question_id:?}",
                answer_name(answer)
            )));
        }
        let question = self
            .waiting
            .remove(question_id)
            .unwrap_or_else(|| unreachable!());
        self.question_item(&question, answer, by)?;
        Ok(Ok(Op::QuestionAnswer {
            id: question_id.to_string(),
            answer,
        }))
    }

    /// The questions still waiting, oldest first (`q9` before `q10`, as
    /// the engine orders them).
    pub fn waiting(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.waiting.keys().cloned().collect();
        ids.sort_by_key(|id| (id.len(), id.clone()));
        ids
    }

    /// A line the caller sent that is not taken.
    pub fn refused(&mut self, message: &str) -> io::Result<()> {
        self.line(&json!({"type": "error", "message": message}))
    }

    fn question_item(&mut self, question: &Question, answer: Answer, by: &str) -> io::Result<()> {
        let id = self.item_id();
        let reason: Vec<&str> = question.reason.iter().map(|n| n.message.as_str()).collect();
        self.completed(json!({
            "id": id,
            "type": "question",
            "prompt": question.prompt,
            "reason": reason,
            "options": question.options,
            "answer": answer_name(answer),
            "answered_by": by,
        }))
    }

    pub fn into_inner(self) -> W {
        self.out
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    fn line(&mut self, value: &Value) -> io::Result<()> {
        let text = serde_json::to_string(value).map_err(io::Error::other)?;
        let mut line = String::with_capacity(text.len() + 1);
        for c in text.chars() {
            if needs_json_escape(c) {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    line.push_str(&format!("\\u{unit:04x}"));
                }
            } else {
                line.push(c);
            }
        }
        line.push('\n');
        self.out.write_all(line.as_bytes())?;
        self.out.flush()
    }

    fn item_id(&mut self) -> String {
        let id = format!("item_{}", self.next_item);
        self.next_item += 1;
        id
    }

    fn completed(&mut self, item: Value) -> io::Result<()> {
        self.line(&json!({"type": "item.completed", "item": item}))
    }

    /// The first line: the saved chat's id, which `viy exec resume` takes,
    /// or null for an ephemeral run.
    pub fn thread_started(&mut self, thread_id: Option<&str>) -> io::Result<()> {
        self.line(&json!({"type": "thread.started", "thread_id": thread_id}))
    }

    /// The reasoning, then the answer, written so far: each an item, its
    /// text as it came.
    fn flush_text(&mut self) -> io::Result<()> {
        let reasoning = std::mem::take(&mut self.reasoning);
        if !reasoning.trim().is_empty() {
            let id = self.item_id();
            self.completed(json!({"id": id, "type": "reasoning", "text": reasoning}))?;
        }
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            let id = self.item_id();
            self.completed(json!({"id": id, "type": "agent_message", "text": text}))?;
        }
        Ok(())
    }

    /// The turn is over: what it left, and every call still running,
    /// completed as interrupted.
    fn end_turn(&mut self) -> io::Result<()> {
        self.flush_text()?;
        let mut left: Vec<Running> = self.running.drain().flat_map(|(_, q)| q).collect();
        left.sort_by_key(|r| r.item_id[5..].parse::<u64>().unwrap_or(0));
        for running in left {
            self.completed(tool_item(&running, "interrupted", None))?;
        }
        Ok(())
    }

    /// One engine event; the answer to give, for a question.
    pub fn event(&mut self, msg: &EventMsg) -> io::Result<Option<Op>> {
        match msg {
            EventMsg::TurnStarted(_) => self.line(&json!({"type": "turn.started"}))?,
            EventMsg::RoundStarted(_) => self.flush_text()?,
            EventMsg::ReasoningContentDelta(d) => self.reasoning.push_str(&d.delta),
            EventMsg::AgentMessageContentDelta(d) => self.text.push_str(&d.delta),
            EventMsg::ToolCallBegin(e) => {
                self.flush_text()?;
                let item_id = self.item_id();
                let running = Running {
                    item_id,
                    tool: e.tool.clone(),
                    summary: e.summary.clone(),
                    output: String::new(),
                };
                let item = tool_item(&running, "in_progress", None);
                self.running
                    .entry(e.call_id.clone())
                    .or_default()
                    .push_back(running);
                self.line(&json!({"type": "item.started", "item": item}))?;
            }
            EventMsg::ToolCallOutputDelta(e) => {
                // Calls sharing an id run one after another: the oldest.
                if let Some(running) = self
                    .running
                    .get_mut(&e.call_id)
                    .and_then(VecDeque::front_mut)
                {
                    running.output.push_str(&e.chunk);
                }
            }
            EventMsg::ToolCallEnd(e) => {
                let running = self
                    .running
                    .get_mut(&e.call_id)
                    .and_then(VecDeque::pop_front);
                if self.running.get(&e.call_id).is_some_and(VecDeque::is_empty) {
                    self.running.remove(&e.call_id);
                }
                if let Some(running) = running {
                    let status = match e.status {
                        ToolCallStatus::Completed => "completed",
                        ToolCallStatus::Failed => "failed",
                        ToolCallStatus::Declined => "declined",
                        _ => "interrupted",
                    };
                    let item = tool_item(&running, status, e.detail.as_deref());
                    self.completed(item)?;
                }
            }
            EventMsg::Notice(n) if n.level != NoticeLevel::Info => {
                self.flush_text()?;
                let id = self.item_id();
                self.completed(json!({
                    "id": id, "type": "error", "code": n.code, "message": n.message,
                }))?;
            }
            EventMsg::TokenCount(t) => self.context = Some((t.used, t.budget)),
            EventMsg::QuestionRequest(e) if self.caller_answers => {
                self.flush_text()?;
                let q = &e.question;
                let reason: Vec<&str> = q.reason.iter().map(|n| n.message.as_str()).collect();
                let answers: Vec<String> = answers(q).into_iter().map(answer_name).collect();
                self.line(&json!({
                    "type": "question.requested",
                    "question_id": e.question_id,
                    "prompt": q.prompt,
                    "reason": reason,
                    "options": q.options,
                    "answers": answers,
                    "default": answer_name(q.default),
                }))?;
                self.waiting.insert(e.question_id.clone(), q.clone());
            }
            EventMsg::QuestionResolved(e) => {
                // Answered by the chat mode, or nothing waits for it now.
                if self.waiting.remove(&e.question_id).is_some() {
                    self.line(&json!({
                        "type": "question.resolved",
                        "question_id": e.question_id,
                        "by": e.by,
                    }))?;
                }
            }
            EventMsg::QuestionRequest(e) => {
                self.flush_text()?;
                let answer = e.question.unattended;
                self.question_item(&e.question, answer, "unattended")?;
                return Ok(Some(Op::QuestionAnswer {
                    id: e.question_id.clone(),
                    answer,
                }));
            }
            // Nobody here may type one: refused, as in the plain exec.
            EventMsg::SecretRequest(e) => {
                self.flush_text()?;
                let id = self.item_id();
                self.completed(json!({
                    "id": id,
                    "type": "question",
                    "prompt": e.prompt,
                    "reason": [],
                    "options": [],
                    "answer": "none",
                    "answered_by": "unattended",
                }))?;
                return Ok(Some(Op::SecretAnswer {
                    id: e.question_id.clone(),
                    secret: None,
                }));
            }
            EventMsg::TurnComplete(e) => {
                self.end_turn()?;
                match &e.outcome {
                    TurnOutcome::Failed { message } => {
                        self.line(&json!({"type": "turn.failed", "error": {"message": message}}))?;
                    }
                    outcome => {
                        let (used, budget) = self.context.unwrap_or((0, 0));
                        self.line(&json!({
                            "type": "turn.completed",
                            "capped": matches!(outcome, TurnOutcome::Capped),
                            "usage": {"context_tokens": used, "context_window": budget},
                        }))?;
                    }
                }
            }
            EventMsg::TurnAborted(_) => {
                self.end_turn()?;
                self.line(&json!({"type": "turn.failed", "error": {"message": "interrupted"}}))?;
            }
            EventMsg::TurnRefused(e) => {
                self.line(&json!({"type": "error", "message": e.message}))?;
            }
            _ => {}
        }
        Ok(None)
    }
}

/// The answers `question` takes, in the order its choices list them.
fn answers(question: &Question) -> Vec<Answer> {
    match question.choices {
        Choices::YesNo => vec![Answer::Yes, Answer::No],
        Choices::YesNoAlways => vec![Answer::Yes, Answer::No, Answer::Always],
        Choices::YesNoCancel => vec![Answer::Yes, Answer::No, Answer::Cancel],
        // A pick may be declined, as `q` does at the terminal.
        Choices::Pick => (0..question.options.len())
            .filter_map(|i| u32::try_from(i).ok().map(Answer::Pick))
            .chain([Answer::No])
            .collect(),
        // A kind added later: yes or no, until it is named here.
        _ => vec![Answer::Yes, Answer::No],
    }
}

fn offers(question: &Question, answer: Answer) -> bool {
    answers(question).contains(&answer)
}

/// The caller's word for an answer, as [`answer_name`] writes it.
pub fn parse_answer(word: &str) -> Option<Answer> {
    match word.trim() {
        "yes" => Some(Answer::Yes),
        "no" => Some(Answer::No),
        "always" => Some(Answer::Always),
        "cancel" => Some(Answer::Cancel),
        other => other
            .strip_prefix("option ")
            .and_then(|n| n.trim().parse::<u32>().ok())
            .filter(|n| *n >= 1)
            .map(|n| Answer::Pick(n - 1)),
    }
}

/// An answer as a word: `yes`, `no`, `always`, `cancel`, or `option N`
/// (counted from 1) for a pick.
fn answer_name(answer: Answer) -> String {
    match answer {
        Answer::Yes => "yes".into(),
        Answer::No => "no".into(),
        Answer::Always => "always".into(),
        Answer::Cancel => "cancel".into(),
        Answer::Pick(n) => format!("option {}", u64::from(n) + 1),
    }
}

/// A tool call as an item: run_shell as Codex's `command_execution`, with
/// the exit code its end says (`exit 1`; 0 when it completed without one),
/// every other tool as viy's `tool_call`.
fn tool_item(running: &Running, status: &str, detail: Option<&str>) -> Value {
    if running.tool == "run_shell" {
        let exit_code = detail
            .and_then(|d| d.strip_prefix("exit "))
            .and_then(|code| code.trim().parse::<i32>().ok())
            .or((status == "completed").then_some(0));
        json!({
            "id": running.item_id,
            "type": "command_execution",
            "command": running.summary,
            "aggregated_output": running.output,
            "exit_code": exit_code,
            "status": status,
        })
    } else {
        json!({
            "id": running.item_id,
            "type": "tool_call",
            "tool": running.tool,
            "arguments": running.summary,
            "output": running.output,
            "detail": detail,
            "status": status,
        })
    }
}

#[cfg(test)]
#[path = "exec_json_tests.rs"]
mod tests;
