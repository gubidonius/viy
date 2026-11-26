//! Images from the paste sources (kb d191, WP 9.48): Ctrl+V's file list
//! and picture, a dropped file's path. Only the input box attaches: a
//! question box or the rewind pick in front, a secret taking the paste, or
//! a shell command, never.

use std::path::Path;
use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;
use crate::clipboard::Backend;
use crate::clipboard::Clipboard;
use crate::clipboard::Picture;
use crate::clipboard::Place;
use crate::clipboard::Selection;

/// A clipboard holding `files`, a `picture` and `text`.
#[derive(Clone, Default)]
struct Pictures {
    files: Vec<PathBuf>,
    picture: Option<Picture>,
    text: String,
}

impl Backend for Pictures {
    fn set(&mut self, _: Selection, _: &str) -> Result<(), String> {
        Ok(())
    }

    fn get(&mut self, _: Selection) -> Result<String, String> {
        Ok(self.text.clone())
    }

    fn tmux(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }

    fn files(&mut self) -> Result<Vec<PathBuf>, String> {
        Ok(self.files.clone())
    }

    fn picture(&mut self) -> Result<Option<Picture>, String> {
        Ok(self.picture.clone())
    }
}

/// A red pixel.
fn red() -> Picture {
    Picture {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    }
}

fn red_url() -> String {
    crate::image_paste::png_data_url(1, 1, red().rgba).unwrap()
}

/// A PNG file in `dir`.
fn png_in(dir: &Path, name: &str) -> PathBuf {
    use base64::Engine;
    let path = dir.join(name);
    let url = red_url();
    let encoded = url.strip_prefix("data:image/png;base64,").unwrap();
    std::fs::write(
        &path,
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap(),
    )
    .unwrap();
    path
}

fn chat(clipboard: &Pictures) -> Fixture {
    let mut f = fixture(50, 12);
    let clipboard = clipboard.clone();
    let place = Place {
        x11: true,
        ..Place::default()
    };
    f.app
        .set_clipboard(Clipboard::with(place, move || Box::new(clipboard.clone())));
    f.screen();
    f
}

/// The clipboard's answer to the request out, taken in.
async fn answered(f: &mut Fixture) {
    let done = tokio::time::timeout(Duration::from_secs(10), f.app.clipboard.done())
        .await
        .expect("no clipboard request was out");
    f.app.clipboard_done(done);
}

fn footer_says(f: &Fixture) -> Option<String> {
    f.app.copied.as_ref().map(|(said, ..)| said.clone())
}

#[tokio::test]
async fn ctrl_v_attaches_the_clipboards_picture_and_enter_sends_it() {
    let mut f = chat(&Pictures {
        picture: Some(red()),
        text: "the text beside it".into(),
        ..Pictures::default()
    });
    f.type_text("what is ");
    f.ctrl('v');
    answered(&mut f).await;
    f.type_text(" here");
    assert!(
        f.screen()
            .iter()
            .any(|row| row.contains("› what is [Image #1] here")),
        "{:#?}",
        f.rows()
    );
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "what is [Image #1] here".into(),
            images: vec![ImageInput::DataUrl { url: red_url() }],
        }]
    );
}

#[tokio::test]
async fn ctrl_v_attaches_every_image_file_the_clipboard_lists() {
    let dir = viy_testkit::scratch::ScratchDir::new("app-image-files").unwrap();
    let (a, b) = (png_in(dir.path(), "a.png"), png_in(dir.path(), "b.png"));
    let mut f = chat(&Pictures {
        files: vec![a.clone(), b.clone()],
        picture: Some(red()),
        ..Pictures::default()
    });
    f.ctrl('v');
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "[Image #1] [Image #2]");
    assert_eq!(
        f.app.composer.images(),
        [ImageInput::File { path: a }, ImageInput::File { path: b }]
    );
}

