use super::*;

#[test]
fn the_mouse_is_captured_unless_tmux_says_off() {
    assert!(mouse_capture(None));
    assert!(mouse_capture(Some("1\n")));
    assert!(mouse_capture(Some("on")));
    assert!(mouse_capture(Some("")));
    assert!(!mouse_capture(Some("0\n")));
    assert!(!mouse_capture(Some("off")));
}

#[test]
fn a_copy_goes_to_the_last_active_client_that_takes_the_clipboard() {
    let target = clipboard_target(
        || Ok("external\n".to_string()),
        || Ok("42 /dev/pts/7\n1 /dev/pts/8\n".to_string()),
        |client| {
            assert_eq!(client, "/dev/pts/7");
            Ok("193: Ms: (string) \\033]52;%p1%s;%p2%s\\a\n".to_string())
        },
    );
    assert_eq!(target, Ok("/dev/pts/7".to_string()));
}

#[test]
fn no_copy_through_tmux_when_it_forwards_none() {
    let off = clipboard_target(
        || Ok("off\n".to_string()),
        || panic!("no client is asked for when forwarding is off"),
        |_| panic!("no terminal is asked about when forwarding is off"),
    );
    assert_eq!(
        off,
        Err("tmux clipboard forwarding is disabled".to_string())
    );
    let nobody = clipboard_target(
        || Ok("external\n".to_string()),
        || Ok("\n".to_string()),
        |_| panic!("no client to ask about"),
    );
    assert_eq!(
        nobody,
        Err("tmux clipboard forwarding is unavailable: no attached client".to_string())
    );
    for info in ["", "193: Ms: [missing]\n", "193: Ms: (string) \n"] {
        let cannot = clipboard_target(
            || Ok("external\n".to_string()),
            || Ok("42 /dev/pts/7\n".to_string()),
            |_| Ok(info.to_string()),
        );
        assert_eq!(
            cannot,
            Err("tmux clipboard forwarding is unavailable: missing Ms capability".to_string()),
            "{info:?}"
        );
    }
}

/// By hand, inside a tmux with a client attached: the text reaches tmux's
/// buffer through `load-buffer -w`.
#[test]
#[ignore = "needs to run inside tmux with a client attached"]
fn a_copy_reaches_the_real_tmux_buffer() {
    let text = "viy tmux copy check: ünïcödé\nsecond line";
    copy(text).unwrap();
    let out = Command::new("tmux").arg("show-buffer").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), text);
}
