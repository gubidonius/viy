//! `viy exec` (alias `e`): viy run non-interactively, as Codex's `codex
//! exec` (kb d083; rp's `cmd_ask` before it). One prompt, its answer on
//! stdout through the plain renderer. The chat is saved, marked one-shot so
//! the resume pickers leave it out unless asked, and its id said on stderr,
//! so a script piping the answer gets only the answer; `--ephemeral` keeps
//! nothing. `viy exec resume` goes on in a saved chat, `viy exec fork` in a
//! copy of one. `--json` prints the run as JSON lines instead
//! ([`crate::exec_json`]).

use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use viy_agent::Engine;
use viy_agent::chat::Chat;
use viy_agent::chat::ChatTurns;
use viy_agent::sessions;
use viy_agent::sessions::SessionError;
use viy_agent::sessions::Sessions;
use viy_config::Config;
use viy_config::Paths;
use viy_config::Project;
use viy_exec::SystemExec;
use viy_http::HttpClient;
use viy_http::ReqwestHttp;
use viy_pod::backend::Backend;
use viy_pod::commands::Env;
use viy_pod::notice::Report;
use viy_protocol::EventMsg;
use viy_protocol::Op;
use viy_term::plain::Plain;
use viy_time::SystemTimeProvider;

use crate::Failure;
use crate::cli::ExecArgs;
use crate::cli::ExecCommand;
use crate::dev;
use crate::exec_json::JsonEvents;
use crate::sessions_cli::find;

/// The chat the prompt goes to.
enum Target<'a> {
    New,
    /// A saved chat by id or name, or the most recent with `last`.
    Resume {
        session: Option<&'a str>,
        last: bool,
        all: bool,
    },
    /// A copy of a saved chat.
    Fork(&'a str),
}

/// Runs `viy exec`.
pub fn cmd_exec(
    args: &ExecArgs,
    paths: &Paths,
    cfg: Config,
    start: &Path,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let (target, prompt) = match &args.command {
        None => (Target::New, args.prompt.as_deref()),
        Some(ExecCommand::Resume(r)) => {
            let (session, prompt) = r.split();
            let target = Target::Resume {
                session,
                last: r.last,
                all: r.all,
            };
            (target, prompt)
        }
        Some(ExecCommand::Fork(f)) => (Target::Fork(&f.session_id), f.prompt.as_deref()),
    };
    if let Target::Resume {
        session: None,
        last: false,
        ..
    } = target
    {
        return Err("viy exec resume: give a SESSION_ID, or --last".into());
    }
    let prompt = match prompt {
        Some(prompt) if prompt != "-" => prompt.to_string(),
        _ => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| Failure::from(format!("stdin: {e}")))?;
            text
        }
    };
    let saved = !args.ephemeral;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::from(e.to_string()))?;
    let http: Arc<dyn HttpClient> =
        Arc::new(ReqwestHttp::new().map_err(|e| Failure::from(e.to_string()))?);
    runtime.block_on(async {
        // Advisory lines stay off a JSON stdout.
        let advisory: &mut (dyn Write + Send) = if args.json { &mut *err } else { &mut *out };
        let url = base_url(&cfg, args.url.as_deref(), paths, &http, advisory).await?;
        let mut chat = dev::chat(&cfg, &url, Arc::clone(&http), paths, start, saved)?;
        let store = Sessions::new(paths.sessions_dir(), Arc::new(SystemTimeProvider));
        match target {
            Target::New => chat.mark_exec(),
            Target::Resume { session, all, .. } => {
                let id = match session {
                    Some(session) => find(&store, session, "")?.id,
                    None => last(&store, start, all)?,
                };
                go_on(&mut chat, &store, &id, saved)?;
            }
            Target::Fork(session) => {
                let id = find(&store, session, "")?.id;
                if saved {
                    let (twin, st) = store
                        .copy(&id, chat.redactor(), true)
                        .map_err(|e| Failure::from(e.to_string()))?;
                    chat.continue_in(twin, st);
                } else {
                    chat.load_history(replayed(&store, &id)?);
                }
            }
        }
        if let Some(system) = &args.system {
            chat.set_system(Some(system.clone()));
        }
        chat.end_with("exec");
        chat.skip_tool_probe();
        if let Some(id) = chat.session_id().filter(|_| saved) {
            let _ = writeln!(err, "session id: {id}");
        }
        let thread_id = chat.session_id().filter(|_| saved);
        let turns = Arc::new(ChatTurns::new(chat));
        let mut engine = Engine::spawn(turns);
        if args.json {
            return run_json(&mut engine, prompt, thread_id.as_deref(), out).await;
        }
        let mut plain = Plain::new(&mut *out).show_think(args.think);
        let failed = |e: std::io::Error| Failure::from(e.to_string());
        let turn = engine
            .submit(Op::UserTurn { text: prompt })
            .map_err(|e| Failure::from(e.to_string()))?;
        while let Some(event) = engine.next_event().await {
            match &event.msg {
                EventMsg::ShutdownComplete => break,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::TurnRefused(_)
                    if event.id == turn =>
                {
                    let _ = engine.submit(Op::Shutdown);
                }
                // A chat's opening line; nothing a script wants.
                EventMsg::Notice(n) if n.code == "context_length" => continue,
                _ => {}
            }
            if let Some(op) = plain.event(&event.msg).map_err(failed)? {
                let _ = engine.submit(op);
            }
        }
        let out = plain.into_inner();
        out.flush().map_err(|e| Failure::from(e.to_string()))
    })
}

