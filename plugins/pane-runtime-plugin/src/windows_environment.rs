//! Windows environment-block key syntax, shared by admission and the supervisor.
//!
//! CMD inherits hidden names such as `=C:`, `=ExitCode`, and `=ExitCodeAscii`.
//! These are not ordinary `SetEnvironmentVariable` names: an environment block
//! allows one leading '=' in a nonempty key, followed by the value separator.
//! Do not restrict inherited pseudo-variables to a list of known shell names.
pub(crate) fn valid_key(key: &[u16]) -> bool {
    let name = key.strip_prefix(&[u16::from(b'=')]).unwrap_or(key);
    !name.is_empty() && !name.contains(&0) && !name.contains(&u16::from(b'='))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_pseudo_names_and_ordinary_names_are_valid() {
        for key in [
            "Path",
            "=C:",
            "=c:",
            "=ExitCode",
            "=ExitCodeAscii",
            "=ShellPrivate",
            "日本語",
        ] {
            assert!(
                valid_key(&key.encode_utf16().collect::<Vec<_>>()),
                "rejected {key:?}"
            );
        }
        // Native UTF-16 must not be lossily converted during validation.
        assert!(valid_key(&[u16::from(b'='), 0xd800]));
    }

    #[test]
    fn empty_nul_and_extra_separators_are_invalid() {
        for key in ["", "=", "==", "A=B", "=A=B", "=C:=", "A\0B", "=A\0"] {
            assert!(
                !valid_key(&key.encode_utf16().collect::<Vec<_>>()),
                "accepted {key:?}"
            );
        }
    }
}
