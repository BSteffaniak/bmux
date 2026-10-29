//! Bounded image decoding. Visible geometry is supplied solely by PreparedPresentation.
use crate::renderer::{RenderError, raster_cache::Cache};
use bmux_tui::image::{ImagePayload, ImagePixelFormat};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 4096;

fn validate(payload: &ImagePayload) -> Result<(u32, u32, usize), RenderError> {
    let (width, height) = match payload {
        ImagePayload::Png { width, height, .. } | ImagePayload::Pixels { width, height, .. } => {
            (*width, *height)
        }
    };
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err(RenderError::Capacity(
            "image dimensions exceed upload limit",
        ));
    }
    let length = width as usize * height as usize * 4;
    if length > MAX_IMAGE_BYTES {
        return Err(RenderError::Capacity("image exceeds byte limit"));
    }
    match payload {
        ImagePayload::Pixels { bytes, format, .. } => {
            let expected = match format {
                ImagePixelFormat::Rgb8 => length / 4 * 3,
                ImagePixelFormat::Rgba8 => length,
            };
            if bytes.len() != expected {
                return Err(RenderError::Image(format!(
                    "invalid pixel payload length: expected {expected}, got {}",
                    bytes.len()
                )));
            }
        }
        ImagePayload::Png { bytes, .. } if bytes.len() > MAX_IMAGE_BYTES => {
            return Err(RenderError::Capacity("encoded image exceeds byte limit"));
        }
        ImagePayload::Png { .. } => {}
    }
    Ok((width, height, length))
}

fn image_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    limits
}

/// PNG metadata is parsed during decoder construction, so limits must be supplied
/// here, not just applied after inspecting dimensions.
fn png_decoder(
    bytes: &[u8],
    limits: image::Limits,
) -> image::ImageResult<image::codecs::png::PngDecoder<std::io::Cursor<&[u8]>>> {
    image::codecs::png::PngDecoder::with_limits(std::io::Cursor::new(bytes), limits)
}

pub(crate) fn decode(payload: &ImagePayload) -> Result<(u32, u32, Vec<u8>), RenderError> {
    let (width, height, length) = validate(payload)?;
    let rgba = match payload {
        ImagePayload::Pixels {
            bytes,
            format: ImagePixelFormat::Rgba8,
            ..
        } => bytes.clone(),
        ImagePayload::Pixels {
            bytes,
            format: ImagePixelFormat::Rgb8,
            ..
        } => {
            let mut rgba = Vec::with_capacity(length);
            for pixel in bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            rgba
        }
        ImagePayload::Png { bytes, .. } => {
            use image::ImageDecoder;
            let mut limits = image_limits();
            let mut decoder = png_decoder(bytes, limits.clone())
                .map_err(|e| RenderError::Image(e.to_string()))?;
            if decoder.dimensions() != (width, height) {
                return Err(RenderError::Image(
                    "PNG declared dimensions differ from encoded dimensions".into(),
                ));
            }
            // Match ImageReader::decode's output-buffer accounting, without building
            // a second decoder or reparsing metadata under a different budget.
            limits
                .reserve(decoder.total_bytes())
                .map_err(|e| RenderError::Image(e.to_string()))?;
            decoder
                .set_limits(limits)
                .map_err(|e| RenderError::Image(e.to_string()))?;
            let image = image::DynamicImage::from_decoder(decoder)
                .map_err(|e| RenderError::Image(e.to_string()))?;
            if image.width() != width || image.height() != height {
                return Err(RenderError::Image(
                    "PNG declared dimensions differ from decoded dimensions".into(),
                ));
            }
            image.into_rgba8().into_raw()
        }
    };
    Ok((width, height, rgba))
}

/// Validated borrowed content key. Hash payload bytes once, then reuse the fingerprint
/// for frame deduplication and decoded-cache lookup. Equality always checks the payload.
#[derive(Clone, Copy)]
pub(crate) struct ContentKey<'a> {
    payload: &'a ImagePayload,
    fingerprint: u64,
}

