//! Native interoperability checks must run only on an explicitly isolated display.

use bmux_clipboard::{ClipboardPayload, copy_png_image, native_text, read_payload};

fn decode_pixels(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let image = bmux_raster::decode(bytes, u32::MAX, u32::MAX, 512 * 1024 * 1024).unwrap();
    (image.width, image.height, image.rgba)
}

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

    let rgba = (
        2,
        2,
        vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 1, 2, 3, 255],
    );
    let png = bmux_raster::encode_png(rgba.0, rgba.1, 4, &rgba.2).unwrap();
    copy_png_image(&png).unwrap();
    assert!(
        native_text::read().unwrap().is_none(),
        "image-only owner advertised text"
    );
    let ClipboardPayload::ImagePng(bytes) = read_payload(true).unwrap() else {
        panic!("native image was not selected");
    };
    assert!(decode_pixels(&bytes) == rgba);
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

    let rgba = (2, 1, vec![255, 0, 0, 128, 0, 1, 2, 255]);
    let png = bmux_raster::encode_png(rgba.0, rgba.1, 4, &rgba.2).unwrap();
    copy_png_image(&png).unwrap();
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
    assert!(decode_pixels(&output.stdout) == rgba);

    let external_rgba = (1, 2, vec![0, 255, 0, 255, 0, 0, 255, 64]);
    let external_png =
        bmux_raster::encode_png(external_rgba.0, external_rgba.1, 4, &external_rgba.2).unwrap();
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
        .write_all(&external_png)
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let result = loop {
        let result = read_payload(true).map(|payload| match payload {
            ClipboardPayload::ImagePng(bytes) => decode_pixels(&bytes),
            _ => (0, 0, Vec::new()),
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
