use std::fmt::Write as _;

/// Escape single quotes for DSL string values.
pub(super) fn escape_single_quote(s: &str) -> String {
    s.replace('\'', "\\'")
}

/// Escape bytes to C-style escape string for use in `send-keys keys='...'`.
pub(super) fn bytes_to_c_escaped(data: &[u8]) -> String {
    let mut result = String::new();
    for &byte in data {
        match byte {
            b'\r' => result.push_str("\\r"),
            b'\n' => result.push_str("\\n"),
            b'\t' => result.push_str("\\t"),
            b'\\' => result.push_str("\\\\"),
            b'\'' => result.push_str("\\'"),
            0x1b => result.push_str("\\e"),
            0x7f => result.push_str("\\x7f"),
            0x20..=0x7e => result.push(byte as char),
            _ => {
                write!(result, "\\x{byte:02x}").unwrap();
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_to_c_escaped_basic() {
        assert_eq!(bytes_to_c_escaped(b"hello\r\n"), "hello\\r\\n");
    }

    #[test]
    fn bytes_to_c_escaped_ctrl() {
        assert_eq!(bytes_to_c_escaped(&[0x01]), "\\x01");
        assert_eq!(bytes_to_c_escaped(&[0x1b]), "\\e");
    }

    #[test]
    fn bytes_to_c_escaped_mixed() {
        assert_eq!(bytes_to_c_escaped(b"echo hello\r"), "echo hello\\r");
    }

    #[test]
    fn escape_single_quote_basic() {
        assert_eq!(escape_single_quote("it's"), "it\\'s");
        assert_eq!(escape_single_quote("a\\b"), "a\\b");
    }
}