/// GNOME Terminal and tmux drop a file as a bracketed paste of its path:
/// each image attaches, a space after it, and typing goes on there.
#[tokio::test]
async fn a_dropped_image_file_attaches_and_typing_goes_on_after_it() {
    let dir = viy_testkit::scratch::ScratchDir::new("app-image-drop").unwrap();
    let shot = png_in(dir.path(), "my shot.png");
    let mut f = chat(&Pictures::default());
    f.type_text("see ");
    f.input(Input::Paste(format!("'{}' ", shot.display())));
    f.type_text("now");
    assert_eq!(f.app.composer.text(), "see [Image #1] now");
    f.key(KeyCode::Enter);
    assert_eq!(
        f.ops(),
        [Op::UserTurn {
            text: "see [Image #1] now".into(),
            images: vec![ImageInput::File { path: shot }],
        }]
    );
}

/// The hostile pastes: a path in a larger paste, a path that is no image,
/// are text, as before.
#[tokio::test]
async fn a_paste_that_is_more_than_image_paths_is_text() {
    let dir = viy_testkit::scratch::ScratchDir::new("app-image-text").unwrap();
    let shot = png_in(dir.path(), "shot.png");
    let notes = dir.path().join("notes.png");
    std::fs::write(&notes, "not a picture").unwrap();
    let mut f = chat(&Pictures::default());
    let pasted = format!("look at {}\n{}", shot.display(), notes.display());
    f.input(Input::Paste(pasted.clone()));
    assert_eq!(f.app.composer.text(), pasted);
    assert_eq!(f.app.composer.images(), []);
}

/// The question box: a yes/no box in front, its question asked.
fn ask(f: &mut Fixture) {
    f.app.event(started("hi"));
    f.app.event(EventMsg::QuestionRequest(QuestionRequestEvent {
        question_id: "q1".into(),
        question: Question::yes_no_always(Some(QuestionKind::Network), "  run it?"),
    }));
    f.screen();
    f.ops();
    f.clock += Duration::from_secs(5);
    f.screen();
}

/// A question box in front: nothing attaches, nothing answers it. Ctrl+V
/// reads the text only, as before images, and a dropped path is text; both
/// go into the input box behind the box, as any paste does (F37).
#[tokio::test]
async fn behind_a_question_box_nothing_attaches_and_nothing_is_answered() {
    let dir = viy_testkit::scratch::ScratchDir::new("app-image-question").unwrap();
    let shot = png_in(dir.path(), "shot.png");
    let mut f = chat(&Pictures {
        picture: Some(red()),
        text: "y".into(),
        ..Pictures::default()
    });
    ask(&mut f);
    f.ctrl('v');
    answered(&mut f).await;
    f.input(Input::Paste(format!(" {}", shot.display())));
    assert_eq!(f.app.composer.text(), format!("y {}", shot.display()));
    assert_eq!(f.app.composer.images(), []);
    assert_eq!(f.ops(), []);
}

/// Something came in front while the clipboard was read: the images do not
/// attach, the text beside them is pasted instead, and with none the footer
/// says why nothing was.
#[tokio::test]
async fn images_that_can_no_longer_attach_paste_the_text_beside_them() {
    for (text, footer) in [
        ("y", None),
        ("", Some("not pasted: a question is open".to_string())),
    ] {
        let mut f = chat(&Pictures {
            picture: Some(red()),
            text: text.into(),
            ..Pictures::default()
        });
        f.ctrl('v');
        ask(&mut f);
        answered(&mut f).await;
        assert_eq!(f.app.composer.text(), text);
        assert_eq!(f.app.composer.images(), []);
        assert_eq!(footer_says(&f), footer);
        assert_eq!(f.ops(), []);
    }
}

