//! Native interoperability checks must run only on an explicitly isolated display.
#![cfg(feature = "native-clipboard")]

use bmux_clipboard::{ClipboardPayload, copy_png_image, native_text, read_payload};

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
        native_text::write(text).unwrap();
        eprintln!("read text bytes={}", text.len());
        assert!(native_text::read().unwrap().as_deref() == Some(text));
    }
    let text = "large 界\n".repeat(200_000);
    native_text::write(&text).unwrap();
    assert!(native_text::read().unwrap().as_ref() == Some(&text));

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
    assert!(
        native_text::read().unwrap().is_none(),
        "image-only owner advertised text"
    );
    let ClipboardPayload::ImagePng(bytes) = read_payload(true).unwrap() else {
        panic!("native image was not selected");
    };
    assert!(image::load_from_memory(&bytes).unwrap().into_rgba8() == rgba);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires isolated X11 display, xclip, and BMUX_CLIPBOARD_ISOLATED_TEST=1"]
fn independent_xclip_text_interoperability() {
    independent_text_interoperability(false);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires isolated Wayland compositor, wl-clipboard, and BMUX_CLIPBOARD_ISOLATED_TEST=1"]
fn independent_wayland_text_interoperability() {
    independent_text_interoperability(true);
}

#[cfg(target_os = "linux")]
fn independent_text_interoperability(wayland: bool) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    assert_eq!(
        std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
        Ok("1")
    );
    if wayland {
        assert!(std::env::var_os("DISPLAY").is_none());
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_some());
    } else {
        assert!(std::env::var_os("DISPLAY").is_some());
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
    }
    let text = "independent UTF-8 界\n".repeat(100_000);
    native_text::write(&text).unwrap();
    let output = Command::new("timeout")
        .args(if wayland {
            vec![
                "15s",
                "wl-paste",
                "--no-newline",
                "--type",
                "text/plain;charset=utf-8",
            ]
        } else {
            vec![
                "15s",
                "xclip",
                "-selection",
                "clipboard",
                "-out",
                "-target",
                "UTF8_STRING",
            ]
        })
        .output()
        .unwrap();
    assert!(output.status.success(), "external reader failed");
    assert!(output.stdout == text.as_bytes(), "external reader mismatch");

    // Foreground owner remains alive until our native reader has consumed it.
    let text = "external producer 界\n".repeat(100_000);
    let mut owner = Command::new("timeout")
        .args(if wayland {
            vec![
                "15s",
                "wl-copy",
                "--foreground",
                "--type",
                "text/plain;charset=utf-8",
            ]
        } else {
            vec![
                "15s",
                "xclip",
                "-selection",
                "clipboard",
                "-in",
                "-target",
                "UTF8_STRING",
                "-quiet",
            ]
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    owner
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = loop {
        match native_text::read() {
            Ok(Some(value)) if value == text => break true,
            _ if std::time::Instant::now() >= deadline => break false,
            _ => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    let _ = owner.kill();
    let _ = owner.wait();
    assert!(result, "external text not received intact");

    let rgba = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 128, 0, 1, 2, 255]).unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba.clone())
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    copy_png_image(png.get_ref()).unwrap();
    let output = Command::new("timeout")
        .args(if wayland {
            vec!["15s", "wl-paste", "--no-newline", "--type", "image/png"]
        } else {
            vec![
                "15s",
                "xclip",
                "-selection",
                "clipboard",
                "-out",
                "-target",
                "image/png",
            ]
        })
        .output()
        .unwrap();
    assert!(output.status.success(), "external reader failed");
    assert!(
        image::load_from_memory(&output.stdout)
            .unwrap()
            .into_rgba8()
            == rgba
    );

    let external_rgba =
        image::RgbaImage::from_raw(1, 2, vec![0, 255, 0, 255, 0, 0, 255, 64]).unwrap();
    let mut external_png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(external_rgba.clone())
        .write_to(&mut external_png, image::ImageFormat::Png)
        .unwrap();
    let mut owner = Command::new("timeout")
        .args(if wayland {
            vec!["15s", "wl-copy", "--foreground", "--type", "image/png"]
        } else {
            vec![
                "15s",
                "xclip",
                "-selection",
                "clipboard",
                "-in",
                "-target",
                "image/png",
                "-quiet",
            ]
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    owner
        .stdin
        .take()
        .unwrap()
        .write_all(external_png.get_ref())
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = loop {
        let result = read_payload(true).map(|payload| match payload {
            ClipboardPayload::ImagePng(bytes) => {
                image::load_from_memory(&bytes).unwrap().into_rgba8()
            }
            _ => image::RgbaImage::new(0, 0),
        });
        match result {
            Ok(value) if value == external_rgba => break Ok(value),
            value if std::time::Instant::now() >= deadline => break value,
            _ => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    let _ = owner.kill();
    let _ = owner.wait();
    assert!(result.unwrap() == external_rgba);
}
