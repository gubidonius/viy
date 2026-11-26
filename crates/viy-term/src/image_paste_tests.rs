use std::path::Path;
use std::path::PathBuf;

use base64::Engine;
use pretty_assertions::assert_eq;
use viy_testkit::scratch::ScratchDir;

use super::*;

/// A small synthetic picture in `format`.
fn picture(format: image::ImageFormat) -> Vec<u8> {
    let img = image::RgbImage::from_fn(6, 4, |x, y| image::Rgb([x as u8 * 40, y as u8 * 60, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

fn paths(list: &[&str]) -> Option<Vec<PathBuf>> {
    Some(list.iter().map(PathBuf::from).collect())
}

// Codex's `pasted_paths_tests`, without the Windows paths.

#[test]
fn a_file_url_is_its_path() {
    assert_eq!(
        pasted_paths("file:///tmp/example.png"),
        paths(&["/tmp/example.png"])
    );
    assert_eq!(
        pasted_paths("file:///tmp/My%20File.png"),
        paths(&["/tmp/My File.png"])
    );
    assert_eq!(
        pasted_paths("file://localhost/tmp/a.png"),
        paths(&["/tmp/a.png"])
    );
    // Another machine's file is no path here.
    assert_eq!(pasted_paths("file://far.example/tmp/a.png"), None);
}

#[test]
fn shell_quoting_and_escapes_are_undone() {
    assert_eq!(
        pasted_paths("/home/user/My\\ File.png"),
        paths(&["/home/user/My File.png"])
    );
    assert_eq!(
        pasted_paths("\"/home/user/My File.png\""),
        paths(&["/home/user/My File.png"])
    );
    assert_eq!(
        pasted_paths("'/home/user/My File.png'"),
        paths(&["/home/user/My File.png"])
    );
}

/// GNOME Terminal drops several files as one paste, each quoted, a space
/// after each; others put a URL on each line.
#[test]
fn several_dropped_files_are_several_paths() {
    assert_eq!(
        pasted_paths("'/tmp/a b.png' '/tmp/c.png' "),
        paths(&["/tmp/a b.png", "/tmp/c.png"])
    );
    assert_eq!(
        pasted_paths("file:///tmp/a.png\r\nfile:///tmp/b.png\r\n"),
        paths(&["/tmp/a.png", "/tmp/b.png"])
    );
}

#[test]
fn what_is_not_a_list_of_paths_is_text() {
    for text in [
        "",
        "x",
        "   ",
        // Quotes left open.
        "'/tmp/a.png",
        // The shell's comment: the words after it would be lost.
        "# /tmp/a.png",
        "/tmp/a.png # why is it red?",
        "# Bug\n/tmp/a.png",
    ] {
        assert_eq!(pasted_paths(text), None, "{text:?}");
    }
    let long = format!("/tmp/{}.png", "a".repeat(MAX_PATHS_CHARS));
    assert_eq!(pasted_paths(&long), None);
}

/// The hostile cases (WP 9.48): a path that is not an image, a path inside
/// a larger paste, things that are not regular files. Each is text.
#[test]
fn only_image_files_attach_and_only_when_nothing_else_is_pasted() {
    let dir = ScratchDir::new("image-paste").unwrap();
    let png = dir.path().join("shot.png");
    std::fs::write(&png, picture(image::ImageFormat::Png)).unwrap();
    // The format comes from the bytes: a JPEG named .png, a PNG with no
    // extension.
    let jpeg = dir.path().join("photo.png");
    std::fs::write(&jpeg, picture(image::ImageFormat::Jpeg)).unwrap();
    let bare = dir.path().join("bare");
    std::fs::write(&bare, picture(image::ImageFormat::Png)).unwrap();
    let text = dir.path().join("notes.png");
    std::fs::write(&text, "not a picture").unwrap();
    let bmp = dir.path().join("old.bmp");
    std::fs::write(&bmp, b"BM\x46\0\0\0\0\0\0\0\x36\0\0\0").unwrap();
    let link = dir.path().join("link.png");
    std::os::unix::fs::symlink(&png, &link).unwrap();
    let fifo = dir.path().join("pipe.png");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let show = |p: &Path| p.display().to_string();

    assert_eq!(dropped_images(&show(&png)), Some(vec![png.clone()]));
    assert_eq!(
        dropped_images(&format!(
            "'{}' '{}' {} ",
            show(&jpeg),
            show(&bare),
            show(&link)
        )),
        Some(vec![jpeg, bare, link])
    );
    assert_eq!(
        dropped_images(&format!("file://{}", show(&png))),
        Some(vec![png.clone()])
    );
    // A `#` inside a quoted name is the name's, not a comment.
    let hash = dir.path().join("#hash.png");
    std::fs::write(&hash, picture(image::ImageFormat::Png)).unwrap();
    assert_eq!(
        dropped_images(&format!("'{}' ", show(&hash))),
        Some(vec![hash])
    );
    for pasted in [
        // The words after a `#` are the user's, not a comment.
        format!("'{}' # why is it red?", show(&png)),
        // Not an image, or one the chat does not send.
        show(&text),
        show(&bmp),
        // A folder, a FIFO (never opened: it would wait for a writer), a
        // device, a file that is not there.
        show(dir.path()),
        show(&fifo),
        "/dev/zero".to_string(),
        show(&dir.path().join("gone.png")),
        // A path in a larger paste, and one beside a file that is no image.
        format!("look at {}", show(&png)),
        format!("{} {}", show(&png), show(&text)),
        format!("{}\nand say what you see", show(&png)),
    ] {
        assert_eq!(dropped_images(&pasted), None, "{pasted}");
    }
}

#[test]
fn the_clipboards_picture_becomes_a_png_data_url() {
    let rgba: Vec<u8> = (0..3 * 2).flat_map(|i| [i * 40, 200, 10, 255]).collect();
    let url = png_data_url(3, 2, rgba.clone()).unwrap();
    let encoded = url.strip_prefix("data:image/png;base64,").unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let back = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .unwrap()
        .into_rgba8();
    assert_eq!((back.width(), back.height()), (3, 2));
    assert_eq!(back.into_raw(), rgba);
    assert_eq!(
        png_data_url(3, 3, rgba),
        Err("the clipboard's image is not RGBA".into())
    );
}
