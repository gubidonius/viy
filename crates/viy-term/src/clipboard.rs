//! The chat's clipboard (kb d150), Codex's `clipboard_copy` and its worker
//! (`clipboard_copy/{worker,tmux,primary}.rs`) cut to Linux and X11.
//!
//! A copy goes to the desktop's clipboard first, through arboard, whose
//! `Clipboard` is kept for the chat's life: on X11 the owner serves the
//! text, and the copy is gone once it closes. Inside tmux the text also
//! goes to tmux's buffer (`load-buffer -w`), which hands it to the
//! attached terminal; over ssh, or when the desktop's clipboard fails, to
//! the terminal as OSC 52. A terminal write is not acknowledged (GNOME
//! Terminal ignores OSC 52, kb f147), so it counts as sent, not copied. A
//! copy from a mouse selection is also published as X11's PRIMARY for
//! middle-click, on a local X11 display only.
//!
//! The desktop's clipboard can block. One thread does every clipboard
//! call; the UI never waits for it longer than [`BUDGET`], and while the
//! thread is stuck a new request is refused as busy. The OSC 52 sequence
//! comes back to the UI, which writes it with its frames: only the
//! renderer writes escape sequences (ui-plan 7), and this one is built
//! here from constants, the text in base64.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use base64::Engine;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep_until;

/// How long the UI waits for one clipboard call (Codex's `SETUP_TIMEOUT`).
pub const BUDGET: Duration = Duration::from_secs(5);
/// The most text sent to tmux or as OSC 52 (Codex's `OSC52_MAX_RAW_BYTES`).
pub const TERMINAL_MAX: usize = 100_000;
/// How long closing the chat waits for the clipboard's owner to hand the
/// text to a clipboard manager (Codex's worker drop).
const HANDOFF: Duration = Duration::from_millis(250);

/// Where viy runs, as far as the clipboard cares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Place {
    pub tmux: bool,
    pub ssh: bool,
    /// An X11 display (`DISPLAY`).
    pub x11: bool,
    /// A Wayland display (`WAYLAND_DISPLAY`).
    pub wayland: bool,
}

impl Place {
    pub fn here() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some();
        Self {
            tmux: set("TMUX") || set("TMUX_PANE"),
            ssh: set("SSH_TTY") || set("SSH_CONNECTION"),
            x11: set("DISPLAY"),
            wayland: set("WAYLAND_DISPLAY"),
        }
    }

    /// PRIMARY is published on a local X11 display only, never over ssh or
    /// tmux, nor to a Wayland compositor (Codex's `primary::available`).
    pub fn primary(self) -> bool {
        self.x11 && !self.wayland && !self.ssh && !self.tmux
    }
}

/// Which of X11's selections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Clipboard,
    /// What middle-click pastes.
    Primary,
    /// Rarely used by anything: the by-hand check writes here, so the
    /// desktop's clipboard is left alone.
    Secondary,
}

/// How a copy left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The desktop's clipboard has it.
    Copied,
    /// Only asked of the terminal or tmux, which do not say whether it
    /// arrived.
    Sent,
}

/// The clipboard calls the worker makes: arboard, or a test's fake.
pub trait Backend: Send + 'static {
    /// Puts `text` in `selection`, keeping whatever must stay alive to
    /// serve it.
    fn set(&mut self, selection: Selection, text: &str) -> Result<(), String>;
    /// The text in `selection`; empty when it holds none.
    fn get(&mut self, selection: Selection) -> Result<String, String>;
    /// Hands `text` to tmux's buffer and the attached terminal.
    fn tmux(&mut self, text: &str) -> Result<(), String>;
}

/// The desktop's clipboard through arboard, one `Clipboard` kept for the
/// chat's life.
#[derive(Default)]
pub struct X11 {
    clipboard: Option<arboard::Clipboard>,
}

impl X11 {
    fn clipboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.clipboard.is_none() {
            let clipboard =
                arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
            self.clipboard = Some(clipboard);
        }
        self.clipboard
            .as_mut()
            .ok_or_else(|| "clipboard unavailable".to_string())
    }
}

fn kind(selection: Selection) -> arboard::LinuxClipboardKind {
    match selection {
        Selection::Clipboard => arboard::LinuxClipboardKind::Clipboard,
        Selection::Primary => arboard::LinuxClipboardKind::Primary,
        Selection::Secondary => arboard::LinuxClipboardKind::Secondary,
    }
}

impl Backend for X11 {
    fn set(&mut self, selection: Selection, text: &str) -> Result<(), String> {
        use arboard::SetExtLinux;
        self.clipboard()?
            .set()
            .clipboard(kind(selection))
            .text(text)
            .map_err(|e| format!("could not set the clipboard: {e}"))
    }

    fn get(&mut self, selection: Selection) -> Result<String, String> {
        use arboard::GetExtLinux;
        match self.clipboard()?.get().clipboard(kind(selection)).text() {
            Ok(text) => Ok(text),
            Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
            Err(e) => Err(format!("could not read the clipboard: {e}")),
        }
    }