/// A secret in front takes the paste, as any paste: a dropped path as
/// typed text, and Ctrl+V's text (read alone); no image reaches it, the
/// input box or the screen.
#[tokio::test]
async fn a_secret_question_takes_the_text_and_no_image() {
    let dir = viy_testkit::scratch::ScratchDir::new("app-image-secret").unwrap();
    let shot = png_in(dir.path(), "shot.png");
    let mut f = chat(&Pictures {
        picture: Some(red()),
        text: "1234".into(),
        ..Pictures::default()
    });
    f.app
        .event(EventMsg::SecretRequest(viy_protocol::SecretRequestEvent {
            question_id: "s1".into(),
            prompt: "  tok PIN (Enter skips):".into(),
        }));
    f.screen();
    f.input(Input::Paste(shot.display().to_string()));
    f.ctrl('v');
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "");
    assert_eq!(f.app.composer.images(), []);
    assert_eq!(footer_says(&f), None);
    let rows = f.screen();
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("shot.png") || r.contains("1234")),
        "{rows:#?}"
    );
}

/// A shell command takes no image: a dropped path is text in it, the path
/// the command may want, and Ctrl+V brings the clipboard's text only.
#[tokio::test]
async fn a_shell_command_takes_a_dropped_path_as_text() {
    let dir = viy_testkit::scratch::ScratchDir::new("app-image-shell").unwrap();
    let shot = png_in(dir.path(), "shot.png");
    let mut f = chat(&Pictures {
        picture: Some(red()),
        text: " | head".into(),
        ..Pictures::default()
    });
    f.key(KeyCode::Char('!'));
    f.type_text("file ");
    f.input(Input::Paste(shot.display().to_string()));
    f.ctrl('v');
    answered(&mut f).await;
    assert_eq!(
        f.app.composer.text(),
        format!("file {} | head", shot.display())
    );
    assert_eq!(f.app.composer.images(), []);
}

/// Codex's repeat guard: a second Ctrl+V within 250 ms and no other key
/// between is the first repeating, and neither cancels nor repeats its
/// paste; after another key it pastes again.
#[tokio::test]
async fn ctrl_v_held_down_pastes_once() {
    let mut f = chat(&Pictures {
        picture: Some(red()),
        ..Pictures::default()
    });
    f.ctrl('v');
    f.ctrl('v');
    f.ctrl('v');
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "[Image #1]");
    assert_eq!(footer_says(&f), None);
    // A release ends the window too.
    f.input(Input::Key(KeyEvent {
        kind: KeyEventKind::Release,
        ..KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
    }));
    f.ctrl('v');
    answered(&mut f).await;
    f.key(KeyCode::Char(' '));
    f.ctrl('v');
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "[Image #1][Image #2] [Image #3]");
}

/// A Ctrl+V while its paste is still out, however long after, is the
/// same one repeating: the paste goes on, and nothing says the clipboard
/// is busy.
#[tokio::test]
async fn ctrl_v_while_its_paste_runs_is_a_repeat() {
    let mut f = chat(&Pictures {
        picture: Some(red()),
        ..Pictures::default()
    });
    f.ctrl('v');
    f.clock += Duration::from_millis(400);
    f.ctrl('v');
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "[Image #1]");
    assert_eq!(footer_says(&f), None);
}

/// A second right-click while the first one's read is out is refused, and
/// the first paste still comes.
#[tokio::test]
async fn a_click_while_the_clipboard_reads_keeps_the_paste_out() {
    let mut f = chat(&Pictures {
        text: "once".into(),
        ..Pictures::default()
    });
    let right = || {
        Input::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
    };
    f.input(right());
    f.input(right());
    assert_eq!(
        footer_says(&f).as_deref(),
        Some("not pasted: the clipboard is busy")
    );
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "once");
}

/// Right-click and middle-click read the text only, the picture beside
/// it left alone.
#[tokio::test]
async fn right_click_pastes_the_text_not_the_picture() {
    let mut f = chat(&Pictures {
        picture: Some(red()),
        text: "just words".into(),
        ..Pictures::default()
    });
    f.input(Input::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
        column: 5,
        row: 2,
        modifiers: KeyModifiers::NONE,
    }));
    answered(&mut f).await;
    assert_eq!(f.app.composer.text(), "just words");
    assert_eq!(f.app.composer.images(), []);
}
