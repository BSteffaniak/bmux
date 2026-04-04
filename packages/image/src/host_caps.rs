//! Host terminal image capability detection.
//!
//! Probes the host terminal at attach time to determine which image
//! protocols are supported, allowing the compositor to emit images in
//! the best available format.

use crate::model::ImageProtocol;

/// Detected host terminal image capabilities.
#[derive(Clone, Debug, Default)]
pub struct HostImageCapabilities {
    /// Host supports Sixel graphics.
    pub sixel: bool,
    /// Host supports Kitty graphics protocol.
    pub kitty_graphics: bool,
    /// Host supports iTerm2 inline images (OSC 1337).
    pub iterm2_inline: bool,
    /// Maximum image width in pixels (if reported by the terminal).
    pub max_pixel_width: Option<u32>,
    /// Maximum image height in pixels (if reported by the terminal).
    pub max_pixel_height: Option<u32>,
    /// Cell dimensions in pixels (width, height). (0, 0) = unknown.
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
}

impl HostImageCapabilities {
    /// Returns the preferred image protocol based on detected capabilities.
    ///
    /// Preference order: Kitty > Sixel > iTerm2.
    /// Kitty is preferred because it supports transmit-once-place-many,
    /// reducing bandwidth for redraws.
    pub fn preferred_protocol(&self) -> Option<ImageProtocol> {
        if self.kitty_graphics {
            Some(ImageProtocol::KittyGraphics)
        } else if self.sixel {
            Some(ImageProtocol::Sixel)
        } else if self.iterm2_inline {
            Some(ImageProtocol::ITerm2)
        } else {
            None
        }
    }

    /// Whether any image protocol is available.
    pub fn any_supported(&self) -> bool {
        self.sixel || self.kitty_graphics || self.iterm2_inline
    }
}

/// Detect host terminal image capabilities.
///
/// This performs synchronous terminal queries (DA, kitty graphics query,
/// environment variable inspection).  Must be called at attach time before
/// entering the render loop.
///
/// Returns `HostImageCapabilities::default()` (nothing supported) if
/// detection fails or the terminal does not respond.
pub fn detect_from_env() -> HostImageCapabilities {
    let mut caps = HostImageCapabilities::default();

    // -- Environment variable heuristics ----------------------------------

    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let lc_terminal = std::env::var("LC_TERMINAL").unwrap_or_default();

    match term_program.as_str() {
        "iTerm.app" => {
            caps.iterm2_inline = true;
            // iTerm2 also supports sixel since v3.3.0
            caps.sixel = true;
        }
        "WezTerm" => {
            caps.sixel = true;
            caps.kitty_graphics = true;
            caps.iterm2_inline = true;
        }
        "ghostty" => {
            caps.kitty_graphics = true;
        }
        _ => {}
    }

    if lc_terminal == "iTerm2" {
        caps.iterm2_inline = true;
        caps.sixel = true;
    }

    // Kitty sets TERM=xterm-kitty
    let term = std::env::var("TERM").unwrap_or_default();
    if term.contains("kitty") {
        caps.kitty_graphics = true;
    }

    // foot terminal
    if term_program == "foot" || term.starts_with("foot") {
        caps.sixel = true;
    }

    // Alacritty supports sixel since 0.14
    if term_program == "Alacritty" || term.starts_with("alacritty") {
        caps.sixel = true;
    }

    // Windows Terminal
    if term_program == "Windows_Terminal" {
        caps.sixel = true;
    }

    caps
}

/// Enhanced detection that sends terminal queries.
///
/// Must be called while the terminal is in raw mode.  Falls back to
/// `detect_from_env()` results if queries time out.
#[cfg(unix)]
pub fn detect_with_queries() -> HostImageCapabilities {
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    let mut caps = detect_from_env();

    // Send DA1 query (Primary Device Attributes) to detect sixel.
    // Response: ESC [ ? <attrs> c  where attrs include "4" for sixel.
    let mut stdout = std::io::stdout();
    if stdout.write_all(b"\x1b[c").is_err() || stdout.flush().is_err() {
        return caps;
    }

    let deadline = Instant::now() + Duration::from_millis(100);
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut response = Vec::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(100) as i32;

        let mut pollfd = PollFd {
            fd: stdin_fd,
            events: 1, // POLLIN
            revents: 0,
        };
        let ret = unsafe { poll(&mut pollfd, 1, millis) };
        if ret <= 0 {
            break;
        }

        let mut buf = [0u8; 256];
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        match handle.read(&mut buf) {
            Ok(n) if n > 0 => {
                response.extend_from_slice(&buf[..n]);
                // DA1 response ends with 'c'.
                if response.iter().any(|&b| b == b'c') {
                    break;
                }
            }
            _ => break,
        }
    }

    // Parse DA1 response for sixel attribute (4).
    if let Some(da_response) = parse_da1_response(&response) {
        if da_response.contains(&4) {
            caps.sixel = true;
        }
    }

    caps
}

