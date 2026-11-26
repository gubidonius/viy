//! By hand, billed (WP 9.50, kb d191, d194): images pasted into the real
//! chat (`viy dev chat`) against a model server that is already up, as a
//! person pastes them: each file dropped as GNOME Terminal drops one (a
//! bracketed paste of its quoted path), then the question and Enter. Prints
//! the screen after each answer; asserts only that each turn ended. The
//! chats are saved under cargo's temporary home, never the real one.
//!
//! ```text
//! VIY_LIVE_URL=http://127.0.0.1:8000 VIY_LIVE_KEY=<the server's key> \
//! VIY_LIVE_MODEL=<served model name> VIY_LIVE_IMAGES=<a folder of
//! chart.png, screen-2048.png, card-A.png .. card-E.png> \
//! cargo test -p viy --test all live_images -- --ignored --nocapture --test-threads=1
//! ```

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use viy_testkit::pty::Pty;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::screen::dump;

const ROWS: u16 = 60;
const COLS: u16 = 160;

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn image(name: &str) -> PathBuf {
    let path = Path::new(&var("VIY_LIVE_IMAGES")).join(name);
    assert!(path.is_file(), "{}", path.display());
    path
}

/// Waits up to `limit` for `done`; false when it never held.
fn wait(pty: &Pty, limit: Duration, done: impl Fn(&str) -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if pty.with_screen(|p| done(&p.screen().contents())) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// `viy dev chat` on the live server, its input box drawn.
fn open(pty: &Pty, dir: &Path) -> viy_testkit::pty::Spawned {
    let child = pty
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_viy"))
                .args(["dev", "chat", "--url", &var("VIY_LIVE_URL"), "--dir"])
                .arg(dir)
                .env("VLLM_API_KEY", var("VIY_LIVE_KEY"))
                .env("SERVED_MODEL_NAME", var("VIY_LIVE_MODEL"))
                .env("MAX_MODEL_LEN", "524288")
                .env("TOOLS_ENABLED", "0")
                .env("CHAT_AUTO_TITLE", "0"),
        )
        .unwrap();
    assert!(wait(pty, Duration::from_secs(30), |s| s.contains("› ")));
    child
}

/// Drops `files`, types `text`, sends it, and waits for the answer; the
/// screen then, and the seconds from Enter to the turn's end.
fn turn(pty: &Pty, files: &[PathBuf], first_image: usize, text: &str) -> (String, f64) {
    for (n, file) in files.iter().enumerate() {
        pty.send(format!("\x1b[200~'{}' \x1b[201~", file.display()).as_bytes())
            .unwrap();
        let label = format!("[Image #{}]", first_image + n);
        assert!(
            wait(pty, Duration::from_secs(10), |s| s.contains(&label)),
            "{label} never showed"
        );
    }
    pty.send(text.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let sent = Instant::now();
    pty.send(b"\r").unwrap();
    assert!(
        wait(pty, Duration::from_secs(120), |s| s.contains("Esc stops")),
        "the turn never started"
    );
    let ended = wait(pty, Duration::from_secs(900), |s| !s.contains("Esc stops"));
    let seconds = sent.elapsed().as_secs_f64();
    let screen = pty.with_screen(|p| dump(p.screen()));
    assert!(ended, "the turn did not end in 15 min:\n{screen}");
    (screen, seconds)
}

fn show(title: &str, (screen, seconds): (String, f64)) {
    println!("===== {title} ({seconds:.1} s from Enter) =====\n{screen}\n");
}

#[test]
#[ignore = "billed: needs a live model server, see the module's doc"]
fn live_images_a_chart_and_a_screenshot() {
    let dir = ScratchDir::new("live-images").unwrap();
    let pty = Pty::open(ROWS, COLS).unwrap();
    let _chat = open(&pty, dir.path());
    show(
        "chart",
        turn(
            &pty,
            &[image("chart.png")],
            1,
            "Read this bar chart: its title, and each quarter's value as Q1=, Q2=, Q3=, Q4=. Answer in one line.",
        ),
    );
    show(
        "screenshot",
        turn(
            &pty,
            &[image("screen-2048.png")],
            1,
            "From this screenshot: the error code, the missing value's name, the file and line, and the ticket number. Answer in one line.",
        ),
    );
}

#[test]
#[ignore = "billed: needs a live model server, see the module's doc"]
fn live_images_five_cards_in_one_chat() {
    let dir = ScratchDir::new("live-cards").unwrap();
    let pty = Pty::open(ROWS, COLS).unwrap();
    let _chat = open(&pty, dir.path());
    for letter in ["A", "B", "C", "D"] {
        show(
            &format!("card {letter}"),
            turn(
                &pty,
                &[image(&format!("card-{letter}.png"))],
                1,
                "What letter is on this card? One word.",
            ),
        );
    }
    show(
        "card E, and every card",
        turn(
            &pty,
            &[image("card-E.png")],
            1,
            "List the letter of every card you can see in this conversation, oldest first, and say which images you cannot see.",
        ),
    );
}