impl PartialEq for ContentKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.payload == other.payload
    }
}
impl Eq for ContentKey<'_> {}
impl Hash for ContentKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

/// Validate before hashing, copying, or cache access.
pub(crate) fn cache_key(payload: &ImagePayload) -> Result<ContentKey<'_>, RenderError> {
    validate(payload)?;
    let content = match payload {
        ImagePayload::Png {
            bytes,
            width,
            height,
        } => (*width, *height, 0_u8, bytes),
        ImagePayload::Pixels {
            bytes,
            width,
            height,
            format,
        } => (
            *width,
            *height,
            match format {
                ImagePixelFormat::Rgb8 => 1,
                ImagePixelFormat::Rgba8 => 2,
            },
            bytes,
        ),
    };
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    Ok(ContentKey {
        payload,
        fingerprint: hasher.finish(),
    })
}

pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub(crate) struct ImageCache(Cache<ImagePayload, DecodedImage>);

impl Default for ImageCache {
    fn default() -> Self {
        Self(Cache::new(96 * 1024 * 1024, 256))
    }
}

impl ImageCache {
    #[cfg(test)]
    fn get(&mut self, payload: &ImagePayload) -> Result<Arc<DecodedImage>, RenderError> {
        self.get_with_status(cache_key(payload)?)
            .map(|(image, _hit)| image)
    }

