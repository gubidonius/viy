//! `viy chat --json`: a chat another program drives (WP 9.32, kb d180).
//! Its stdout is `viy exec --json`'s lines ([`crate::exec_json`]), one
//! turn after another; its stdin is the caller's lines, one JSON object
//! each:
//!
//! ```text
//! {"type":"user","text":"..."}          a turn
//! {"type":"command","text":"/compact"}  a slash command
//! {"type":"answer","question_id":"q1","answer":"yes"}
//! {"type":"interrupt"}                  stops the turn running as it is read
//! {"type":"shutdown"}                   as the end of input
//! ```
//!
//! Turns and commands go in one at a time, in order: the next once the
//! last has ended. At the end of input the queue is finished, the chat is
//! saved and viy exits.
//!
//! A safeguard's question takes its unattended answer (no for every
//! approval), as in `viy exec --json`, unless the chat was started with
//! `--answer-questions` (Gregory, 2026-08-08: "Only with a flag"): then
//! the chat mode (CHAT_MODE) answers first, as for the person at the
//! terminal, and a question the reviewer and the mode leave to the user is
//! a `question.requested` line, with the answers it takes, and waits for
//! the caller's `answer`; at the end of input every waiting question takes
//! its unattended answer. A secret is never taken over the pipe. A line
//! that is not taken is an `error` line, and the chat goes on.

use std::collections::VecDeque;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use viy_agent::Engine;
use viy_agent::chat::ChatTurns;
use viy_config::Config;
use viy_config::Paths;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_protocol::EventMsg;
use viy_protocol::Op;

use crate::Failure;
use crate::cli::ChatArgs;
use crate::dev;
use crate::exec::base_url;
use crate::exec_json::JsonEvents;
use crate::exec_json::parse_answer;

/// One line from the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A turn or a slash command: goes in when the last one has ended.
    Submit(Op),
    Answer {
        question_id: String,
        answer: String,
    },
    Interrupt,
    /// The end of the caller's input.
    End,
}

/// The caller's line as an [`Input`], or why it is not one.
pub fn parse(line: &str) -> Result<Input, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    let kind = field("type").ok_or("no \"type\"")?;
    match kind {
        "user" => {
            let text = field("text").ok_or("a user line needs \"text\"")?;
            if text.trim().is_empty() {
                return Err("a user line needs some text".into());
            }
            Ok(Input::Submit(Op::UserTurn {
                text: text.into(),
                images: Vec::new(),
            }))
        }
        "command" => {
            let text = field("text").ok_or("a command line needs \"text\"")?;
            if !text.starts_with('/') {
                return Err(format!("a command starts with /, not {text:?}"));
            }
            Ok(Input::Submit(Op::Command { text: text.into() }))
        }
        "answer" => Ok(Input::Answer {
            question_id: field("question_id")
                .ok_or("an answer needs \"question_id\"")?
                .into(),
            answer: field("answer").ok_or("an answer needs \"answer\"")?.into(),
        }),
        "interrupt" => Ok(Input::Interrupt),
        "shutdown" => Ok(Input::End),
        other => Err(format!("unknown type {other:?}")),
    }
}

/// Runs `viy chat --json` with the caller's lines from `input`.
pub fn cmd_chat_json(
    args: &ChatArgs,
    paths: &Paths,
    cfg: Config,
    start: &Path,
    input: Box<dyn BufRead + Send>,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    runtime.block_on(serve(args, paths, cfg, start, lines(input), out, err))
}

/// The chat on `input`'s lines, its lines on `out`.
pub async fn serve<W: Write>(
    args: &ChatArgs,
    paths: &Paths,
    cfg: Config,
    start: &Path,
    input: mpsc::UnboundedReceiver<Result<Input, String>>,
    out: W,
    err: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    // Advisory lines stay off the JSON stdout.
    let url = base_url(&cfg, args.url.as_deref(), paths, &http, &mut *err).await?;
    let mut chat = dev::chat(&cfg, &url, Arc::clone(&http), paths, start, true)?;
    if let Some(system) = &args.system {
        chat.set_system(Some(system.clone()));
    }
    chat.end_with("chat");
    chat.skip_tool_probe();
    let thread_id = chat.session_id();
    if let Some(id) = &thread_id {
        let _ = writeln!(err, "session id: {id}");
    }
    let turns = Arc::new(ChatTurns::new(chat));
    // With a caller to answer, the chat mode answers first, as for the
    // person at the terminal; without one, nobody is there (no for every
    // approval), as in `viy exec`.
    let mut engine = if args.answer_questions {
        Engine::spawn_with(turns, dev::chat_mode(&cfg), true)
    } else {
        Engine::spawn(turns)
    };
    let mut events = JsonEvents::new(out);
    if args.answer_questions {
        events = events.caller_answers();
    }
    let result = run(&mut engine, &mut events, thread_id.as_deref(), input).await;
    if result.is_err() {
        // The output broke: still end the chat, so it is saved and let go.
        shut_down(&mut engine).await;
    }
    result
}