    fn tmux(&mut self, text: &str) -> Result<(), String> {
        crate::tmux::copy(text)
    }
}

/// Routes one copy (Codex's `copy_to_clipboard_with`): the desktop's
/// clipboard (`native`, already tried) first, then the terminal inside
/// tmux or over ssh whatever the desktop said (tmux may have clients
/// elsewhere), or when the desktop failed. `osc52` takes the text the UI
/// should send as OSC 52.
fn deliver(
    text: &str,
    place: Place,
    native: Result<(), String>,
    mut tmux: impl FnMut(&str) -> Result<(), String>,
    mut osc52: impl FnMut(&str) -> Result<(), String>,
) -> Result<Delivery, String> {
    let mut terminal = || {
        if place.tmux {
            let tmuxed = if text.len() > TERMINAL_MAX {
                Err(too_large(text))
            } else {
                tmux(text)
            };
            tmuxed.or_else(|tmux_error| {
                osc52(text).map_err(|osc_error| format!("tmux: {tmux_error}; OSC 52: {osc_error}"))
            })
        } else {
            osc52(text)
        }
    };
    let sent = (place.tmux || place.ssh).then(&mut terminal);
    match native {
        Ok(()) => Ok(Delivery::Copied),
        Err(native_error) => sent
            .unwrap_or_else(terminal)
            .map(|()| Delivery::Sent)
            .map_err(|terminal_error| format!("{native_error}; terminal: {terminal_error}")),
    }
}

fn too_large(text: &str) -> String {
    format!(
        "too large for the terminal ({} bytes; at most {TERMINAL_MAX})",
        text.len()
    )
}

/// The OSC 52 sequence that asks the terminal to put `text` in its
/// clipboard, wrapped for tmux's passthrough inside tmux (Codex's
/// `osc52_sequence`).
pub fn osc52(text: &str, tmux: bool) -> Result<String, String> {
    if text.len() > TERMINAL_MAX {
        return Err(too_large(text));
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Ok(if tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    })
}

/// What the UI asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    Copy,
    Read,
}

/// A request done, or given up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Copy {
        id: u64,
        /// The chars copied.
        chars: usize,
        result: Result<Delivery, String>,
        /// The OSC 52 sequence for the UI to write, if the copy went to
        /// the terminal.
        terminal: Option<String>,
    },
    Read {
        id: u64,
        result: Result<String, String>,
    },
}

impl Done {
    pub fn id(&self) -> u64 {
        match self {
            Self::Copy { id, .. } | Self::Read { id, .. } => *id,
        }
    }

    /// What the footer says of a copy.
    pub fn notice(&self) -> Option<String> {
        let Self::Copy { chars, result, .. } = self else {
            return None;
        };
        let what = if *chars == 1 {
            "1 char".to_string()
        } else {
            format!("{chars} chars")
        };
        Some(match result {
            Ok(Delivery::Copied) => format!("copied {what}"),
            Ok(Delivery::Sent) => format!("sent {what} to the terminal's clipboard"),
            Err(error) => format!("not copied: {error}"),
        })
    }
}

/// Why a request was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Nothing selected.
    Empty,
    /// The last request has not come back.
    Busy,
    /// Reading the clipboard over ssh would read the far machine's.
    Remote,
    /// The worker could not start, or stopped.
    Stopped(String),
}

enum Request {
    Copy {
        id: u64,
        text: String,
        primary: bool,
    },
    Read {
        id: u64,
        selection: Selection,
    },
}

/// The request the worker is on.
#[derive(Debug, Clone, Copy)]
struct Pending {
    id: u64,
    ask: Ask,
    chars: usize,
    deadline: Instant,
    /// Given up on: its answer is dropped when it comes.
    expired: bool,
}

type Make = Box<dyn FnOnce() -> Box<dyn Backend> + Send>;

/// The UI's side: requests go to the clipboard's thread, answers come
/// back through [`Clipboard::done`].
pub struct Clipboard {
    place: Place,
    make: Option<Make>,
    requests: Option<std_mpsc::Sender<Request>>,
    answers_tx: mpsc::UnboundedSender<Done>,
    answers: mpsc::UnboundedReceiver<Done>,
    /// Disconnected once the thread has dropped its backend.
    finished: Option<std_mpsc::Receiver<()>>,
    next: u64,
    pending: Option<Pending>,
}

impl Clipboard {
    /// The desktop's clipboard, for where viy runs now.
    pub fn new() -> Self {
        Self::with(Place::here(), || Box::new(X11::default()))
    }

    /// `make` builds the backend on the clipboard's thread, at the first
    /// request.
    pub fn with(place: Place, make: impl FnOnce() -> Box<dyn Backend> + Send + 'static) -> Self {
        let (answers_tx, answers) = mpsc::unbounded_channel();
        Self {
            place,
            make: Some(Box::new(make)),
            requests: None,
            answers_tx,
            answers,
            finished: None,
            next: 0,
            pending: None,
        }
    }

