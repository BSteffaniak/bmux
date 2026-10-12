#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiny_tail_is_durable_but_not_complete_until_real_eof() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"TAIL").unwrap();
        let (boundary, bytes) = DurableCapture::read(root.path(), 0, 4).unwrap();
        assert_eq!(bytes, b"TAIL");
        assert_eq!(boundary.bytes, 4);
        assert!(!boundary.eof);
        capture.end(None).unwrap();
        let (boundary, bytes) = DurableCapture::read(root.path(), 0, 4).unwrap();
        assert!(boundary.eof);
        assert_eq!(bytes, b"TAIL");
    }
    #[test]
    fn backpressure_sized_capture_retains_exact_tail_and_rejects_truncation() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        for _ in 0..40 {
            capture.append(&[42; 8192]).unwrap();
        }
        capture.append(b"TAIL").unwrap();
        capture.end(None).unwrap();
        let (boundary, tail) = DurableCapture::read(root.path(), 327_680, 4).unwrap();
        assert_eq!(boundary.bytes, 327_684);
        assert!(boundary.eof);
        assert_eq!(tail, b"TAIL");
        capture.file.set_len(1).unwrap();
        assert!(DurableCapture::read(root.path(), 0, 4).is_err());
    }
    #[test]
    fn ordered_resize_preserves_raw_projection_and_time() {
        use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::{
            TerminalCaptureEvent, TerminalCaptureEventKind,
        };
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"before").unwrap();
        capture.resize(103, 31).unwrap();
        capture.append(b"after").unwrap();
        capture.end(None).unwrap();
        let (boundary, raw) = DurableCapture::read(root.path(), 0, 64).unwrap();
        assert_eq!(raw, b"beforeafter");
        assert_eq!(boundary.bytes, 11);
        let (_, bytes) = DurableCapture::read_events(root.path(), 0, 262_144).unwrap();
        let mut input = bytes.as_slice();
        let mut events = Vec::new();
        while !input.is_empty() {
            let length =
                usize::try_from(u32::from_le_bytes(input[..4].try_into().unwrap())).unwrap();
            events.push(
                serde_json::from_slice::<TerminalCaptureEvent>(&input[4..4 + length]).unwrap(),
            );
            input = &input[4 + length..];
        }
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[1].event,
            TerminalCaptureEventKind::Resize {
                cols: 103,
                rows: 31
            }
        ));
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].mono_ns <= pair[1].mono_ns)
        );
        std::fs::write(
            root.path().join("capture.json"),
            br#"{"version":4,"bytes":11,"eof":true,"error":null}"#,
        )
        .unwrap();
        assert!(DurableCapture::read(root.path(), 0, 64).is_err());
    }
    #[test]
    fn reader_error_never_manufactures_complete_capture() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = DurableCapture::create(root.path()).unwrap();
        capture.append(b"prefix").unwrap();
        capture.end(Some("read failed".into())).unwrap();
        let (boundary, bytes) = DurableCapture::read(root.path(), 0, 64).unwrap();
        assert!(!boundary.eof);
        assert!(boundary.error.is_some());
        assert_eq!(bytes, b"prefix");
    }
}
