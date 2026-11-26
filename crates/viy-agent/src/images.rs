//! The images a message carries (kb d191), prepared as Codex prepares a
//! local image for the prompt (`codex-rs/utils/image`, `ResizeToFit`): one
//! wider or taller than [`MAX_DIMENSION`] is resized to fit a
//! `MAX_DIMENSION` square with its aspect kept, PNG, JPEG and WebP keep
//! their format (and their bytes, when nothing was resized), anything else
//! the decoder reads becomes PNG. The colour profile (RGB only) and EXIF
//! travel with a re-encoded image. What comes out is a base64 data URL,
//! the only kind of image viy sends.
//!
//! Codex's per-process cache is left out: viy prepares an image once, when
//! its message is sent, and the saved chat keeps the result.

use std::io::Cursor;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::ColorType;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageDecoder;
use image::ImageEncoder;
use image::ImageFormat;
use image::ImageReader;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use viy_protocol::ImageInput;

/// Codex's `MAX_DIMENSION`: the largest width or height sent.
pub const MAX_DIMENSION: u32 = 2048;

/// Codex's `MAX_PROMPT_IMAGE_INPUT_BYTES`: a sanity guard against a
/// pathological file, not a target size.
const MAX_INPUT_BYTES: usize = 1024 * 1024 * 1024;

/// Codex's JPEG quality for a resized JPEG.
const JPEG_QUALITY: u8 = 85;

/// Each image as a data URL, in order; the first that cannot be prepared
/// stops it, with what the user is told. Blocking: decoding and resizing
/// a large picture takes a while, so the caller runs it off the async
/// thread.
pub fn prepare_all(images: &[ImageInput]) -> Result<Vec<String>, String> {
    images
        .iter()
        .enumerate()
        .map(|(i, image)| prepare(image).map_err(|e| format!("image #{} not sent: {e}", i + 1)))
        .collect()
}

/// One image as a data URL.
pub fn prepare(image: &ImageInput) -> Result<String, String> {
    let bytes = match image {
        ImageInput::File { path } => read_file(path)?,
        ImageInput::DataUrl { url } => data_url_bytes(url)?,
    };
    let (bytes, mime) = for_prompt(bytes)?;
    Ok(format!("data:{mime};base64,{}", BASE64.encode(bytes)))
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let shown = path.display();
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{shown}: {e}"))?
        .len();
    if usize::try_from(size)
        .ok()
        .is_none_or(|n| n > MAX_INPUT_BYTES)
    {
        return Err(format!("{shown} is too large ({size} bytes)"));
    }
    std::fs::read(path).map_err(|e| format!("{shown}: {e}"))
}

/// The bytes of a base64 data URL (Codex's `load_data_url_for_prompt`).
fn data_url_bytes(url: &str) -> Result<Vec<u8>, String> {
    let rest = url
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("data:"))
        .and_then(|_| url.get(5..))
        .ok_or("not a data URL")?;
    let (meta, encoded) = rest.split_once(',').ok_or("a data URL without a comma")?;
    if !meta
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Err("only base64 data URLs are read".into());
    }
    if encoded.len() > MAX_INPUT_BYTES {
        return Err(format!("too large ({} bytes of base64)", encoded.len()));
    }
    BASE64
        .decode(encoded)
        .map_err(|e| format!("its base64 is broken: {e}"))
}

/// The bytes to send and their MIME type.
fn for_prompt(bytes: Vec<u8>) -> Result<(Vec<u8>, &'static str), String> {
    let guessed = image::guess_format(&bytes).map_err(|_| "not an image".to_string())?;
    let mut decoder = ImageReader::with_format(Cursor::new(&bytes), guessed)
        .into_decoder()
        .map_err(|e| unreadable(guessed, &e))?;
    // Codex keeps only an RGB profile: a CMYK JPEG decodes to RGB, and
    // its profile would then mislabel the pixels. Bytes 16..20 are the
    // ICC colour space.
    let icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|profile| profile.get(16..20) == Some(b"RGB "));
    let exif = decoder.exif_metadata().ok().flatten();
    let decoded = DynamicImage::from_decoder(decoder).map_err(|e| unreadable(guessed, &e))?;
    let (width, height) = decoded.dimensions();
    let kept = keeps_its_bytes(guessed).then_some(guessed);
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        let resized = decoded.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Triangle);
        return encode(&resized, kept.unwrap_or(ImageFormat::Png), icc, exif);
    }
    match kept {
        Some(format) => Ok((bytes, mime(format))),
        None => encode(&decoded, ImageFormat::Png, icc, exif),
    }
}

/// Codex's `can_preserve_source_bytes`.
fn keeps_its_bytes(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    )
}

fn unreadable(format: ImageFormat, e: &image::ImageError) -> String {
    match e {
        image::ImageError::Unsupported(_) => {
            let kind = format.extensions_str().first().copied().unwrap_or("these");
            format!("{kind} images are not read")
        }
        other => format!("could not read it: {other}"),
    }
}

/// Codex's `encode_image`: PNG, JPEG at quality 85, or lossless WebP.
fn encode(
    image: &DynamicImage,
    format: ImageFormat,
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
) -> Result<(Vec<u8>, &'static str), String> {
    let mut out = Vec::new();
    let failed = |e: image::ImageError| format!("could not encode it: {e}");
    match format {
        ImageFormat::Jpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
            with_metadata(&mut encoder, icc, exif)?;
            encoder.encode_image(image).map_err(failed)?;
        }
        ImageFormat::WebP => {
            let rgba = image.to_rgba8();
            let mut encoder = WebPEncoder::new_lossless(&mut out);
            with_metadata(&mut encoder, icc, exif)?;
            encoder
                .write_image(
                    rgba.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .map_err(failed)?;
        }
        _ => {
            let rgba = image.to_rgba8();
            let mut encoder = PngEncoder::new(&mut out);
            with_metadata(&mut encoder, icc, exif)?;
            encoder
                .write_image(
                    rgba.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .map_err(failed)?;
        }
    }
    let format = match format {
        ImageFormat::Jpeg | ImageFormat::WebP => format,
        _ => ImageFormat::Png,
    };
    Ok((out, mime(format)))
}

fn with_metadata(
    encoder: &mut impl ImageEncoder,
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
) -> Result<(), String> {
    if let Some(icc) = icc {
        encoder
            .set_icc_profile(icc)
            .map_err(|e| format!("could not keep its colour profile: {e}"))?;
    }
    if let Some(exif) = exif {
        encoder
            .set_exif_metadata(exif)
            .map_err(|e| format!("could not keep its EXIF data: {e}"))?;
    }
    Ok(())
}

fn mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
