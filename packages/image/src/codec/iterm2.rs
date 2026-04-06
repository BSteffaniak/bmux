//! iTerm2 inline image codec.
//!
//! iTerm2 uses OSC 1337: `ESC ] 1337 ; File = [params] : <base64-data> BEL`
//!
//! Parameters are semicolon-separated key=value pairs before the colon.
//! The base64 data after the colon is the image file (PNG, JPEG, GIF, etc.).

use crate::model::ImagePixelSize;

/// Parsed iTerm2 inline image parameters.
#[derive(Clone, Debug, Default)]
pub struct ITerm2Params {
    /// Whether to display inline (vs download).
    pub inline: bool,
    /// Desired display width (e.g., "auto", "100px", "50%", "10").
    pub width: Option<String>,
    /// Desired display height.
    pub height: Option<String>,
    /// File name (optional metadata).
    pub name: Option<String>,
    /// File size in bytes (optional metadata).
    pub size: Option<u64>,
    /// Whether to preserve aspect ratio (default true).
    pub preserve_aspect_ratio: bool,
}

/// Parse the iTerm2 OSC 1337 body (bytes after "1337;File=" and before ST).
///
/// Returns the parsed parameters and the raw image file bytes (decoded from
/// base64).
pub fn parse_body(body: &[u8]) -> Option<(ITerm2Params, Vec<u8>)> {
    // Find the colon separating params from base64 data.
    let colon_pos = body.iter().position(|&b| b == b':')?;
    let param_bytes = &body[..colon_pos];
    let b64_data = &body[colon_pos + 1..];

    let param_str = std::str::from_utf8(param_bytes).ok()?;
    let mut params = ITerm2Params {
        preserve_aspect_ratio: true, // default
        ..Default::default()
    };

    for pair in param_str.split(';') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "inline" => params.inline = value == "1",
                "width" => params.width = Some(value.to_string()),
                "height" => params.height = Some(value.to_string()),
                "name" => {
                    // Name is base64-encoded.
                    params.name = Some(
                        String::from_utf8(crate::codec::base64::base64_decode(value.as_bytes()))
                            .unwrap_or_default(),
                    );
                }
                "size" => params.size = value.parse().ok(),
                "preserveAspectRatio" => params.preserve_aspect_ratio = value != "0",
                _ => {}
            }
        }
    }

    // Decode the base64 image data.
    let image_data = crate::codec::base64::base64_decode(b64_data);

    Some((params, image_data))
}

/// Estimate the pixel size of an iTerm2 inline image.
///
/// This requires decoding the image format (PNG/JPEG header parsing).
/// Returns `None` if the format cannot be determined.
pub fn estimate_pixel_size(image_data: &[u8]) -> Option<ImagePixelSize> {
    // Try PNG header: bytes 16-23 contain width (4 bytes BE) and height (4 bytes BE)
    // after the 8-byte signature and the IHDR chunk header.
    if image_data.len() >= 24 && &image_data[0..8] == b"\x89PNG\r\n\x1a\n" {
        let width = u32::from_be_bytes([
            image_data[16],
            image_data[17],
            image_data[18],
            image_data[19],
        ]);
        let height = u32::from_be_bytes([
            image_data[20],
            image_data[21],
            image_data[22],
            image_data[23],
        ]);
        return Some(ImagePixelSize { width, height });
    }

    // Try JPEG: search for SOF0 marker (0xFF 0xC0) which contains dimensions.
    if image_data.len() >= 2 && image_data[0] == 0xFF && image_data[1] == 0xD8 {
        let mut i = 2;
        while i + 1 < image_data.len() {
            if image_data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = image_data[i + 1];
            // SOF markers: C0-C3, C5-C7, C9-CB, CD-CF
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
                && i + 9 < image_data.len()
            {
                let height = u16::from_be_bytes([image_data[i + 5], image_data[i + 6]]);
                let width = u16::from_be_bytes([image_data[i + 7], image_data[i + 8]]);
                return Some(ImagePixelSize {
                    width: width as u32,
                    height: height as u32,
                });
            }
            // Skip to next marker.
            if i + 3 < image_data.len() {
                let len = u16::from_be_bytes([image_data[i + 2], image_data[i + 3]]) as usize;
                i += 2 + len;
            } else {
                break;
            }
        }
    }

    None
}

/// Encode an image file as an iTerm2 OSC 1337 body (between "1337;File=" and ST).
pub fn encode_body(image_data: &[u8], inline: bool) -> Vec<u8> {
    let b64 = crate::codec::base64::base64_encode(image_data);
    let mut body = Vec::new();
    if inline {
        body.extend_from_slice(b"inline=1:");
    } else {
        body.extend_from_slice(b"inline=0:");
    }
    body.extend_from_slice(b64.as_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_inline_image() {
        let body = b"inline=1:AAAA";
        let (params, data) = parse_body(body).unwrap();
        assert!(params.inline);
        assert!(!data.is_empty());
    }

    #[test]
    fn png_size_detection() {
        // Minimal PNG header with 100x50 dimensions.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        // IHDR chunk: length (13), type "IHDR"
        png.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        // Width (100) and height (50) in big-endian.
        png.extend_from_slice(&100u32.to_be_bytes());
        png.extend_from_slice(&50u32.to_be_bytes());

        let size = estimate_pixel_size(&png).unwrap();
        assert_eq!(size.width, 100);
        assert_eq!(size.height, 50);
    }
}