    /// `hit` is true only when retained decoded pixels were reused. A successful miss
    /// performs one decode/conversion, even if the result is too large to retain.
    /// The validated key reuses the frame lookup's fingerprint without another scan.
    pub(crate) fn get_with_status(
        &mut self,
        content: ContentKey<'_>,
    ) -> Result<(Arc<DecodedImage>, bool), RenderError> {
        let ContentKey {
            payload,
            fingerprint,
        } = content;
        if let Some(image) = self.0.get(fingerprint, |key| key == payload) {
            return Ok((image, true));
        }
        let (width, height, rgba) = decode(payload)?;
        let image = Arc::new(DecodedImage {
            width,
            height,
            rgba,
        });
        let key = payload.clone();
        let key_bytes = match &key {
            ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => {
                bytes.capacity()
            }
        };
        self.0.insert(
            fingerprint,
            key,
            Arc::clone(&image),
            key_bytes + image.rgba.capacity(),
        );
        Ok((image, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pixel_payload(bytes: &[u8], width: u32, height: u32) -> ImagePayload {
        ImagePayload::Pixels {
            bytes: bytes.to_vec(),
            width,
            height,
            format: ImagePixelFormat::Rgba8,
        }
    }

    #[test]
    fn prehashed_keys_keep_exact_equality_on_frame_and_decode_collisions() {
        let a = pixel_payload(&[1, 2, 3, 255], 1, 1);
        let b = pixel_payload(&[4, 5, 6, 255], 1, 1);
        // Force a collision in both layers; production keys hash all payload fields.
        let a_key = ContentKey {
            payload: &a,
            fingerprint: 7,
        };
        let b_key = ContentKey {
            payload: &b,
            fingerprint: 7,
        };
        let mut frame = std::collections::HashMap::new();
        frame.insert(a_key, 1);
        frame.insert(b_key, 2);
        assert_eq!(frame.len(), 2);
        assert_eq!(frame.get(&a_key), Some(&1));
        assert_eq!(frame.get(&b_key), Some(&2));
        let mut cache = ImageCache::default();
        let (first, hit) = cache.get_with_status(a_key).unwrap();
        assert!(!hit);
        let (second, hit) = cache.get_with_status(b_key).unwrap();
        assert!(!hit);
        assert_ne!(first.rgba, second.rgba);
        let (repeated, hit) = cache.get_with_status(a_key).unwrap();
        assert!(hit);
        assert!(Arc::ptr_eq(&first, &repeated));
    }

    #[test]
    fn hit_flags_do_not_claim_reuse_when_retention_is_disabled() {
        let payload = pixel_payload(&[1, 2, 3, 4], 1, 1);
        let key = cache_key(&payload).unwrap();
        let mut cache = ImageCache(Cache::new(0, 0));
        for _ in 0..2 {
            let (image, hit) = cache.get_with_status(key).unwrap();
            assert!(!hit);
            assert_eq!(image.rgba, [1, 2, 3, 4]);
        }
    }

    #[test]
    fn identical_content_reuses_decode_but_replacement_changes_output() {
        let mut cache = ImageCache::default();
        let original = pixel_payload(&[10, 20, 30, 255], 1, 1);
        let first = cache.get(&original).unwrap();
        let repeated = cache.get(&original.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &repeated));
        // Placement/ImageKey never participates: replacing a key's content must miss.
        let replaced = cache.get(&pixel_payload(&[40, 50, 60, 255], 1, 1)).unwrap();
        assert_eq!(replaced.rgba, [40, 50, 60, 255]);
        assert!(!Arc::ptr_eq(&first, &replaced));
        assert!(Arc::ptr_eq(&first, &cache.get(&original).unwrap()));
    }

    #[test]
    fn dimensions_and_format_are_part_of_identity() {
        let mut cache = ImageCache::default();
        let bytes = [1; 16];
        let wide = cache.get(&pixel_payload(&bytes, 4, 1)).unwrap();
        let tall = cache.get(&pixel_payload(&bytes, 1, 4)).unwrap();
        assert_eq!((wide.width, tall.width), (4, 1));
        assert!(!Arc::ptr_eq(&wide, &tall));
        let rgb = ImagePayload::Pixels {
            bytes: vec![1; 12],
            width: 2,
            height: 2,
            format: ImagePixelFormat::Rgb8,
        };
        let rgba = pixel_payload(&[1; 12], 3, 1);
        assert_eq!(cache.get(&rgb).unwrap().rgba.len(), 16);
        assert_eq!(cache.get(&rgba).unwrap().rgba.len(), 12);
    }

    #[test]
    fn invalid_input_is_not_cached_or_allowed_to_displace_valid_output() {
        let mut cache = ImageCache::default();
        let original = pixel_payload(&[1, 2, 3, 4], 1, 1);
        let first = cache.get(&original).unwrap();
        for payload in [
            pixel_payload(&[], 0, 1),
            pixel_payload(&[], u32::MAX, 1),
            pixel_payload(&[], 1, 1),
            ImagePayload::Png {
                bytes: vec![1, 2, 3],
                width: 1,
                height: 1,
            },
        ] {
            assert!(cache.get(&payload).is_err());
        }
        assert!(Arc::ptr_eq(&first, &cache.get(&original).unwrap()));
    }

    #[test]
    fn retention_budget_includes_encoded_key_copy() {
        let payload = pixel_payload(&[1, 2, 3, 4], 1, 1);
        let decoded_only = Cache::<ImagePayload, DecodedImage>::one_entry_budget(4);
        let mut cache = ImageCache(Cache::new(decoded_only, 1));
        let first = cache.get(&payload).unwrap();
        let second = cache.get(&payload).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(first.rgba, second.rgba);
        let with_key_copy = Cache::<ImagePayload, DecodedImage>::one_entry_budget(8);
        let mut cache = ImageCache(Cache::new(with_key_copy, 1));
        let first = cache.get(&payload).unwrap();
        assert!(Arc::ptr_eq(&first, &cache.get(&payload).unwrap()));
    }

    fn png_fixture() -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[1, 2, 3, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    // Build ancillary metadata without another dependency; PNG CRC covers type+data.
    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        let mut crc = u32::MAX;
        for byte in &chunk[4..] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            }
        }
        chunk.extend_from_slice(&(!crc).to_be_bytes());
        chunk
    }

