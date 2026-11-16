//! Whether to capture the mouse: not when tmux has `mouse off`, where
//! capturing would take the wheel from tmux's own scrolling for nothing
//! (Codex's `tui/tmux.rs`). Outside tmux, or when the probe says nothing
//! clear, the mouse is captured. And a copy through tmux's buffer to the
//! attached terminal's clipboard ([`copy`]).

use std::io::Read;
use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// How long tmux may take to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// tmux's `mouse` option for this pane, when running inside tmux and tmux
/// answers within [`PROBE_TIMEOUT`].
pub fn probe() -> Option<String> {
    std::env::var_os("TMUX")?;
    let mut command = Command::new("tmux");
    command.args(["display-message", "-p"]);
    if let Some(pane) = std::env::var_os("TMUX_PANE") {
        command.arg("-t").arg(pane);
    }
    let mut child = command
        .arg("#{mouse}")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout?.take(4096).read_to_string(&mut output).ok()?;
    Some(output)
}

/// Captures the mouse unless tmux's answer says `mouse` is off.
pub fn mouse_capture(tmux_mouse: Option<&str>) -> bool {
    !matches!(tmux_mouse.map(str::trim), Some("0" | "off"))
}

/// Puts `text` in tmux's buffer and has tmux pass it to the clipboard of
/// the client last active in this pane's session (Codex's
/// `clipboard_copy/tmux.rs`): `load-buffer -w` sends it as the terminal
/// expects, no passthrough needed. Fails when tmux's `set-clipboard` is
/// off, no client is attached, or the client's terminal cannot take a
/// clipboard write (no `Ms`). The clipboard's thread runs it
/// ([`crate::clipboard`]); the size limit is checked there.
pub fn copy(text: &str) -> Result<(), String> {
    let pane = std::env::var("TMUX_PANE")
        .map_err(|_| "tmux clipboard forwarding is unavailable: no current pane".to_string())?;
    let client = clipboard_target(
        || output(&["show-options", "-gv", "set-clipboard"]),
        || {
            let session = output(&["display-message", "-p", "-t", &pane, "#{session_id}"])?;
            output(&[
                "list-clients",
                "-t",
                session.trim(),
                "-F",
                "#{client_activity} #{client_name}",
            ])
        },
        |client| output(&["show-messages", "-T", "-t", client]),
    )?;
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-t", &client, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run tmux: {e}"))?;
    let written = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("could not write to tmux: {e}")),
        None => Err("could not write to tmux".to_string()),
    };
    if let Err(e) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("could not wait for tmux: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(failure(&out))
    }
}

/// The client to copy to: the one last active in this session, if tmux
/// forwards clipboard writes and the client's terminal takes them.
fn clipboard_target(
    set_clipboard: impl FnOnce() -> Result<String, String>,
    list_clients: impl FnOnce() -> Result<String, String>,
    terminal_info: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String, String> {
    if set_clipboard()?.trim() == "off" {
        return Err("tmux clipboard forwarding is disabled".to_string());
    }
    // Only this pane's session: an empty list must not pick another one.
    let clients = list_clients()?;
    let client = clients
        .lines()
        .filter_map(|line| {
            let (activity, client) = line.split_once(' ')?;
            Some((activity.parse::<u64>().ok()?, client))
        })
        .filter(|(_, client)| !client.is_empty())
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, client)| client.to_string())
        .ok_or_else(|| {
            "tmux clipboard forwarding is unavailable: no attached client".to_string()
        })?;
    let info = terminal_info(&client)?;
    let takes_clipboard = info.lines().any(|line| {
        line.split_once("Ms: (string) ")
            .is_some_and(|(_, sequence)| !sequence.trim().is_empty())
    });
    if !takes_clipboard {
        return Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string());
    }
    Ok(client)
}

fn output(args: &[&str]) -> Result<String, String> {
    let out = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run tmux: {e}"))?;
    if out.status.success() {
        String::from_utf8(out.stdout).map_err(|e| format!("tmux answered in bad UTF-8: {e}"))
    } else {
        Err(failure(&out))
    }
}

fn failure(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("tmux exited with {}", out.status)
    } else {
        format!("tmux failed: {stderr}")
    }
}

#[cfg(test)]
#[path = "tmux_tests.rs"]
mod tests;
