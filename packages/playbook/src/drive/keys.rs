//! Keys and mouse events as an agent names them, encoded the way an xterm
//! sends them to the program, honoring the input modes the program enabled.
//!
//! Key names accept the spellings agents reach for: `Enter`, `Esc`, `Tab`,
//! `S-Tab`, `Up`, `PgDn`, `F5`, `C-c`, `ctrl+c`, `^C`, `M-x`, `alt+x`,
//! `C-M-Left`, a single character such as `q`, `Q` or `/`, and a repeat
//! suffix such as `Down*3`.

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent modifier keys.
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// The xterm modifier parameter (1 + shift + 2·alt + 4·ctrl).
    const fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }

    const fn any(self) -> bool {
        self.ctrl || self.alt || self.shift
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Mods,
}

/// Parse the key arguments of `press`: each argument may hold several keys
/// separated by spaces, and each key may end in a `*N` repeat.
pub fn parse_keys(arguments: &[String]) -> Result<Vec<KeyPress>> {
    let mut presses = Vec::new();
    for argument in arguments {
        for token in argument.split_whitespace() {
            let (name, count) = split_repeat(token)?;
            let press = parse_key(name)?;
            presses.extend(std::iter::repeat_n(press, count));
        }
    }
    if presses.is_empty() {
        bail!("no keys given; name keys like Enter, Esc, Up, C-c, F5, q");
    }
    Ok(presses)
}

fn split_repeat(token: &str) -> Result<(&str, usize)> {
    if let Some((name, count)) = token.rsplit_once('*')
        && !name.is_empty()
        && !count.is_empty()
        && count.chars().all(|c| c.is_ascii_digit())
    {
        let count: usize = count.parse()?;
        if count == 0 || count > 500 {
            bail!("repeat count in '{token}' must be 1-500");
        }
        return Ok((name, count));
    }
    Ok((token, 1))
}

/// Parse one key name.
pub fn parse_key(token: &str) -> Result<KeyPress> {
    let mut mods = Mods::default();
    let mut rest = token;
    // `^C` is control-C.
    if let Some(after) = rest.strip_prefix('^')
        && after.chars().count() == 1
    {
        mods.ctrl = true;
        rest = after;
    }
    loop {
        if rest.chars().count() <= 1 {
            break;
        }
        let lower = rest.to_ascii_lowercase();
        let prefix = [
            ("ctrl+", 'c'),
            ("control+", 'c'),
            ("c-", 'c'),
            ("alt+", 'a'),
            ("meta+", 'a'),
            ("option+", 'a'),
            ("m-", 'a'),
            ("a-", 'a'),
            ("shift+", 's'),
            ("s-", 's'),
        ]
        .into_iter()
        .find(|(prefix, _)| lower.starts_with(prefix) && lower.len() > prefix.len());
        let Some((prefix, which)) = prefix else {
            break;
        };
        match which {
            'c' => mods.ctrl = true,
            'a' => mods.alt = true,
            _ => mods.shift = true,
        }
        rest = &rest[prefix.len()..];
    }
    if rest.eq_ignore_ascii_case("backtab") {
        mods.shift = true;
        rest = "Tab";
    }
    let key = parse_key_name(rest).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown key '{token}'; use names like Enter, Esc, Tab, Backspace, Space, Up, Down, \
             Left, Right, Home, End, PgUp, PgDn, Insert, Delete, F1-F12, single characters, \
             and modifiers C- (ctrl), M- (alt), S- (shift)"
        )
    })?;
    if let Key::Char(c) = key
        && mods.ctrl
        && control_byte(c).is_none()
    {
        bail!("'{token}': ctrl+{c} has no terminal encoding");
    }
    Ok(KeyPress { key, mods })
}

fn parse_key_name(name: &str) -> Option<Key> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(match c {
            ' ' => Key::Space,
            c => Key::Char(c),
        });
    }
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "enter" | "return" | "ret" | "cr" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" | "bs" | "bspace" => Key::Backspace,
        "esc" | "escape" => Key::Escape,
        "space" | "spc" => Key::Space,
        "up" | "arrowup" | "arrow_up" => Key::Up,
        "down" | "arrowdown" | "arrow_down" => Key::Down,
        "left" | "arrowleft" | "arrow_left" => Key::Left,
        "right" | "arrowright" | "arrow_right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pgup" | "pageup" | "page_up" | "ppage" => Key::PageUp,
        "pgdn" | "pagedown" | "page_down" | "npage" => Key::PageDown,
        "insert" | "ins" | "ic" => Key::Insert,
        "delete" | "del" | "dc" => Key::Delete,
        "minus" => Key::Char('-'),
        "plus" => Key::Char('+'),
        "slash" => Key::Char('/'),
        "backslash" => Key::Char('\\'),
        "colon" => Key::Char(':'),
        "semicolon" => Key::Char(';'),
        "question" => Key::Char('?'),
        _ => {
            let number = lower.strip_prefix('f')?.parse::<u8>().ok()?;
            if (1..=12).contains(&number) {
                Key::F(number)
            } else {
                return None;
            }
        }
    })
}