/// The turn with its events as JSON lines on `out` (`--json`).
async fn run_json(
    engine: &mut Engine,
    prompt: String,
    thread_id: Option<&str>,
    out: &mut (dyn Write + Send),
) -> Result<(), Failure> {
    let failed = |e: std::io::Error| Failure::from(e.to_string());
    let mut events = JsonEvents::new(&mut *out);
    events.thread_started(thread_id).map_err(failed)?;
    let turn = engine
        .submit(Op::UserTurn { text: prompt })
        .map_err(|e| Failure::from(e.to_string()))?;
    let mut ok = true;
    while let Some(event) = engine.next_event().await {
        match &event.msg {
            EventMsg::ShutdownComplete => break,
            EventMsg::TurnComplete(e) if event.id == turn => {
                ok = !matches!(e.outcome, viy_protocol::TurnOutcome::Failed { .. });
                let _ = engine.submit(Op::Shutdown);
            }
            EventMsg::TurnAborted(_) | EventMsg::TurnRefused(_) if event.id == turn => {
                ok = false;
                let _ = engine.submit(Op::Shutdown);
            }
            _ => {}
        }
        if let Some(op) = events.event(&event.msg).map_err(failed)? {
            let _ = engine.submit(op);
        }
    }
    events.into_inner().flush().map_err(failed)?;
    // The lines said why; a script checking the status needs no more.
    if ok { Ok(()) } else { Err(Failure::silent(1)) }
}

/// The most recent saved chat, one-shot ones included, of this project or
/// of every project.
fn last(store: &Sessions, start: &Path, all: bool) -> Result<String, Failure> {
    let project = Project::find(start).map_err(|e| Failure::from(e.to_string()))?;
    let root = project.root().display().to_string();
    let scope = (!all).then_some(root.as_str());
    store
        .list(scope)
        .into_iter()
        .next()
        .map(|row| row.id)
        .ok_or_else(|| Failure::from("No saved session found to resume."))
}

/// Goes on in saved chat `id`: writing to it, or only reading it.
fn go_on(chat: &mut Chat, store: &Sessions, id: &str, saved: bool) -> Result<(), Failure> {
    if !saved {
        chat.load_history(replayed(store, id)?);
        return Ok(());
    }
    let (mut log, st) = store.open(id).map_err(|e| match e {
        SessionError::Busy(_) => {
            Failure::from(format!("session {id} is open in a viy chat; quit it first"))
        }
        SessionError::Io(e) => Failure::from(e.to_string()),
    })?;
    log.set_redactor(chat.redactor());
    chat.continue_in(log, st);
    Ok(())
}

fn replayed(store: &Sessions, id: &str) -> Result<sessions::State, Failure> {
    sessions::replay(&store.path_of(id)).map_err(|e| Failure::from(e.to_string()))
}

/// Where the model answers (rp's `resolve_base_url`): `--url`, else
/// VLLM_BASE_URL, else the backend's; its advisory lines first.
pub(crate) async fn base_url(
    cfg: &Config,
    url: Option<&str>,
    paths: &Paths,
    http: &Arc<dyn HttpClient>,
    out: &mut (dyn Write + Send),
) -> Result<String, Failure> {
    if let Some(url) = url {
        return Ok(url.to_string());
    }
    if let Ok(url) = std::env::var("VLLM_BASE_URL")
        && !url.is_empty()
    {
        return Ok(url);
    }
    let backend = Backend::resolve(cfg)?;
    let exe = std::env::current_exe().map_err(|e| Failure::from(format!("viy itself: {e}")))?;
    let env = Env {
        paths,
        exec: &SystemExec,
        http,
        time: &SystemTimeProvider,
        exe: &exe,
    };
    let warn = |report: Report| {
        if let Report::Notice(notice) = report {
            eprintln!("{}", notice.text);
        }
    };
    let found = backend
        .base_url(&env, cfg, true, &warn)
        .await
        .map_err(|e| Failure::from(e.to_string()))?;
    let Some(url) = found else {
        return Err(Failure::from(match backend {
            Backend::Openai => "ENDPOINT_URL is not set (BACKEND=openai).".to_string(),
            Backend::Runpod => "No pod is running. Start one with:  viy up".to_string(),
            Backend::Local => "No server is running. Start one with:  viy up".to_string(),
        }));
    };
    for line in backend.preflight(&env, cfg).await.unwrap_or_default() {
        let _ = writeln!(out, "{line}");
    }
    Ok(url)
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
