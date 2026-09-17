//! Print an explicitly selected image protocol inline, leaving it after exit.
//!
//! `cargo run -p bmux_tui_runtime --example image_runtime -- --protocol sixel`
//! With one protocol feature enabled, no argument is needed.
//! No alternate screen, raw mode, input reader, or image deletion is needed.

use std::io;
#[cfg(feature = "image-kitty")]
use std::io::Write;

#[cfg(feature = "image-iterm2")]
#[path = "image_iterm2.rs"]
mod iterm2;
#[cfg(feature = "image-sixel")]
#[path = "image_sixel.rs"]
mod sixel;

fn select_protocol<'a>(args: &[String], enabled: &'a [&str]) -> io::Result<&'a str> {
    let requested = match args {
        [] => None,
        [flag, protocol] if flag == "--protocol" => Some(protocol.as_str()),
        _ => {
            return Err(io::Error::other(
                "usage: image_runtime [--protocol kitty|sixel|iterm2]",
            ));
        }
    };
    if let Some(requested) = requested {
        return enabled.iter().copied().find(|name| *name == requested).ok_or_else(|| {
            io::Error::other(format!("protocol {requested:?} is unknown or disabled; enable its image-<protocol> Cargo feature"))
        });
    }
    match enabled {
        [only] => Ok(only),
        [] => Err(io::Error::other(
            "enable image-kitty, image-sixel, or image-iterm2",
        )),
        _ => Err(io::Error::other(
            "multiple protocols enabled; select --protocol kitty|sixel|iterm2",
        )),
    }
}

fn main() -> io::Result<()> {
    let enabled = [
        #[cfg(feature = "image-kitty")]
        "kitty",
        #[cfg(feature = "image-sixel")]
        "sixel",
        #[cfg(feature = "image-iterm2")]
        "iterm2",
    ];
    let args: Vec<_> = std::env::args().skip(1).collect();
    match select_protocol(&args, &enabled)? {
        #[cfg(feature = "image-kitty")]
        "kitty" => kitty(),
        #[cfg(feature = "image-sixel")]
        "sixel" => sixel::main(),
        #[cfg(feature = "image-iterm2")]
        "iterm2" => iterm2::main(),
        _ => unreachable!("selection only returns enabled protocols"),
    }
}

#[cfg(feature = "image-kitty")]
fn kitty() -> io::Result<()> {
    let mut out = io::stdout().lock();
    let id = std::process::id().max(1);
    let pixels = [
        255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
    ];
    writeln!(out, "BMUX inline image")?;
    // Reserve space first (including at the bottom of the screen), then return
    // to its top using relative movement. Never overwrite earlier shell output.
    for _ in 0..8 {
        writeln!(out)?;
    }
    write!(out, "\r\x1b[8A")?;
    for chunk in bmux_image::codec::kitty::encode_transmit_chunks(
        id,
        bmux_image::KittyFormat::Rgba,
        &pixels,
        2,
        2,
    ) {
        out.write_all(b"\x1b_")?;
        out.write_all(&chunk)?;
        out.write_all(b"\x1b\\")?;
    }
    // C=1 keeps the cursor at the placement origin; advance below it ourselves.
    write!(
        out,
        "\x1b_Ga=p,i={id},p={id},c=16,r=8,C=1,q=2;\x1b\\\x1b[8B\r"
    )?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::select_protocol;

    #[test]
    fn selects_each_single_enabled_protocol() {
        for protocol in ["kitty", "sixel", "iterm2"] {
            assert_eq!(select_protocol(&[], &[protocol]).unwrap(), protocol);
        }
    }

    #[test]
    fn explicit_selection_requires_enabled_protocol() {
        let enabled = ["kitty", "sixel", "iterm2"];
        for protocol in enabled {
            let args = ["--protocol".to_owned(), protocol.to_owned()];
            assert_eq!(select_protocol(&args, &enabled).unwrap(), protocol);
            assert!(select_protocol(&args, &[]).is_err());
        }
        assert!(select_protocol(&["--protocol".into(), "unknown".into()], &enabled).is_err());
    }

    #[test]
    fn rejects_ambiguous_missing_and_malformed_selection() {
        assert!(select_protocol(&[], &[]).is_err());
        assert!(select_protocol(&[], &["kitty", "sixel"]).is_err());
        assert!(select_protocol(&["--protocol".into()], &["kitty"]).is_err());
        assert!(select_protocol(&["--other".into(), "kitty".into()], &["kitty"]).is_err());
    }
}
