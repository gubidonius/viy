use std::cell::RefCell;
use std::sync::Arc;
use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::*;

const LOCAL: Place = Place {
    tmux: false,
    ssh: false,
    x11: true,
    wayland: false,
};

/// Which backends `deliver` called, in order.
fn route(
    place: Place,
    native: Result<(), String>,
    tmux: Result<(), String>,
    osc52: Result<(), String>,
) -> (Result<Delivery, String>, Vec<&'static str>) {
    let calls = RefCell::new(Vec::new());
    let result = deliver(
        "hello",
        place,
        native,
        |_| {
            calls.borrow_mut().push("tmux");
            tmux.clone()
        },
        |_| {
            calls.borrow_mut().push("osc52");
            osc52.clone()
        },
    );
    (result, calls.into_inner())
}

#[test]
fn a_local_copy_goes_to_the_desktop_and_nowhere_else() {
    assert_eq!(
        route(LOCAL, Ok(()), Ok(()), Ok(())),
        (Ok(Delivery::Copied), vec![])
    );
}

#[test]
fn a_failed_desktop_copy_falls_back_to_osc52_unconfirmed() {
    assert_eq!(
        route(LOCAL, Err("no display".into()), Ok(()), Ok(())),
        (Ok(Delivery::Sent), vec!["osc52"])
    );
    assert_eq!(
        route(
            LOCAL,
            Err("no display".into()),
            Ok(()),
            Err("too large".into())
        ),
        (
            Err("no display; terminal: too large".to_string()),
            vec!["osc52"]
        )
    );
}

#[test]
fn over_ssh_the_terminal_is_asked_too() {
    let ssh = Place { ssh: true, ..LOCAL };
    assert_eq!(
        route(ssh, Ok(()), Ok(()), Ok(())),
        (Ok(Delivery::Copied), vec!["osc52"])
    );
    assert_eq!(
        route(ssh, Err("no display".into()), Ok(()), Ok(())),
        (Ok(Delivery::Sent), vec!["osc52"])
    );
}

#[test]
fn inside_tmux_the_buffer_is_tried_before_osc52() {
    let tmux = Place {
        tmux: true,
        ..LOCAL
    };
    assert_eq!(
        route(tmux, Ok(()), Ok(()), Ok(())),
        (Ok(Delivery::Copied), vec!["tmux"])
    );
    assert_eq!(
        route(tmux, Ok(()), Err("off".into()), Ok(())),
        (Ok(Delivery::Copied), vec!["tmux", "osc52"])
    );
    assert_eq!(
        route(
            tmux,
            Err("no display".into()),
            Err("off".into()),
            Err("blocked".into())
        ),
        (
            Err("no display; terminal: tmux: off; OSC 52: blocked".to_string()),
            vec!["tmux", "osc52"]
        )
    );
}

#[test]
fn text_too_large_for_the_terminal_never_reaches_tmux() {
    let big = "x".repeat(TERMINAL_MAX + 1);
    let tmux_called = RefCell::new(false);
    let result = deliver(
        &big,
        Place {
            tmux: true,
            ..LOCAL
        },
        Ok(()),
        |_| {
            *tmux_called.borrow_mut() = true;
            Ok(())
        },
        |t| osc52(t, true).map(|_| ()),
    );
    assert_eq!(result, Ok(Delivery::Copied));
    assert!(!*tmux_called.borrow());
    assert_eq!(
        osc52(&big, false),
        Err(format!(
            "too large for the terminal ({} bytes; at most {TERMINAL_MAX})",
            TERMINAL_MAX + 1
        ))
    );
}

