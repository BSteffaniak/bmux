//! Native interoperability checks must run only on an explicitly isolated display.
#![cfg(feature = "native-clipboard")]

use bmux_clipboard::{ClipboardPayload, copy_png_image, copy_text, read_payload, read_text};

#[test]
#[ignore = "requires BMUX_CLIPBOARD_ISOLATED_TEST=1 and a disposable native clipboard"]
fn native_text_and_image_roundtrip() {
    assert_eq!(
        std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
        Ok("1")
    );
    for text in [
        "",
        "Unicode 界 🦀 e\u{301}\nsecond line",
        "trailing newline\n",
    ] {
        eprintln!("copy text bytes={}", text.len());
        copy_text(text).unwrap();
        eprintln!("read text bytes={}", text.len());
        assert_eq!(read_text().unwrap(), text);
    }
    let text = "large 界\n".repeat(200_000);
    copy_text(&text).unwrap();
    assert_eq!(read_text().unwrap(), text);

    let rgba = image::RgbaImage::from_raw(
        2,
        2,
        vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 1, 2, 3, 255],
    )
    .unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba.clone())
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    copy_png_image(png.get_ref()).unwrap();
    let ClipboardPayload::ImagePng(bytes) = read_payload(true).unwrap() else {
        panic!("native image was not selected");
    };
    assert_eq!(image::load_from_memory(&bytes).unwrap().into_rgba8(), rgba);
}
