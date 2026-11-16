//! Selecting in the chat's transcript with the mouse (kb d150, WP 9.13):
//! a drag copies on release, Ctrl+C copies a selection without stopping
//! anything, a question box keeps its keys and is never answered by the
//! mouse.

use std::sync::Arc;
use std::sync::Mutex;

use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use pretty_assertions::assert_eq;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;

use super::*;
use crate::clipboard::Backend;
use crate::clipboard::Clipboard;
use crate::clipboard::Place;
use crate::clipboard::Selection;

const ANSWER: &str = "The keeper counted ships.\nShe mended the lamp.";

/// A clipboard in memory: what was set, in order, and what a read gives.
#[derive(Clone, Default)]
struct Sink {
    log: Arc<Mutex<Vec<String>>>,
    native_fails: bool,
    clipboard: String,
    primary: String,
}

impl Backend for Sink {
    fn set(&mut self, selection: Selection, text: &str) -> Result<(), String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{selection:?} {text}"));
        if self.native_fails {
            Err("no display".into())
        } else {
            Ok(())
        }
    }

    fn get(&mut self, selection: Selection) -> Result<String, String> {
        Ok(match selection {
            Selection::Primary => self.primary.clone(),
            _ => self.clipboard.clone(),
        })
    }

    fn tmux(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

impl Sink {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

/// A chat on a local X11 desktop whose clipboard is `sink`, the answer
/// in its transcript.
fn chat(sink: &Sink, height: u16) -> Fixture {
    let mut f = fixture(40, height);
    let sink = sink.clone();
    let place = Place {
        x11: true,
        ..Place::default()
    };
    f.app
        .set_clipboard(Clipboard::with(place, move || Box::new(sink.clone())));
    f.app.push(Cell::answer(ANSWER, false));
    f.screen();
    f
}

fn mouse(f: &mut Fixture, kind: MouseEventKind, column: u16, row: u16) {
    f.input(Input::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }));
}

/// The screen row that shows `text`.
fn row_of(f: &Fixture, text: &str) -> u16 {
    let rows = f.rows();
    let y = rows
        .iter()
        .position(|r| r.contains(text))
        .unwrap_or_else(|| panic!("{text:?} not on {rows:#?}"));
    u16::try_from(y).unwrap()
}

/// Drags from the start of the answer to the end of its last line.
fn select_answer(f: &mut Fixture) {
    let first = row_of(f, "The keeper");
    let last = row_of(f, "She mended");
    mouse(f, MouseEventKind::Down(MouseButton::Left), 0, first);
    mouse(f, MouseEventKind::Drag(MouseButton::Left), 10, first + 1);
    mouse(f, MouseEventKind::Drag(MouseButton::Left), 19, last);
    mouse(f, MouseEventKind::Up(MouseButton::Left), 19, last);
}

/// The clipboard's answer to the request out, taken in; a test that
/// asked for nothing fails here rather than waiting forever.
async fn finish_copy(f: &mut Fixture) {
    let done = tokio::time::timeout(Duration::from_secs(10), f.app.clipboard.done())
        .await
        .expect("no clipboard request was out");
    f.app.clipboard_done(done);
}

#[tokio::test]
async fn a_drag_over_an_answer_copies_it_and_the_footer_says_so() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    select_answer(&mut f);
    finish_copy(&mut f).await;
    // From a mouse selection: PRIMARY too, on a local X11 desktop.
    assert_eq!(
        sink.log(),
        [format!("Primary {ANSWER}"), format!("Clipboard {ANSWER}")]
    );
    let rows = f.screen();
    let footer = rows.last().unwrap();
    let chars = ANSWER.chars().count();
    assert!(
        footer.contains(&format!("copied {chars} chars")),
        "{footer:?}"
    );
    // The notice goes after a few seconds.
    f.clock = Instant::now() + COPIED_SHOWN;
    let rows = f.screen();
    assert!(!rows.last().unwrap().contains("copied"), "{rows:#?}");
}