#[test]
fn osc52_carries_the_text_in_base64_and_reaches_a_terminal_clipboard() {
    assert_eq!(
        osc52("hello", false),
        Ok("\x1b]52;c;aGVsbG8=\x07".to_string())
    );
    assert_eq!(
        osc52("hello", true),
        Ok("\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\".to_string())
    );
    // What a terminal makes of it: one clipboard write of the text.
    let text = "line one\nline two ␛[2J";
    let mut parser = viy_testkit::screen::parser(5, 40);
    let sequence = osc52(text, false).unwrap();
    viy_testkit::screen::feed(&mut parser, sequence.as_bytes());
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    assert_eq!(
        parser.callbacks().foreign(),
        [format!("clipboard copy \"c\" {encoded:?}")]
    );
}

#[test]
fn primary_is_published_on_a_local_x11_display_only() {
    assert!(LOCAL.primary());
    assert!(
        !Place {
            tmux: true,
            ..LOCAL
        }
        .primary()
    );
    assert!(!Place { ssh: true, ..LOCAL }.primary());
    assert!(
        !Place {
            wayland: true,
            ..LOCAL
        }
        .primary()
    );
    assert!(
        !Place {
            x11: false,
            ..LOCAL
        }
        .primary()
    );
}

/// A clipboard in memory: what was set, and a gate that holds every call
/// until the test lets it go.
#[derive(Clone, Default)]
struct Fake {
    log: Arc<Mutex<Vec<String>>>,
    native_fails: bool,
    held: Option<Arc<Mutex<std_mpsc::Receiver<()>>>>,
}

impl Fake {
    fn wait(&self) {
        if let Some(held) = &self.held {
            let _ = held.lock().unwrap().recv();
        }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Backend for Fake {
    fn set(&mut self, selection: Selection, text: &str) -> Result<(), String> {
        self.wait();
        self.log
            .lock()
            .unwrap()
            .push(format!("set {selection:?} {text}"));
        if self.native_fails {
            Err("no display".into())
        } else {
            Ok(())
        }
    }

    fn get(&mut self, selection: Selection) -> Result<String, String> {
        self.wait();
        self.log.lock().unwrap().push(format!("get {selection:?}"));
        Ok("pasted\ntext".into())
    }

    fn tmux(&mut self, text: &str) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("tmux {text}"));
        Ok(())
    }
}

fn clipboard(place: Place, fake: &Fake) -> Clipboard {
    let fake = fake.clone();
    Clipboard::with(place, move || Box::new(fake.clone()))
}

#[tokio::test]
async fn a_copy_comes_back_with_what_the_footer_says() {
    let fake = Fake::default();
    let mut clip = clipboard(LOCAL, &fake);
    let id = clip.copy("héllo", false).unwrap();
    assert!(clip.busy());
    let done = clip.done().await;
    assert_eq!(
        done,
        Done::Copy {
            id,
            chars: 5,
            result: Ok(Delivery::Copied),
            terminal: None,
        }
    );
    assert_eq!(done.notice().as_deref(), Some("copied 5 chars"));
    assert!(!clip.busy());
    assert_eq!(fake.log(), ["set Clipboard héllo"]);
}

#[tokio::test]
async fn a_selection_is_also_published_as_primary() {
    let fake = Fake::default();
    let mut clip = clipboard(LOCAL, &fake);
    clip.copy("picked", true).unwrap();
    clip.done().await;
    assert_eq!(fake.log(), ["set Primary picked", "set Clipboard picked"]);
    // Inside tmux PRIMARY is left alone, and tmux gets the text.
    let fake = Fake::default();
    let mut clip = clipboard(
        Place {
            tmux: true,
            ..LOCAL
        },
        &fake,
    );
    clip.copy("picked", true).unwrap();
    clip.done().await;
    assert_eq!(fake.log(), ["set Clipboard picked", "tmux picked"]);
}

#[tokio::test]
async fn without_a_desktop_clipboard_the_ui_gets_the_osc52_to_write() {
    let fake = Fake {
        native_fails: true,
        ..Fake::default()
    };
    let mut clip = clipboard(LOCAL, &fake);
    let id = clip.copy("hi", false).unwrap();
    let done = clip.done().await;
    assert_eq!(
        done,
        Done::Copy {
            id,
            chars: 2,
            result: Ok(Delivery::Sent),
            terminal: Some("\x1b]52;c;aGk=\x07".to_string()),
        }
    );
    assert_eq!(
        done.notice().as_deref(),
        Some("sent 2 chars to the terminal")
    );
}

#[tokio::test]
async fn nothing_is_written_for_an_empty_copy_and_one_request_at_a_time() {
    let (release, held) = std_mpsc::channel();
    let fake = Fake {
        held: Some(Arc::new(Mutex::new(held))),
        ..Fake::default()
    };
    let mut clip = clipboard(LOCAL, &fake);
    assert_eq!(clip.copy("", true), Err(Refused::Empty));
    let id = clip.copy("one", false).unwrap();
    assert_eq!(clip.copy("two", false), Err(Refused::Busy));
    assert_eq!(clip.read(Selection::Clipboard), Err(Refused::Busy));
    release.send(()).unwrap();
    assert_eq!(clip.done().await.id(), id);
    assert_eq!(fake.log(), ["set Clipboard one"]);
}

#[tokio::test]
async fn a_read_brings_the_text_but_never_over_ssh() {
    let fake = Fake::default();
    let mut clip = clipboard(LOCAL, &fake);
    let id = clip.read(Selection::Clipboard).unwrap();
    assert_eq!(
        clip.done().await,
        Done::Read {
            id,
            result: Ok("pasted\ntext".into())
        }
    );
    let mut remote = clipboard(Place { ssh: true, ..LOCAL }, &fake);
    assert_eq!(remote.read(Selection::Clipboard), Err(Refused::Remote));
    assert_eq!(fake.log(), ["get Clipboard"]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_stuck_clipboard_is_given_up_on_after_five_seconds() {
    let (release, held) = std_mpsc::channel();
    let fake = Fake {
        held: Some(Arc::new(Mutex::new(held))),
        ..Fake::default()
    };
    let mut clip = clipboard(LOCAL, &fake);
    let started = Instant::now();
    let id = clip.copy("stuck", false).unwrap();
    let done = clip.done().await;
    assert_eq!(started.elapsed(), BUDGET);
    assert_eq!(
        done,
        Done::Copy {
            id,
            chars: 5,
            result: Err("the clipboard did not answer in 5 s".into()),
            terminal: None,
        }
    );
    // The stuck thread is left behind: the next copy goes to a new one.
    assert!(!clip.busy());
    let next = clip.copy("next", false).unwrap();
    // Both threads wait on the gate. Which one it lets through first is not
    // fixed: the 5 s pass at once on the paused clock, maybe before the stuck
    // thread reached the gate.
    release.send(()).unwrap();
    release.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // The stuck thread's late answer is dropped; the new one's comes.
    assert_eq!(clip.done().await.id(), next);
    let mut log = fake.log();
    log.sort();
    assert_eq!(log, ["set Clipboard next", "set Clipboard stuck"]);
}

#[test]
fn closing_never_waits_long_for_a_stuck_clipboard() {
    let (_release, held) = std_mpsc::channel::<()>();
    let fake = Fake {
        held: Some(Arc::new(Mutex::new(held))),
        ..Fake::default()
    };
    let mut clip = clipboard(LOCAL, &fake);
    clip.copy("stuck", false).unwrap();
    let started = std::time::Instant::now();
    drop(clip);
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// By hand, on an X11 desktop: the real backend writes SECONDARY (which
/// nothing uses, so the desktop's clipboard is untouched) and `xclip`
/// reads it back while the backend still owns it.
#[test]
#[ignore = "needs an X11 display and xclip"]
fn the_desktop_clipboard_is_reached_through_x11() {
    let mut x11 = X11::default();
    let text = "viy clipboard check: ünïcödé and\ntwo lines";
    x11.set(Selection::Secondary, text).unwrap();
    assert_eq!(x11.get(Selection::Secondary).unwrap(), text);
    let out = std::process::Command::new("xclip")
        .args(["-o", "-selection", "secondary"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), text);
}