/// The caller's lines, parsed, on a thread of their own; [`Input::End`]
/// when they end.
pub fn lines(input: Box<dyn BufRead + Send>) -> mpsc::UnboundedReceiver<Result<Input, String>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for (n, line) in input.lines().enumerate() {
            let parsed = match line {
                // A line that is not UTF-8 is refused; any other read error
                // ends the input, or it would repeat for ever.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    Err(format!("line {}: not UTF-8", n + 1))
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("stdin: {e}")));
                    break;
                }
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => parse(&line).map_err(|why| format!("line {}: {why}", n + 1)),
            };
            let end = parsed == Ok(Input::End);
            if tx.send(parsed).is_err() || end {
                return;
            }
        }
        let _ = tx.send(Ok(Input::End));
    });
    rx
}

/// The chat: the caller's submissions one at a time, the engine's events
/// as lines, until the input has ended and the queue is done.
async fn run<W: Write>(
    engine: &mut Engine,
    events: &mut JsonEvents<W>,
    thread_id: Option<&str>,
    mut input: mpsc::UnboundedReceiver<Result<Input, String>>,
) -> Result<(), Failure> {
    let failed = |e: std::io::Error| Failure::from(e.to_string());
    events.thread_started(thread_id).map_err(failed)?;
    let mut queue: VecDeque<Op> = VecDeque::new();
    let mut current: Option<String> = None;
    let mut ended = false;
    let mut shutting_down = false;
    loop {
        if current.is_none() && !shutting_down {
            if let Some(op) = queue.pop_front() {
                match engine.submit(op) {
                    Ok(id) => current = Some(id),
                    Err(_) => break,
                }
            } else if ended {
                shutting_down = engine.submit(Op::Shutdown).is_ok();
                if !shutting_down {
                    break;
                }
            }
        }
        tokio::select! {
            event = engine.next_event() => {
                let Some(event) = event else { break };
                if current.as_deref() == Some(event.id.as_str())
                    && matches!(
                        event.msg,
                        EventMsg::TurnComplete(_)
                            | EventMsg::TurnAborted(_)
                            | EventMsg::TurnRefused(_)
                            | EventMsg::CommandComplete(_)
                    )
                {
                    current = None;
                }
                let done = matches!(event.msg, EventMsg::ShutdownComplete);
                if let Some(op) = events.event(&event.msg).map_err(failed)? {
                    let _ = engine.submit(op);
                }
                if ended {
                    answer_unattended(engine, events).map_err(failed)?;
                }
                if done {
                    break;
                }
            }
            line = input.recv(), if !ended => {
                match line.unwrap_or(Ok(Input::End)) {
                    Err(why) => events.refused(&why).map_err(failed)?,
                    Ok(Input::Submit(op)) => queue.push_back(op),
                    // Stops whatever turn runs as the line is read.
                    Ok(Input::Interrupt) if current.is_some() => {
                        let _ = engine.submit(Op::Interrupt);
                    }
                    Ok(Input::Interrupt) => {
                        events.refused("no turn is running").map_err(failed)?;
                    }
                    Ok(Input::Answer { question_id, answer }) => {
                        let Some(answer) = parse_answer(&answer) else {
                            events
                                .refused(&format!("{answer:?} is not an answer"))
                                .map_err(failed)?;
                            continue;
                        };
                        match events.answer(&question_id, Some(answer), "caller").map_err(failed)? {
                            Ok(op) => {
                                let _ = engine.submit(op);
                            }
                            Err(why) => events.refused(&why).map_err(failed)?,
                        }
                    }
                    Ok(Input::End) => {
                        ended = true;
                        answer_unattended(engine, events).map_err(failed)?;
                    }
                }
            }
        }
    }
    events.flush().map_err(failed)
}

/// Ends the engine without writing anything: after the output failed.
async fn shut_down(engine: &mut Engine) {
    if engine.submit(Op::Shutdown).is_err() {
        return;
    }
    let drain = async {
        while let Some(event) = engine.next_event().await {
            match &event.msg {
                EventMsg::ShutdownComplete => return,
                EventMsg::QuestionRequest(e) => {
                    let _ = engine.submit(Op::QuestionAnswer {
                        id: e.question_id.clone(),
                        answer: e.question.unattended,
                    });
                }
                EventMsg::SecretRequest(e) => {
                    let _ = engine.submit(Op::SecretAnswer {
                        id: e.question_id.clone(),
                        secret: None,
                    });
                }
                _ => {}
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), drain).await;
}

/// Nobody is left to answer: every waiting question takes its unattended
/// answer.
fn answer_unattended<W: Write>(
    engine: &mut Engine,
    events: &mut JsonEvents<W>,
) -> std::io::Result<()> {
    for id in events.waiting() {
        // The unattended answer is always taken (`JsonEvents::answer`).
        if let Ok(op) = events.answer(&id, None, "unattended")? {
            let _ = engine.submit(op);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "chat_json_tests.rs"]
mod tests;