#[tokio::test]
async fn ctrl_c_copies_a_selection_and_stops_nothing() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    f.app.event(started("hi"));
    f.ops();
    select_answer(&mut f);
    finish_copy(&mut f).await;
    assert!(f.app.view.has_selection());
    assert_eq!(f.ctrl('c'), None);
    finish_copy(&mut f).await;
    assert_eq!(f.ops(), []);
    assert!(!f.app.view.has_selection());
    assert_eq!(sink.log().last(), Some(&format!("Clipboard {ANSWER}")));
    // Nothing selected: Ctrl+C stops the turn, as before.
    f.ctrl('c');
    assert_eq!(f.ops(), [Op::Interrupt]);
}

#[tokio::test]
async fn esc_drops_a_selection_and_primes_nothing() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    select_answer(&mut f);
    finish_copy(&mut f).await;
    f.key(KeyCode::Esc);
    assert!(!f.app.view.has_selection());
    assert!(!f.app.esc_primed);
    f.key(KeyCode::Esc);
    assert!(f.app.esc_primed);
}

#[tokio::test]
async fn a_question_keeps_its_keys_and_the_mouse_never_answers_it() {
    let sink = Sink::default();
    let mut f = chat(&sink, 20);
    f.app.event(started("hi"));
    f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: "q1".into(),
        question: Question {
            reason: vec![NoticeEvent {
                level: NoticeLevel::Warn,
                code: "approve_auto".into(),
                message: "  wants the network: pip install httpx".into(),
            }],
            ..Question::yes_no_always(Some(QuestionKind::Network), "  run it with network access?")
        },
    }));
    f.ops();
    let rows = f.screen();
    // Clicks and drags on the box's rows answer nothing and select nothing.
    let asked = row_of(&f, "  run it with network access?");
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 2),
        (MouseEventKind::Drag(MouseButton::Left), 20),
        (MouseEventKind::Up(MouseButton::Left), 20),
        (MouseEventKind::Down(MouseButton::Left), 0),
        (MouseEventKind::Up(MouseButton::Left), 0),
    ] {
        mouse(&mut f, kind, column, asked);
    }
    assert_eq!(f.ops(), [], "{rows:#?}");
    assert!(!f.app.view.has_selection());
    // A selection in the transcript: Ctrl+C goes to the box, not the
    // clipboard.
    select_answer(&mut f);
    finish_copy(&mut f).await;
    let copies = sink.log().len();
    f.clock += Duration::from_secs(5);
    f.ctrl('c');
    assert!(!f.app.view.has_selection());
    assert_eq!(sink.log().len(), copies);
    assert_ne!(f.ops(), [], "Ctrl+C reached the box");
}

#[tokio::test]
async fn a_copy_the_desktop_cannot_take_goes_to_the_terminal_with_the_frame() {
    let sink = Sink {
        native_fails: true,
        ..Sink::default()
    };
    let mut f = chat(&sink, 10);
    select_answer(&mut f);
    finish_copy(&mut f).await;
    let rows = f.screen();
    let encoded = base64::engine::general_purpose::STANDARD.encode(ANSWER);
    use base64::Engine;
    assert_eq!(
        f.term.backend().vt100().callbacks().foreign(),
        [format!("clipboard copy \"c\" {encoded:?}")]
    );
    assert!(
        rows.last()
            .unwrap()
            .contains("sent 46 chars to the terminal"),
        "{rows:#?}"
    );
}

#[tokio::test]
async fn ctrl_c_while_the_release_copy_runs_keeps_the_selection() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    select_answer(&mut f);
    // The release's copy has not come back yet.
    assert_eq!(f.ctrl('c'), None);
    assert!(f.app.view.has_selection());
    assert_eq!(f.app.copied, None);
    finish_copy(&mut f).await;
    assert_eq!(sink.log().len(), 2);
    assert!(f.app.copied.as_ref().is_some_and(|(_, failed, _)| !failed));
}

// The input box (WP 9.14).

/// The draft `text` in the input box, drawn.
fn draft(f: &mut Fixture, text: &str) {
    f.app.composer.paste(text);
    f.screen();
}

