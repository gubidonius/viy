use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageFormat;
use image::Rgb;
use image::RgbImage;
use pretty_assertions::assert_eq;
use viy_protocol::ImageInput;

use super::*;

/// A synthetic picture: a gradient, so a resize has something to keep.
fn picture(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let img = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img)
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

fn data_url(mime: &str, bytes: &[u8]) -> ImageInput {
    ImageInput::DataUrl {
        url: format!("data:{mime};base64,{}", BASE64.encode(bytes)),
    }
}

/// The MIME type and the decoded size of a prepared data URL.
fn sent(url: &str) -> (String, (u32, u32)) {
    let (meta, encoded) = url.strip_prefix("data:").unwrap().split_once(',').unwrap();
    let bytes = BASE64.decode(encoded).unwrap();
    let mime = meta.strip_suffix(";base64").unwrap().to_string();
    (mime, image::load_from_memory(&bytes).unwrap().dimensions())
}

#[test]
fn a_small_png_goes_as_it_is() {
    let png = picture(64, 32, ImageFormat::Png);
    let url = prepare(&data_url("image/png", &png)).unwrap();
    assert_eq!(
        url,
        format!("data:image/png;base64,{}", BASE64.encode(&png))
    );
}

#[test]
fn a_wide_png_is_resized_to_fit_2048() {
    let url = prepare(&data_url(
        "image/png",
        &picture(2100, 700, ImageFormat::Png),
    ))
    .unwrap();
    assert_eq!(sent(&url), ("image/png".to_string(), (2048, 683)));
}

#[test]
fn a_tall_jpeg_stays_a_jpeg_when_resized() {
    let url = prepare(&data_url(
        "image/jpeg",
        &picture(1000, 2600, ImageFormat::Jpeg),
    ))
    .unwrap();
    assert_eq!(sent(&url), ("image/jpeg".to_string(), (788, 2048)));
}

#[test]
fn a_gif_becomes_a_png() {
    let url = prepare(&data_url("image/gif", &picture(40, 30, ImageFormat::Gif))).unwrap();
    assert_eq!(sent(&url), ("image/png".to_string(), (40, 30)));
}

/// The format comes from the bytes, not the MIME type a data URL claims
/// or a file's name.
#[test]
fn the_format_comes_from_the_bytes() {
    let png = picture(8, 8, ImageFormat::Png);
    let url = prepare(&data_url("image/jpeg", &png)).unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "{url}");
    let scratch = viy_testkit::scratch::ScratchDir::new("images").unwrap();
    let path = scratch.path().join("shot.jpg");
    std::fs::write(&path, &png).unwrap();
    let url = prepare(&ImageInput::File { path }).unwrap();
    assert_eq!(
        url,
        format!("data:image/png;base64,{}", BASE64.encode(&png))
    );
}

#[test]
fn what_cannot_be_sent_says_why() {
    let scratch = viy_testkit::scratch::ScratchDir::new("images-bad").unwrap();
    let gone = scratch.path().join("gone.png");
    let text = scratch.path().join("notes.png");
    std::fs::write(&text, "not a picture").unwrap();
    let bmp = scratch.path().join("old.bmp");
    std::fs::write(&bmp, b"BM\x46\0\0\0\0\0\0\0\x36\0\0\0").unwrap();
    for (image, said) in [
        (ImageInput::File { path: text }, "not an image".to_string()),
        (
            ImageInput::File { path: bmp },
            "bmp images are not read".to_string(),
        ),
        (
            ImageInput::DataUrl {
                url: "https://example.com/cat.png".into(),
            },
            "not a data URL".to_string(),
        ),
        (
            ImageInput::DataUrl {
                url: "data:image/png,AAAA".into(),
            },
            "only base64 data URLs are read".to_string(),
        ),
    ] {
        assert_eq!(prepare(&image), Err(said));
    }
    let missing = prepare(&ImageInput::File { path: gone.clone() }).unwrap_err();
    assert!(
        missing.starts_with(&format!("{}: ", gone.display())),
        "{missing}"
    );
}

#[test]
fn the_first_image_that_fails_stops_the_rest() {
    let good = data_url("image/png", &picture(4, 4, ImageFormat::Png));
    let bad = ImageInput::DataUrl {
        url: "data:image/png;base64,bm90IGFuIGltYWdl".into(),
    };
    assert_eq!(
        prepare_all(&[good.clone(), bad]),
        Err("image #2 not sent: not an image".to_string())
    );
    assert_eq!(prepare_all(&[good]).unwrap().len(), 1);
}
