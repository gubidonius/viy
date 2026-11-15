//! The chat's side of `!` and `!!` ([`crate::shell`], kb d149): what runs
//! the command, asks first when it runs a file the model may have written,
//! and keeps a `!` command's record for the next message.

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use viy_config::redact::Redactor;
use viy_protocol::Answer;
use viy_protocol::EventMsg;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_protocol::Question;
use viy_protocol::ToolCallOutputDeltaEvent;
use viy_protocol::ToolCallStatus;
use viy_tools::untrusted;

use super::Chat;
use super::notice;
use crate::ShellEnd;
use crate::TurnContext;
use crate::files;
use crate::shell;

/// How many of the files a question names.
const NAMED_FILES: usize = 3;
/// How long the login shell may take to say its `PATH`.
const PATH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Runs `argv` for the user (kb d149): its own session and group, stdin
/// /dev/null, the output drained for [`shell::IO_DRAIN`] once it ends.
async fn run_user_command(
    argv: &[OsString],
    ws: &Path,
    env: Vec<(OsString, OsString)>,
    timeout: std::time::Duration,
) -> std::io::Result<viy_exec::sandboxed::Finished> {
    let running = viy_exec::sandboxed::spawn(viy_exec::sandboxed::Spec {
        program: argv[0].clone(),
        args: argv[1..].to_vec(),
        cwd: ws.to_path_buf(),
        env,
        pass_fds: Vec::new(),
    })?;
    Ok(running.finish_draining(timeout, shell::IO_DRAIN).await)
}

impl Chat {
    /// Runs `command` for the user in the workspace, outside the sandbox;
    /// with `model_sees`, its record goes to the model with the next
    /// message.
    pub async fn user_shell(
        &mut self,
        ctx: &TurnContext,
        call_id: &str,
        command: &str,
        model_sees: bool,
    ) -> ShellEnd {
        let ws = self.places.workspace.clone();
        let paths = self.command_paths().await;
        let files = shell::run_files(command, &ws, &self.places.home, &paths);
        if let Some((doubted, unknown)) = self.doubted(&files)
            && !self
                .run_written_anyway(ctx, &ws, &doubted, unknown.as_deref())
                .await
        {
            return ShellEnd {
                status: ToolCallStatus::Declined,
                detail: Some("declined".into()),
            };
        }
        let argv = shell::argv(command, self.login_shell.as_deref());
        let env: Vec<_> = std::env::vars_os().collect();
        let began = Instant::now();
        let ran = match run_user_command(&argv, &ws, env, shell::TIMEOUT).await {
            Ok(r) => shell::Ran {
                exit_code: r.exit_code,
                timed_out: r.timed_out,
                duration: began.elapsed(),
                stdout: r.stdout,
                stderr: r.stderr,
            },
            Err(e) => {
                notice(
                    ctx,
                    NoticeLevel::Error,
                    "user_shell",
                    format!("  could not start {}: {e}", argv[0].to_string_lossy()),
                );
                return ShellEnd {
                    status: ToolCallStatus::Failed,
                    detail: Some("failed".into()),
                };
            }
        };
        let output = ran.output();
        let shown = untrusted::clean(&files::clip(&output, shell::SHOWN_CHARS));
        if !shown.trim().is_empty() {
            ctx.emit(EventMsg::ToolCallOutputDelta(ToolCallOutputDeltaEvent {
                call_id: call_id.to_string(),
                chunk: shown.trim_end().to_string(),
            }));
        }
        self.tracer.event(
            "user_shell",
            serde_json::json!({"model_sees": model_sees, "exit_code": ran.exit_code,
                               "timed_out": ran.timed_out}),
        );
        if model_sees {
            self.keep_record(ctx, command, &ran);
        }
        let ok = ran.exit_code == Some(0) && !ran.timed_out;
        ShellEnd {
            status: if ok {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            },
            detail: Some(ran.detail()),
        }
    }

    /// The record the next message carries: stdout and stderr each cut as
    /// run_shell's are, secrets masked, chat-control tokens defused, fenced.
    fn keep_record(&mut self, ctx: &TurnContext, command: &str, ran: &shell::Ran) {
        let limit = self.files.max_output;
        let clipped = shell::Ran {
            stdout: files::clip(&ran.stdout, limit),
            stderr: files::clip(&ran.stderr, limit),
            ..ran.clone()
        };
        let output = clipped.output();
        let masked = Redactor::from_config(&self.cfg).text(&output);
        if masked != output {
            notice(
                ctx,
                NoticeLevel::Info,
                "user_shell",
                "  what looked like secrets in the output was masked for the model",
            );
        }
        let (defused, _) = untrusted::neutralize(&masked);
        let Ok(nonce) = untrusted::nonce() else {
            notice(
                ctx,
                NoticeLevel::Error,
                "user_shell",
                "  no randomness for the untrusted fence: the model does not see this output",
            );
            return;
        };
        self.shell_records
            .push(shell::record(command, ran, &defused, &nonce));
    }