#[tokio::test]
async fn a_drag_in_the_input_box_selects_and_copies_and_keeps_the_draft() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    draft(&mut f, "please check the lamp");
    let y = row_of(&f, "› please check");
    // `check` is at columns 7..12 of the text, 9..14 on screen after
    // the prompt; the release on the space after it ends the selection.
    mouse(&mut f, MouseEventKind::Down(MouseButton::Left), 9, y);
    mouse(&mut f, MouseEventKind::Drag(MouseButton::Left), 11, y);
    mouse(&mut f, MouseEventKind::Up(MouseButton::Left), 14, y);
    assert_eq!(f.app.composer.selected_text(), Some("check"));
    finish_copy(&mut f).await;
    assert_eq!(sink.log(), ["Primary check", "Clipboard check"]);
    assert_eq!(f.app.composer.text(), "please check the lamp");
    // Ctrl+C copies it again and keeps the draft.
    f.ctrl('c');
    finish_copy(&mut f).await;
    assert_eq!(f.app.composer.text(), "please check the lamp");
    assert_eq!(
        sink.log().last().map(String::as_str),
        Some("Clipboard check")
    );
    assert_eq!(f.ops(), []);
}

#[tokio::test]
async fn ctrl_x_cuts_and_typing_replaces_the_input_selection() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    draft(&mut f, "please check the lamp");
    let y = row_of(&f, "› please check");
    // A double click takes the word.
    for _ in 0..2 {
        mouse(&mut f, MouseEventKind::Down(MouseButton::Left), 12, y);
        mouse(&mut f, MouseEventKind::Up(MouseButton::Left), 12, y);
    }
    assert_eq!(f.app.composer.selected_text(), Some("check"));
    finish_copy(&mut f).await;
    f.ctrl('x');
    finish_copy(&mut f).await;
    assert_eq!(f.app.composer.text(), "please  the lamp");
    assert_eq!(
        sink.log().last().map(String::as_str),
        Some("Clipboard check")
    );
    // A word selected again, then typed over.
    draft(&mut f, "");
    let y = row_of(&f, "› please");
    for _ in 0..2 {
        mouse(&mut f, MouseEventKind::Down(MouseButton::Left), 15, y);
        mouse(&mut f, MouseEventKind::Up(MouseButton::Left), 15, y);
    }
    finish_copy(&mut f).await;
    assert_eq!(f.app.composer.selected_text(), Some("lamp"));
    f.type_text("bell");
    assert_eq!(f.app.composer.text(), "please  the bell");
}

#[tokio::test]
async fn ctrl_v_pastes_the_clipboard_as_a_paste_and_sends_nothing() {
    let sink = Sink {
        clipboard: "!rm -rf build\r\nsecond line".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 10);
    f.ctrl('v');
    finish_copy(&mut f).await;
    // A pasted `!` stays text (kb d149), its newline kept, nothing sent.
    assert_eq!(f.app.composer.text(), "!rm -rf build\nsecond line");
    assert_eq!(f.app.composer.shell(), None);
    assert_eq!(f.ops(), []);
}

#[tokio::test]
async fn a_key_typed_before_the_clipboard_answers_cancels_the_paste() {
    let sink = Sink {
        clipboard: "pasted".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 10);
    f.ctrl('v');
    f.key(KeyCode::Char('a'));
    finish_copy(&mut f).await;
    assert_eq!(f.app.composer.text(), "a");
}

#[tokio::test]
async fn right_click_pastes_the_clipboard_and_middle_click_primary() {
    let sink = Sink {
        clipboard: "from the clipboard".into(),
        primary: " and primary".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 10);
    mouse(&mut f, MouseEventKind::Down(MouseButton::Right), 5, 2);
    finish_copy(&mut f).await;
    mouse(&mut f, MouseEventKind::Down(MouseButton::Middle), 5, 2);
    finish_copy(&mut f).await;
    assert_eq!(f.app.composer.text(), "from the clipboard and primary");
}