    pub fn place(&self) -> Place {
        self.place
    }

    /// Whether a request is out.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Copies `text`; `selected` (a mouse selection) also publishes it as
    /// PRIMARY where [`Place::primary`] allows. Nothing is written for
    /// empty text.
    pub fn copy(&mut self, text: &str, selected: bool) -> Result<u64, Refused> {
        if text.is_empty() {
            return Err(Refused::Empty);
        }
        let chars = text.chars().count();
        let primary = selected && self.place.primary();
        self.send(Ask::Copy, chars, |id| Request::Copy {
            id,
            text: text.to_string(),
            primary,
        })
    }

    /// Reads the text in `selection` (Ctrl+V, middle-click). Never over
    /// ssh: the display there is the far machine's (Codex's reader).
    pub fn read(&mut self, selection: Selection) -> Result<u64, Refused> {
        if self.place.ssh {
            return Err(Refused::Remote);
        }
        self.send(Ask::Read, 0, |id| Request::Read { id, selection })
    }

    fn send(
        &mut self,
        ask: Ask,
        chars: usize,
        request: impl FnOnce(u64) -> Request,
    ) -> Result<u64, Refused> {
        if self.pending.is_some() {
            return Err(Refused::Busy);
        }
        self.start()?;
        self.next += 1;
        let id = self.next;
        self.requests
            .as_ref()
            .ok_or_else(|| Refused::Stopped("the clipboard stopped".into()))?
            .send(request(id))
            .map_err(|_| Refused::Stopped("the clipboard stopped".into()))?;
        self.pending = Some(Pending {
            id,
            ask,
            chars,
            deadline: Instant::now() + BUDGET,
            expired: false,
        });
        Ok(id)
    }

    fn start(&mut self) -> Result<(), Refused> {
        let Some(make) = self.make.take() else {
            return Ok(());
        };
        let (requests, incoming) = std_mpsc::channel::<Request>();
        let (finished_tx, finished) = std_mpsc::channel::<()>();
        let answers = self.answers_tx.clone();
        let place = self.place;
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut backend = make();
                while let Ok(request) = incoming.recv() {
                    let done = match request {
                        Request::Copy { id, text, primary } => {
                            // PRIMARY first, as Codex: a failed PRIMARY
                            // never stops the copy.
                            if primary {
                                let _ = backend.set(Selection::Primary, &text);
                            }
                            let native = backend.set(Selection::Clipboard, &text);
                            let mut terminal = None;
                            let result = deliver(
                                &text,
                                place,
                                native,
                                |t| backend.tmux(t),
                                |t| {
                                    terminal = Some(osc52(t, place.tmux)?);
                                    Ok(())
                                },
                            );
                            Done::Copy {
                                id,
                                chars: text.chars().count(),
                                result,
                                terminal,
                            }
                        }
                        Request::Read { id, selection } => Done::Read {
                            id,
                            result: backend.get(selection),
                        },
                    };
                    if answers.send(done).is_err() {
                        break;
                    }
                }
                drop(backend);
                drop(finished_tx);
            })
            .map_err(|e| Refused::Stopped(format!("could not start the clipboard: {e}")))?;
        self.requests = Some(requests);
        self.finished = Some(finished);
        Ok(())
    }

    /// The next answer, or a request given up on after [`BUDGET`].
    /// Cancel-safe: it changes nothing until it returns.
    pub async fn done(&mut self) -> Done {
        loop {
            let deadline = self.pending.filter(|p| !p.expired).map(|p| p.deadline);
            tokio::select! {
                answer = self.answers.recv() => {
                    // A sender is kept here, so the channel never closes.
                    let Some(answer) = answer else { continue };
                    let Some(pending) = self.pending.filter(|p| p.id == answer.id()) else {
                        continue;
                    };
                    self.pending = None;
                    if !pending.expired {
                        return answer;
                    }
                }
                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    if let Some(pending) = &mut self.pending {
                        pending.expired = true;
                        return timed_out(*pending);
                    }
                }
            }
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

fn timed_out(pending: Pending) -> Done {
    let error = "the clipboard did not answer in 5 s".to_string();
    match pending.ask {
        Ask::Copy => Done::Copy {
            id: pending.id,
            chars: pending.chars,
            result: Err(error),
            terminal: None,
        },
        Ask::Read => Done::Read {
            id: pending.id,
            result: Err(error),
        },
    }
}

impl Drop for Clipboard {
    /// Lets the thread drop the desktop's `Clipboard`, which on X11 hands
    /// the text to a clipboard manager if there is one, for at most
    /// [`HANDOFF`]: even that can stall on an X server.
    fn drop(&mut self) {
        self.requests.take();
        if let Some(finished) = &self.finished {
            let _ = finished.recv_timeout(HANDOFF);
        }
    }
}

#[cfg(test)]
#[path = "clipboard_tests.rs"]
mod tests;
