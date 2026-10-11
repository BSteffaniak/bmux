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
