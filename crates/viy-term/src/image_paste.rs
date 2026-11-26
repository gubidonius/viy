//! Images a paste brings (kb d191), Codex's `clipboard_paste.rs` cut to
//! Linux: a dropped file, which GNOME Terminal and tmux hand over as a
//! bracketed paste of its path, and the clipboard's picture for Ctrl+V.
//!
//! A paste is image files when every word of it, as the shell splits it
//! (quotes, backslash escapes, `file://` URLs; Codex's
//! `normalize_pasted_path` without its Windows and WSL paths), names a
//! regular file whose first bytes read as an image the chat sends (PNG,
//! JPEG, GIF, WebP). Codex takes one path; GNOME Terminal drops several
//! as one paste, so every one counts. Anything else is text, as before.

use std::path::Path;
use std::path::PathBuf;

use base64::Engine;

/// The longest paste looked at for paths (Codex's
/// `LARGE_PASTE_CHAR_THRESHOLD`): a longer one is text.
const MAX_PATHS_CHARS: usize = 1000;

/// The paths a paste names, every word of it one, or `None` when it is
/// not a list of paths: one char or none, longer than
/// [`MAX_PATHS_CHARS`], quotes left open, a word starting with `#` (the
/// shell's comment, which would drop the words after it), or a `file://`
/// URL that names no local file.
pub fn pasted_paths(pasted: &str) -> Option<Vec<PathBuf>> {
    let pasted = pasted.replace("\r\n", "\n").replace('\r', "\n");
    let chars = pasted.trim().chars().count();
    if chars <= 1 || chars > MAX_PATHS_CHARS {
        return None;
    }
    if pasted.split_whitespace().any(|word| word.starts_with('#')) {
        return None;
    }
    let words = shlex::split(&pasted)?;
    if words.is_empty() {
        return None;
    }
    words.iter().map(|word| path_of(word)).collect()
}

/// One word as a path: a `file://` URL's (on this machine only), or the
/// word itself.
fn path_of(word: &str) -> Option<PathBuf> {
    match url::Url::parse(word) {
        Ok(url) if url.scheme() == "file" => url.to_file_path().ok(),
        _ => Some(PathBuf::from(word)),
    }
}

/// Whether `path` is a regular file whose header reads as an image the
/// chat sends; the format from the bytes, not the name, as
/// `viy_agent::images` decides it. Only a regular file is opened (a
/// device's open can do things), without waiting and never as the
/// terminal (a FIFO's open would wait for a writer), and checked again as
/// opened, so a FIFO, a device or a folder put there after the look is
/// never read.
pub fn is_image(path: &Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    else {
        return false;
    };
    file.metadata().is_ok_and(|m| m.is_file())
        && image::ImageReader::new(std::io::BufReader::new(file))
            .with_guessed_format()
            .is_ok_and(|reader| reader.into_dimensions().is_ok())
}

/// `paths`, made absolute, when every one is an image ([`is_image`]).
pub fn image_files(paths: &[PathBuf]) -> Option<Vec<PathBuf>> {
    if paths.is_empty() || !paths.iter().all(|p| is_image(p)) {
        return None;
    }
    paths.iter().map(|p| std::path::absolute(p).ok()).collect()
}

/// The image files a paste names, when it names nothing else.
pub fn dropped_images(pasted: &str) -> Option<Vec<PathBuf>> {
    image_files(&pasted_paths(pasted)?)
}

/// The clipboard's picture, `width` x `height` RGBA, as a PNG data URL
/// (Codex's `paste_image_as_png`, kept in memory instead of a temporary
/// file).
pub fn png_data_url(width: usize, height: usize, rgba: Vec<u8>) -> Result<String, String> {
    let size = |n: usize| u32::try_from(n).map_err(|_| "the image is too large".to_string());
    let picture = image::RgbaImage::from_raw(size(width)?, size(height)?, rgba)
        .ok_or_else(|| "the clipboard's image is not RGBA".to_string())?;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(picture)
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| format!("could not encode the clipboard's image: {e}"))?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png.into_inner())
    ))
}

#[cfg(test)]
#[path = "image_paste_tests.rs"]
mod tests;