/// Non-unix stub for query-based detection.
#[cfg(not(unix))]
pub fn detect_with_queries() -> HostImageCapabilities {
    detect_from_env()
}

/// Parse a DA1 response (`ESC [ ? <attrs> c`) into a list of attribute numbers.
fn parse_da1_response(response: &[u8]) -> Option<Vec<u32>> {
    // Find ESC [ ? ... c
    let esc_pos = response.iter().position(|&b| b == 0x1b)?;
    let rest = &response[esc_pos..];
    if rest.len() < 4 || rest[1] != b'[' || rest[2] != b'?' {
        return None;
    }
    let c_pos = rest[3..].iter().position(|&b| b == b'c')?;
    let params = &rest[3..3 + c_pos];
    let params_str = std::str::from_utf8(params).ok()?;
    let attrs: Vec<u32> = params_str
        .split(';')
        .filter_map(|s| s.parse().ok())
        .collect();
    Some(attrs)
}

#[cfg(unix)]
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[cfg(unix)]
unsafe fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32 {
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    }
    unsafe { poll(fds, nfds, timeout) }
}

/// Query the host terminal for cell pixel dimensions.
///
/// Uses the `TIOCGWINSZ` ioctl which reports both character and pixel
/// dimensions of the terminal window.  Divides pixel size by character
/// size to get cell dimensions.  Returns `(width, height)` or `(0, 0)`
/// if the query fails.
#[cfg(unix)]
pub fn query_cell_pixel_size() -> (u16, u16) {
    use std::mem::MaybeUninit;
    use std::os::unix::io::AsRawFd;

    let fd = std::io::stdout().as_raw_fd();
    let mut ws = MaybeUninit::<Winsize>::uninit();

    // TIOCGWINSZ = 0x5413 on Linux, 0x40087468 on macOS.
    // The libc constants handle this portably.
    let ret = unsafe { ioctl_tiocgwinsz(fd, ws.as_mut_ptr()) };
    if ret != 0 {
        return (0, 0);
    }

    let ws = unsafe { ws.assume_init() };
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return (0, 0);
    }
    if ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return (0, 0);
    }

    let cell_w = ws.ws_xpixel / ws.ws_col;
    let cell_h = ws.ws_ypixel / ws.ws_row;
    (cell_w, cell_h)
}

#[cfg(unix)]
#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[cfg(target_os = "macos")]
const TIOCGWINSZ: u64 = 0x4008_7468;

#[cfg(target_os = "linux")]
const TIOCGWINSZ: u64 = 0x5413;

#[cfg(unix)]
unsafe fn ioctl_tiocgwinsz(fd: i32, ws: *mut Winsize) -> i32 {
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    unsafe { ioctl(fd, TIOCGWINSZ, ws) }
}

/// Non-unix stub.
#[cfg(not(unix))]
pub fn query_cell_pixel_size() -> (u16, u16) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_protocol_order() {
        let caps = HostImageCapabilities {
            sixel: true,
            kitty_graphics: true,
            iterm2_inline: true,
            ..Default::default()
        };
        assert_eq!(
            caps.preferred_protocol(),
            Some(ImageProtocol::KittyGraphics)
        );

        let caps = HostImageCapabilities {
            sixel: true,
            kitty_graphics: false,
            iterm2_inline: true,
            ..Default::default()
        };
        assert_eq!(caps.preferred_protocol(), Some(ImageProtocol::Sixel));
    }

    #[test]
    fn no_support_returns_none() {
        let caps = HostImageCapabilities::default();
        assert_eq!(caps.preferred_protocol(), None);
        assert!(!caps.any_supported());
    }

    #[test]
    fn parse_da1_with_sixel_attribute() {
        // DA1 response with attribute 4 (sixel) present.
        let response = b"\x1b[?62;4;6;22c";
        let attrs = parse_da1_response(response).unwrap();
        assert!(attrs.contains(&4));
        assert!(attrs.contains(&62));
    }

    #[test]
    fn parse_da1_without_sixel() {
        let response = b"\x1b[?62;6;22c";
        let attrs = parse_da1_response(response).unwrap();
        assert!(!attrs.contains(&4));
    }

    #[test]
    fn parse_da1_invalid_response() {
        assert!(parse_da1_response(b"garbage").is_none());
        assert!(parse_da1_response(b"\x1b[1;2R").is_none()); // not a DA response
    }
}
