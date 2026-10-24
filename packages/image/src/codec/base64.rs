//! Shared base64 encoding/decoding used by both kitty and iterm2 codecs.

/// Simple base64 decoder (standard alphabet, no padding required).
pub fn base64_decode(input: &[u8]) -> Vec<u8> {
    const TABLE: [u8; 256] = {
        let mut t = [255u8; 256];
        let mut i = 0u8;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'A' + i) as usize] = i;
            t[(b'a' + i) as usize] = i + 26;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 10 {
                break;
            }
            t[(b'0' + i) as usize] = i + 52;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut accum: u32 = 0;
    let mut bits: u32 = 0;

    for &byte in input {
        let val = TABLE[byte as usize];
        if val == 255 {
            continue; // skip padding and whitespace
        }
        accum = (accum << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accum >> bits) as u8);
            accum &= (1 << bits) - 1;
        }
    }

    out
}

/// Append padded base64 with a cancellation check per at most 3072 input bytes.
/// Cancellation rolls back this call's output; existing prefix bytes survive.
///
/// # Errors
/// * Returns `Interrupted` when the checkpoint requests cancellation.
pub fn base64_encode_into_cancellable(
    input: &[u8],
    out: &mut Vec<u8>,
    mut cancelled: impl FnMut() -> bool,
) -> std::io::Result<()> {
    let original = out.len();
    // All non-final boundaries are multiples of three, preserving exact padding.
    for chunk in input.chunks(3072) {
        if cancelled() {
            out.truncate(original);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "image encoding cancelled",
            ));
        }
        base64_encode_into(chunk, out);
    }
    if cancelled() {
        out.truncate(original);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "image encoding cancelled",
        ));
    }
    Ok(())
}

/// Simple base64 encoder (standard alphabet with padding).
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    base64_encode_into(input, &mut out);
    // The encoder emits only the standard ASCII alphabet and '=' padding.
    String::from_utf8(out).expect("base64 output is ASCII")
}

/// Append standard padded base64 directly to an existing byte buffer.
///
/// Existing bytes are preserved, and empty input appends nothing. Padding is
/// local to this call, so callers must choose protocol chunk boundaries first.
pub fn base64_encode_into(input: &[u8], out: &mut Vec<u8>) {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(CHARS[((triple >> 18) & 0x3F) as usize]);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize]);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize]);
        } else {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize]);
        } else {
            out.push(b'=');
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn bounded_steps_cancel_and_rollback_without_padding_changes() {
        let input: Vec<u8> = (0..20_003).map(|i| (i % 251) as u8).collect();
        let expected = base64_encode(&input);
        let mut complete = b"prefix".to_vec();
        let mut steps = 0;
        base64_encode_into_cancellable(&input, &mut complete, || {
            steps += 1;
            false
        })
        .unwrap();
        assert_eq!(&complete[6..], expected.as_bytes());
        assert_eq!(steps, input.len().div_ceil(3072) + 1);
        for stop in 1..=steps {
            let mut actual = b"prefix".to_vec();
            let mut checks = 0;
            let error = base64_encode_into_cancellable(&input, &mut actual, || {
                checks += 1;
                checks == stop
            })
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            assert_eq!(checks, stop);
            assert_eq!(actual, b"prefix");
        }
    }
}
