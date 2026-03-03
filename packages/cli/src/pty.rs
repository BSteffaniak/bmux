use std::time::Duration;

pub(crate) const ALT_SCREEN_EXIT_SEQUENCE: &[u8] = b"\x1b[?1049l";
pub(crate) const STARTUP_ALT_SCREEN_GUARD_DURATION: Duration = Duration::from_secs(3);

pub(crate) fn extract_filtered_output(
    pending: &mut Vec<u8>,
    startup_guard_active: bool,
) -> (Vec<u8>, bool) {
    if !startup_guard_active {
        if pending.is_empty() {
            return (Vec::new(), false);
        }

        let output = std::mem::take(pending);
        return (output, false);
    }

    let mut output = Vec::new();
    let mut offset = 0;
    let mut dropped_exit_sequence = false;

    while offset < pending.len() {
        if pending[offset..].starts_with(ALT_SCREEN_EXIT_SEQUENCE) {
            dropped_exit_sequence = true;
            offset += ALT_SCREEN_EXIT_SEQUENCE.len();
            continue;
        }

        if pending[offset] == ALT_SCREEN_EXIT_SEQUENCE[0]
            && is_prefix_of_alt_screen_exit_sequence(&pending[offset..])
            && pending.len() - offset < ALT_SCREEN_EXIT_SEQUENCE.len()
        {
            break;
        }

        output.push(pending[offset]);
        offset += 1;
    }

    if offset > 0 {
        pending.drain(0..offset);
    }

    (output, dropped_exit_sequence)
}

fn is_prefix_of_alt_screen_exit_sequence(bytes: &[u8]) -> bool {
    ALT_SCREEN_EXIT_SEQUENCE.starts_with(bytes)
}

#[cfg(test)]
mod tests {
    use super::{ALT_SCREEN_EXIT_SEQUENCE, extract_filtered_output};

    #[test]
    fn drops_full_exit_sequence() {
        let mut pending = b"hello\x1b[?1049lworld".to_vec();
        let (output, dropped) = extract_filtered_output(&mut pending, true);

        assert!(dropped);
        assert_eq!(output, b"helloworld");
        assert!(pending.is_empty());
    }

    #[test]
    fn keeps_partial_sequence_until_complete() {
        let mut pending = ALT_SCREEN_EXIT_SEQUENCE[..5].to_vec();
        let (output, dropped) = extract_filtered_output(&mut pending, true);

        assert!(!dropped);
        assert!(output.is_empty());
        assert_eq!(pending, ALT_SCREEN_EXIT_SEQUENCE[..5]);
    }

    #[test]
    fn flushes_all_bytes_when_guard_disabled() {
        let mut pending = b"abc\x1b[?1049l".to_vec();
        let (output, dropped) = extract_filtered_output(&mut pending, false);

        assert!(!dropped);
        assert_eq!(output, b"abc\x1b[?1049l");
        assert!(pending.is_empty());
    }
}