    #[test]
    fn png_metadata_obeys_limits_during_decoder_construction() {
        let mut bytes = png_fixture();
        let mut text = b"Description\0".to_vec();
        text.extend_from_slice(&[b'x'; 4096]);
        // Signature (8 bytes) + complete IHDR (25 bytes), before IDAT.
        bytes.splice(33..33, png_chunk(b"tEXt", &text));
        let limits = image_limits();
        assert_eq!(limits.max_alloc, Some(MAX_IMAGE_BYTES as u64));
        assert_eq!(limits.max_image_width, Some(MAX_IMAGE_EDGE));
        assert_eq!(limits.max_image_height, Some(MAX_IMAGE_EDGE));
        assert!(png_decoder(&bytes, limits.clone()).is_ok());
        // A scaled budget exercises the same constructor without a 64 MiB fixture.
        let mut small = limits;
        small.max_alloc = Some(1024);
        assert!(matches!(
            png_decoder(&bytes, small),
            Err(image::ImageError::Limits(_))
        ));
    }

    #[test]
    fn damaged_png_after_valid_ihdr_does_not_admit_or_evict_warm_entry() {
        let bytes = png_fixture();
        let payload = ImagePayload::Png {
            bytes: bytes.clone(),
            width: 1,
            height: 1,
        };
        // One slot means admitting any damaged payload would evict the valid entry.
        let mut cache = ImageCache(Cache::new(4096, 1));
        let first = cache.get(&payload).unwrap();
        let mut corrupt = bytes.clone();
        let idat = corrupt
            .windows(4)
            .position(|chunk| chunk == b"IDAT")
            .unwrap();
        corrupt[idat + 4] ^= 0xff;
        // Keep IHDR and the IDAT header but remove compressed data/trailer.
        let truncated = bytes[..idat + 4].to_vec();
        for damaged in [truncated, corrupt] {
            assert_eq!(&damaged[..33], &bytes[..33]);
            let bad = ImagePayload::Png {
                bytes: damaged,
                width: 1,
                height: 1,
            };
            // Repeating the failed request must not find an admitted bad entry.
            for _ in 0..2 {
                assert!(cache.get(&bad).is_err());
                let (warm, hit) = cache.get_with_status(cache_key(&payload).unwrap()).unwrap();
                assert!(hit);
                assert!(Arc::ptr_eq(&first, &warm));
                assert_eq!(warm.rgba, [1, 2, 3, 255]);
            }
        }
    }

    #[test]
    fn png_header_dimensions_must_match_before_decode() {
        let bytes = png_fixture();
        let mut cache = ImageCache::default();
        assert!(
            cache
                .get(&ImagePayload::Png {
                    bytes: bytes.clone(),
                    width: 2,
                    height: 1,
                })
                .is_err()
        );
        let payload = ImagePayload::Png {
            bytes,
            width: 1,
            height: 1,
        };
        let (first, hit) = cache.get_with_status(cache_key(&payload).unwrap()).unwrap();
        assert!(!hit);
        assert_eq!(first.rgba, [1, 2, 3, 255]);
        let (second, hit) = cache.get_with_status(cache_key(&payload).unwrap()).unwrap();
        assert!(hit);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn rgb_is_opaque_and_bad_lengths_rejected() {
        let payload = ImagePayload::Pixels {
            bytes: vec![10, 20, 30],
            width: 1,
            height: 1,
            format: ImagePixelFormat::Rgb8,
        };
        assert_eq!(decode(&payload).unwrap().2, [10, 20, 30, 255]);
        assert!(
            decode(&ImagePayload::Pixels {
                bytes: vec![],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8
            })
            .is_err()
        );
    }
    #[test]
    fn huge_declared_images_rejected_before_decode() {
        assert!(
            decode(&ImagePayload::Png {
                bytes: vec![],
                width: u32::MAX,
                height: 1
            })
            .is_err()
        );
    }
}