    /// viy's `PATH` and the login shell's, asked once per chat: a profile
    /// may put folders on it that viy's lacks, and the command runs there.
    async fn command_paths(&mut self) -> Vec<OsString> {
        let mut paths = vec![std::env::var_os("PATH").unwrap_or_default()];
        if self.login_path.is_none() {
            let argv = shell::argv("printf %s \"$PATH\"", self.login_shell.as_deref());
            let env = std::env::vars_os().collect();
            let got = run_user_command(&argv, &self.places.workspace, env, PATH_WAIT).await;
            self.login_path = Some(
                got.ok()
                    .filter(|r| r.exit_code == Some(0))
                    .map(|r| OsString::from(r.stdout))
                    .unwrap_or_default(),
            );
        }
        if let Some(login) = self.login_path.as_ref().filter(|p| !p.is_empty()) {
            paths.push(login.clone());
        }
        paths
    }

    /// Of `files`, the ones to ask about, and why when it is not that they
    /// changed while the model was at work: where the log of its work could
    /// not be written or read, viy cannot tell, and asks about every one in
    /// the workspace (the safe side).
    fn doubted(&self, files: &[PathBuf]) -> Option<(Vec<PathBuf>, Option<String>)> {
        let windows = if self.work_log_failed {
            Err("this chat could not log when the model works".to_string())
        } else {
            self.work_log
                .windows()
                .map_err(|e| format!("the log of when the model works cannot be read ({e})"))
        };
        let (doubted, unknown): (Vec<PathBuf>, _) = match windows {
            Ok(windows) => (
                shell::written_in(files, &windows)
                    .into_iter()
                    .map(|(p, _)| p)
                    .collect(),
                None,
            ),
            Err(why) => (
                files
                    .iter()
                    .filter(|f| {
                        f.starts_with(&self.places.workspace)
                            || f.starts_with(&self.places.project_dir)
                    })
                    .cloned()
                    .collect(),
                Some(why),
            ),
        };
        (!doubted.is_empty()).then_some((doubted, unknown))
    }

    /// Whether the user wants a command that runs `files` to go ahead:
    /// files that changed while the model was at work (or, with `unknown`
    /// saying why, that may have), which run outside the sandbox, as the
    /// user, if it does. Never answered by a chat mode; no with nobody to
    /// ask.
    async fn run_written_anyway(
        &self,
        ctx: &TurnContext,
        ws: &Path,
        files: &[PathBuf],
        unknown: Option<&str>,
    ) -> bool {
        let names: Vec<String> = files
            .iter()
            .take(NAMED_FILES)
            .map(|p| {
                p.strip_prefix(ws)
                    .map_or_else(|_| p.display().to_string(), |r| r.display().to_string())
            })
            .collect();
        let more = files.len().saturating_sub(NAMED_FILES);
        let mut listed = names.join(", ");
        if more > 0 {
            listed.push_str(&format!(" and {more} more"));
        }
        let (it, was) = if files.len() == 1 {
            ("it", "was")
        } else {
            ("them", "were")
        };
        let message = match unknown {
            None => format!(
                "  this runs {listed}, which {was} written or changed while the model was at \
                 work: {it} would run outside the sandbox, as you"
            ),
            Some(why) => format!(
                "  this runs {listed}, and {why}, so viy cannot tell whether the model wrote \
                 {it}: {it} would run outside the sandbox, as you"
            ),
        };
        let question = Question {
            reason: vec![NoticeEvent {
                level: NoticeLevel::Warn,
                code: "user_shell".into(),
                message,
            }],
            ..Question::yes_no(None, "  run it anyway?")
        };
        ctx.ask(&question).await == Answer::Yes
    }

    /// Says once that the model's work could not be logged: `!` cannot
    /// tell what it wrote from then on.
    pub(crate) fn work_log_unwritten(&mut self, ctx: &TurnContext, e: &std::io::Error) {
        if !std::mem::replace(&mut self.work_log_failed, true) {
            notice(
                ctx,
                NoticeLevel::Warn,
                "work_log",
                format!(
                    "  could not log when the model works ({e}): a ! command will not know \
                     which files it wrote"
                ),
            );
        }
    }
}

#[cfg(test)]
#[path = "user_shell_tests.rs"]
mod tests;