#[tokio::test]
async fn a_secret_question_takes_the_clipboards_text() {
    let sink = Sink {
        clipboard: "1234".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 12);
    f.app
        .event(EventMsg::SecretRequest(viy_protocol::SecretRequestEvent {
            question_id: "s1".into(),
            prompt: "  tok PIN (Enter skips):".into(),
        }));
    f.screen();
    f.ctrl('v');
    finish_copy(&mut f).await;
    assert_eq!(f.app.composer.text(), "");
    let rows = f.screen();
    assert!(!rows.iter().any(|r| r.contains("1234")), "{rows:#?}");
}

#[tokio::test]
async fn ctrl_v_and_right_click_never_answer_a_question() {
    let sink = Sink {
        clipboard: "y".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 20);
    f.app.event(started("hi"));
    f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: "q1".into(),
        question: Question::yes_no_always(Some(QuestionKind::Network), "  run it?"),
    }));
    f.screen();
    f.ops();
    f.clock += Duration::from_secs(5);
    f.screen();
    f.ctrl('v');
    finish_copy(&mut f).await;
    mouse(&mut f, MouseEventKind::Down(MouseButton::Right), 5, 5);
    finish_copy(&mut f).await;
    assert_eq!(f.ops(), []);
    // The text went to the input box behind the box, as a paste does.
    assert_eq!(f.app.composer.text(), "yy");
}

#[tokio::test]
async fn a_click_in_a_draft_then_up_and_typing_deletes_nothing() {
    let sink = Sink::default();
    let mut f = chat(&sink, 12);
    draft(&mut f, "first line\nsecond line\nthird line");
    let y = row_of(&f, "  second line");
    mouse(&mut f, MouseEventKind::Down(MouseButton::Left), 5, y);
    mouse(&mut f, MouseEventKind::Up(MouseButton::Left), 5, y);
    assert_eq!(f.app.composer.selected_text(), None);
    f.key(KeyCode::Up);
    f.key(KeyCode::Char('x'));
    assert_eq!(
        f.app.composer.text(),
        "firxst line\nsecond line\nthird line"
    );
}

#[tokio::test]
async fn ctrl_x_while_the_clipboard_is_busy_changes_nothing() {
    let sink = Sink {
        clipboard: "later".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 10);
    draft(&mut f, "please check the lamp");
    // A read is out (a right-click) when the word is selected and cut.
    mouse(&mut f, MouseEventKind::Down(MouseButton::Right), 5, 2);
    let y = row_of(&f, "› please check");
    mouse(&mut f, MouseEventKind::Down(MouseButton::Left), 9, y);
    mouse(&mut f, MouseEventKind::Up(MouseButton::Left), 14, y);
    assert_eq!(f.app.composer.selected_text(), Some("check"));
    f.ctrl('x');
    assert_eq!(f.app.composer.text(), "please check the lamp");
    assert_eq!(f.app.composer.selected_text(), Some("check"));
}

#[tokio::test]
async fn a_release_lost_outside_the_window_does_not_steal_the_next_drag() {
    let sink = Sink::default();
    let mut f = chat(&sink, 10);
    draft(&mut f, "draft text");
    let y = row_of(&f, "› draft");
    mouse(&mut f, MouseEventKind::Down(MouseButton::Left), 3, y);
    // No release: the next press is in the transcript.
    select_answer(&mut f);
    finish_copy(&mut f).await;
    assert_eq!(sink.log().last(), Some(&format!("Clipboard {ANSWER}")));
}

#[tokio::test]
async fn the_mouse_pastes_nothing_into_the_rewind_pick() {
    let sink = Sink {
        clipboard: "pasted".into(),
        ..Sink::default()
    };
    let mut f = chat(&sink, 10);
    f.app.picking = Some(0);
    mouse(&mut f, MouseEventKind::Down(MouseButton::Right), 5, 2);
    mouse(&mut f, MouseEventKind::Down(MouseButton::Middle), 5, 2);
    assert!(!f.app.clipboard.busy());
    f.app.picking = None;
    assert_eq!(f.app.composer.text(), "");
}