/// Control byte for ctrl+`c`, as terminals send it.
const fn control_byte(c: char) -> Option<u8> {
    let lower = c.to_ascii_lowercase();
    match lower {
        'a'..='z' => Some(lower as u8 - b'a' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Input modes that change how keys are encoded.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyModes {
    pub application_cursor: bool,
}

/// Bytes an xterm sends for `press`.
pub fn encode_key(press: KeyPress, modes: KeyModes) -> Vec<u8> {
    let KeyPress { key, mods } = press;
    let mut out = Vec::new();
    match key {
        Key::Char(c) => {
            if mods.alt {
                out.push(0x1b);
            }
            if mods.ctrl {
                out.push(control_byte(c).unwrap_or(0));
            } else {
                let c = if mods.shift { c.to_ascii_uppercase() } else { c };
                let mut buffer = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            }
        }
        Key::Enter => alt_prefixed(&mut out, mods, b"\r"),
        Key::Tab if mods.shift => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => alt_prefixed(&mut out, mods, b"\t"),
        Key::Backspace => alt_prefixed(&mut out, mods, if mods.ctrl { b"\x08" } else { b"\x7f" }),
        Key::Escape => alt_prefixed(&mut out, mods, b"\x1b"),
        Key::Space => alt_prefixed(&mut out, mods, if mods.ctrl { b"\0" } else { b" " }),
        Key::Up => cursor_key(&mut out, b'A', mods, modes),
        Key::Down => cursor_key(&mut out, b'B', mods, modes),
        Key::Right => cursor_key(&mut out, b'C', mods, modes),
        Key::Left => cursor_key(&mut out, b'D', mods, modes),
        Key::Home => cursor_key(&mut out, b'H', mods, modes),
        Key::End => cursor_key(&mut out, b'F', mods, modes),
        Key::Insert => tilde_key(&mut out, 2, mods),
        Key::Delete => tilde_key(&mut out, 3, mods),
        Key::PageUp => tilde_key(&mut out, 5, mods),
        Key::PageDown => tilde_key(&mut out, 6, mods),
        Key::F(n @ 1..=4) => {
            let letter = b'P' + (n - 1);
            if mods.any() {
                out.extend_from_slice(format!("\x1b[1;{}", mods.param()).as_bytes());
                out.push(letter);
            } else {
                out.extend_from_slice(&[0x1b, b'O', letter]);
            }
        }
        Key::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde_key(&mut out, code, mods);
        }
    }
    out
}

fn alt_prefixed(out: &mut Vec<u8>, mods: Mods, bytes: &[u8]) {
    if mods.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
}

fn cursor_key(out: &mut Vec<u8>, letter: u8, mods: Mods, modes: KeyModes) {
    if mods.any() {
        out.extend_from_slice(format!("\x1b[1;{}", mods.param()).as_bytes());
        out.push(letter);
    } else if modes.application_cursor {
        out.extend_from_slice(&[0x1b, b'O', letter]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', letter]);
    }
}

fn tilde_key(out: &mut Vec<u8>, code: u8, mods: Mods) {
    if mods.any() {
        out.extend_from_slice(format!("\x1b[{code};{}~", mods.param()).as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
    }
}

/// Typed text as the terminal would send it: newlines become Enter (CR).
pub fn encode_text(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(b'\r');
            }
            '\n' => out.push(b'\r'),
            c => {
                let mut buffer = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out
}

/// Pasted text: wrapped in bracketed-paste markers when the program asked for them.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // A pasted end marker would end the paste early; drop any inside the text.
        let body = body.replace("\x1b[201~", "");
        [b"\x1b[200~".as_slice(), body.as_bytes(), b"\x1b[201~"].concat()
    } else {
        body.into_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    Drag(MouseButton),
    WheelUp,
    WheelDown,
}

/// The mouse reporting the program enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseModes {
    /// `press`, `press-release`, `drag`, `motion` or `off`.
    pub reporting: MouseReporting,
    pub sgr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MouseReporting {
    Off,
    Press,
    PressRelease,
    Drag,
    Motion,
}

impl MouseReporting {
    pub fn from_name(name: &str) -> Self {
        match name {
            "press" => Self::Press,
            "press-release" => Self::PressRelease,
            "drag" => Self::Drag,
            "motion" => Self::Motion,
            _ => Self::Off,
        }
    }
}

/// Bytes for a mouse event at a 1-based row and column, or `None` when the
/// program does not report that kind of event.
pub fn encode_mouse(
    action: MouseAction,
    row: u16,
    col: u16,
    mods: Mods,
    modes: MouseModes,
) -> Option<Vec<u8>> {
    let needed = match action {
        MouseAction::Press(_) | MouseAction::WheelUp | MouseAction::WheelDown => {
            MouseReporting::Press
        }
        MouseAction::Release(_) => MouseReporting::PressRelease,
        MouseAction::Drag(_) => MouseReporting::Drag,
    };
    if modes.reporting < needed {
        return None;
    }
    let button_code = |button: MouseButton| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let mut code: u32 = match action {
        MouseAction::Press(button) => button_code(button),
        MouseAction::Release(button) => {
            if modes.sgr {
                button_code(button)
            } else {
                3
            }
        }
        MouseAction::Drag(button) => 32 + button_code(button),
        MouseAction::WheelUp => 64,
        MouseAction::WheelDown => 65,
    };
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }
    if modes.sgr {
        let suffix = if matches!(action, MouseAction::Release(_)) { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{col};{row}{suffix}").into_bytes());
    }
    // Legacy X10 encoding: each value offset by 32 in one byte.
    let encode = |value: u32| u8::try_from(value + 32).ok();
    Some(vec![
        0x1b,
        b'[',
        b'M',
        encode(code)?,
        encode(u32::from(col))?,
        encode(u32::from(row))?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<u8> {
        parse_keys(&[text.to_owned()])
            .unwrap()
            .into_iter()
            .flat_map(|press| encode_key(press, KeyModes::default()))
            .collect()
    }

    #[test]
    fn control_spellings_agree() {
        assert_eq!(keys("C-c"), vec![3]);
        assert_eq!(keys("ctrl+c"), vec![3]);
        assert_eq!(keys("^C"), vec![3]);
        assert_eq!(keys("Ctrl+C"), vec![3]);
    }

    #[test]
    fn single_characters_keep_their_case() {
        assert_eq!(keys("q Q / ?"), b"qQ/?".to_vec());
        assert_eq!(keys("S-a"), b"A".to_vec());
    }

    #[test]
    fn repeats_and_named_keys() {
        assert_eq!(keys("Down*3"), b"\x1b[B\x1b[B\x1b[B".to_vec());
        assert_eq!(keys("Enter Esc Tab S-Tab"), b"\r\x1b\t\x1b[Z".to_vec());
        assert_eq!(keys("F5 F1"), b"\x1b[15~\x1bOP".to_vec());
        assert_eq!(keys("C-Left"), b"\x1b[1;5D".to_vec());
        assert_eq!(keys("M-x"), b"\x1bx".to_vec());
        assert_eq!(keys("PgDn"), b"\x1b[6~".to_vec());
    }

    #[test]
    fn application_cursor_mode_changes_arrows() {
        let press = parse_key("Up").unwrap();
        assert_eq!(
            encode_key(press, KeyModes { application_cursor: true }),
            b"\x1bOA".to_vec()
        );
    }

    #[test]
    fn unknown_keys_explain_the_syntax() {
        let error = parse_key("Hyper").unwrap_err().to_string();
        assert!(error.contains("Enter"), "{error}");
        assert!(parse_keys(&["Down*0".to_owned()]).is_err());
    }

    #[test]
    fn text_and_paste() {
        assert_eq!(encode_text("ls\n"), b"ls\r".to_vec());
        assert_eq!(encode_paste("a\nb", true), b"\x1b[200~a\rb\x1b[201~".to_vec());
        assert_eq!(encode_paste("a", false), b"a".to_vec());
    }

    #[test]
    fn mouse_follows_reporting_mode() {
        let sgr = MouseModes {
            reporting: MouseReporting::PressRelease,
            sgr: true,
        };
        let left = MouseButton::Left;
        assert_eq!(
            encode_mouse(MouseAction::Press(left), 3, 10, Mods::default(), sgr).unwrap(),
            b"\x1b[<0;10;3M".to_vec()
        );
        assert_eq!(
            encode_mouse(MouseAction::Release(left), 3, 10, Mods::default(), sgr).unwrap(),
            b"\x1b[<0;10;3m".to_vec()
        );
        let off = MouseModes {
            reporting: MouseReporting::Off,
            sgr: false,
        };
        assert!(encode_mouse(MouseAction::Press(left), 1, 1, Mods::default(), off).is_none());
        let x10 = MouseModes {
            reporting: MouseReporting::Press,
            sgr: false,
        };
        assert_eq!(
            encode_mouse(MouseAction::Press(left), 1, 1, Mods::default(), x10).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
    }
}
