use std::time::Duration;

pub(crate) const STARTUP_ALT_SCREEN_GUARD_DURATION: Duration = Duration::from_secs(3);

pub(crate) fn extract_filtered_output(
    pending: &mut Vec<u8>,
    startup_guard_active: bool,
) -> Vec<u8> {
    if !startup_guard_active {
        if pending.is_empty() {
            return Vec::new();
        }

        let output = std::mem::take(pending);
        return output;
    }

    if pending.is_empty() {
        return Vec::new();
    }

    std::mem::take(pending)
}

#[cfg(test)]
mod tests {
    use super::extract_filtered_output;

    #[test]
    fn keeps_full_exit_sequence() {
        let mut pending = b"hello\x1b[?1049lworld".to_vec();
        let output = extract_filtered_output(&mut pending, true);

        assert_eq!(output, b"hello\x1b[?1049lworld");
        assert!(pending.is_empty());
    }

    #[test]
    fn flushes_all_bytes_when_guard_disabled() {
        let mut pending = b"abc\x1b[?1049l".to_vec();
        let output = extract_filtered_output(&mut pending, false);

        assert_eq!(output, b"abc\x1b[?1049l");
        assert!(pending.is_empty());
    }
}
