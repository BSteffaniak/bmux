/// LEB128 variable-length integer encoding/decoding.
///
/// Uses unsigned LEB128 for unsigned types and ZigZag + LEB128 for signed types,
/// producing compact representations for small values.

/// Encode a `u64` as unsigned LEB128 into the output buffer.
pub fn encode_u64(output: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Encode a `u32` as unsigned LEB128 into the output buffer.
#[inline]
pub fn encode_u32(output: &mut Vec<u8>, value: u32) {
    encode_u64(output, u64::from(value));
}

/// Encode a `u16` as unsigned LEB128 into the output buffer.
#[inline]
pub fn encode_u16(output: &mut Vec<u8>, value: u16) {
    encode_u64(output, u64::from(value));
}

/// Encode a `usize` as unsigned LEB128 into the output buffer.
#[inline]
pub fn encode_usize(output: &mut Vec<u8>, value: usize) {
    encode_u64(output, value as u64);
}

/// ZigZag encode a signed `i64` to unsigned `u64`.
#[inline]
fn zigzag_encode_i64(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// ZigZag encode a signed `i32` to unsigned `u64`.
#[inline]
fn zigzag_encode_i32(value: i32) -> u64 {
    zigzag_encode_i64(i64::from(value))
}

/// ZigZag encode a signed `i16` to unsigned `u64`.
#[inline]
fn zigzag_encode_i16(value: i16) -> u64 {
    zigzag_encode_i64(i64::from(value))
}

/// Encode a signed `i64` using ZigZag + LEB128.
#[inline]
pub fn encode_i64(output: &mut Vec<u8>, value: i64) {
    encode_u64(output, zigzag_encode_i64(value));
}

/// Encode a signed `i32` using ZigZag + LEB128.
#[inline]
pub fn encode_i32(output: &mut Vec<u8>, value: i32) {
    encode_u64(output, zigzag_encode_i32(value));
}

/// Encode a signed `i16` using ZigZag + LEB128.
#[inline]
pub fn encode_i16(output: &mut Vec<u8>, value: i16) {
    encode_u64(output, zigzag_encode_i16(value));
}

/// Decode unsigned LEB128 from a byte slice, returning the value and bytes consumed.
///
/// # Errors
///
/// Returns `None` if the buffer is truncated or the varint exceeds 10 bytes.
pub fn decode_u64(input: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in input.iter().enumerate() {
        if shift >= 70 {
            return None; // Overflow: varint too long
        }
        result |= u64::from(byte & 0x7F) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    None // Truncated
}

/// Decode unsigned LEB128 as `u32`.
#[inline]
pub fn decode_u32(input: &[u8]) -> Option<(u32, usize)> {
    let (value, consumed) = decode_u64(input)?;
    let value = u32::try_from(value).ok()?;
    Some((value, consumed))
}

/// Decode unsigned LEB128 as `u16`.
#[inline]
pub fn decode_u16(input: &[u8]) -> Option<(u16, usize)> {
    let (value, consumed) = decode_u64(input)?;
    let value = u16::try_from(value).ok()?;
    Some((value, consumed))
}

/// Decode unsigned LEB128 as `usize`.
#[inline]
pub fn decode_usize(input: &[u8]) -> Option<(usize, usize)> {
    let (value, consumed) = decode_u64(input)?;
    let value = usize::try_from(value).ok()?;
    Some((value, consumed))
}

/// ZigZag decode unsigned `u64` to signed `i64`.
#[inline]
fn zigzag_decode_i64(value: u64) -> i64 {
    ((value >> 1) as i64) ^ (-((value & 1) as i64))
}

/// Decode ZigZag + LEB128 as `i64`.
#[inline]
pub fn decode_i64(input: &[u8]) -> Option<(i64, usize)> {
    let (raw, consumed) = decode_u64(input)?;
    Some((zigzag_decode_i64(raw), consumed))
}

/// Decode ZigZag + LEB128 as `i32`.
#[inline]
pub fn decode_i32(input: &[u8]) -> Option<(i32, usize)> {
    let (raw, consumed) = decode_u64(input)?;
    let signed = zigzag_decode_i64(raw);
    let value = i32::try_from(signed).ok()?;
    Some((value, consumed))
}

/// Decode ZigZag + LEB128 as `i16`.
#[inline]
pub fn decode_i16(input: &[u8]) -> Option<(i16, usize)> {
    let (raw, consumed) = decode_u64(input)?;
    let signed = zigzag_decode_i64(raw);
    let value = i16::try_from(signed).ok()?;
    Some((value, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_u64() {
        let cases: &[u64] = &[0, 1, 127, 128, 255, 256, 16383, 16384, u64::MAX];
        for &value in cases {
            let mut buf = Vec::new();
            encode_u64(&mut buf, value);
            let (decoded, consumed) = decode_u64(&buf).unwrap();
            assert_eq!(decoded, value, "roundtrip failed for {value}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn roundtrip_i64() {
        let cases: &[i64] = &[0, 1, -1, 63, -64, 64, -65, i64::MIN, i64::MAX];
        for &value in cases {
            let mut buf = Vec::new();
            encode_i64(&mut buf, value);
            let (decoded, consumed) = decode_i64(&buf).unwrap();
            assert_eq!(decoded, value, "roundtrip failed for {value}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn roundtrip_i32() {
        let cases: &[i32] = &[0, 1, -1, 127, -128, i32::MIN, i32::MAX];
        for &value in cases {
            let mut buf = Vec::new();
            encode_i32(&mut buf, value);
            let (decoded, consumed) = decode_i32(&buf).unwrap();
            assert_eq!(decoded, value, "roundtrip failed for {value}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn roundtrip_i16() {
        let cases: &[i16] = &[0, 1, -1, 127, -128, i16::MIN, i16::MAX];
        for &value in cases {
            let mut buf = Vec::new();
            encode_i16(&mut buf, value);
            let (decoded, consumed) = decode_i16(&buf).unwrap();
            assert_eq!(decoded, value, "roundtrip failed for {value}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn small_values_are_compact() {
        let mut buf = Vec::new();
        encode_u64(&mut buf, 0);
        assert_eq!(buf.len(), 1);

        buf.clear();
        encode_u64(&mut buf, 127);
        assert_eq!(buf.len(), 1);

        buf.clear();
        encode_u64(&mut buf, 128);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn decode_empty_returns_none() {
        assert!(decode_u64(&[]).is_none());
    }

    #[test]
    fn decode_truncated_returns_none() {
        assert!(decode_u64(&[0x80]).is_none());
        assert!(decode_u64(&[0x80, 0x80]).is_none());
    }
}
