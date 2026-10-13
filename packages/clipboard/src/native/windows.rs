//! Native Win32 clipboard access. No process or clipboard lifetime is retained.
//!
//! Clipboard buffers are capped at 256 MiB; RGBA input is capped at 256 MiB
//! minus the 124-byte DIBV5 header. Oversized data fails rather than truncating.
//! These explicit bounds are an acceptance restriction versus Arboard.
//!
//! Format choices and the `BI_RGB`/V5 alpha-mask workaround were informed by
//! Arboard 3.6.1 (Copyright 2022 The Arboard contributors, MIT OR Apache-2.0).
//! This implementation uses independently written Win32 FFI and ownership guards.

use super::NativeImage;
use crate::ClipboardError;
use image::{ImageDecoder, ImageEncoder};
use std::ffi::c_void;
use std::io::Cursor;
use std::ptr;
use std::thread;
use std::time::Duration;

type Handle = *mut c_void;
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const MAX_BYTES: usize = 256 * 1024 * 1024;

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(owner: Handle) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardData(format: u32) -> Handle;
    fn SetClipboardData(format: u32, data: Handle) -> Handle;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn CreateWindowExW(
        extended_style: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        parameter: Handle,
    ) -> Handle;
    fn DestroyWindow(window: Handle) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    fn GlobalFree(memory: Handle) -> Handle;
    fn GlobalLock(memory: Handle) -> *mut c_void;
    fn GlobalUnlock(memory: Handle) -> i32;
    fn GlobalSize(memory: Handle) -> usize;
}

fn failed(message: impl Into<String>) -> ClipboardError {
    ClipboardError::BackendFailed {
        backend: "native-windows".into(),
        message: message.into(),
    }
}

fn system_error(operation: &str) -> ClipboardError {
    failed(format!("{operation}: {}", std::io::Error::last_os_error()))
}

struct Window(Handle);

impl Window {
    fn new() -> Result<Self, ClipboardError> {
        // A non-null owner is required: EmptyClipboard with a null owner makes
        // subsequent SetClipboardData fail. STATIC is a built-in window class;
        // HWND_MESSAGE creates an invisible, thread-local message-only window.
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: All optional arguments are null, class is NUL terminated and
        // HWND_MESSAGE (-3) is the documented pseudo-handle, not dereferenced.
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                ptr::without_provenance_mut::<c_void>(usize::MAX - 2),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if window.is_null() {
            Err(system_error("CreateWindowExW"))
        } else {
            Ok(Self(window))
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: This window is owned and destroyed on its creating thread.
        unsafe { DestroyWindow(self.0) };
    }
}

struct OpenClipboardGuard {
    _owner: Window,
}

impl OpenClipboardGuard {
    fn new() -> Result<Self, ClipboardError> {
        let owner = Window::new()?;
        for attempt in 0..6 {
            // SAFETY: Owner remains alive until after CloseClipboard.
            if unsafe { OpenClipboard(owner.0) } != 0 {
                return Ok(Self { _owner: owner });
            }
            if attempt == 5 {
                return Err(system_error("OpenClipboard after six attempts"));
            }
            thread::sleep(Duration::from_millis(5));
        }
        unreachable!("bounded retry loop returns on its last attempt")
    }

    // Borrowing the guard is the proof that this thread owns the open clipboard;
    // a static method would permit calls before OpenClipboard or after closing.
    #[allow(clippy::unused_self)]
    fn clear(&self) -> Result<(), ClipboardError> {
        // SAFETY: This thread holds the open clipboard.
        if unsafe { EmptyClipboard() } == 0 {
            Err(system_error("EmptyClipboard"))
        } else {
            Ok(())
        }
    }

    // The guard borrow keeps the clipboard open throughout handle access.
    #[allow(clippy::unused_self)]
    fn read(&self, format: u32) -> Result<Option<Vec<u8>>, ClipboardError> {
        // SAFETY: Clipboard is open throughout handle access and copying.
        unsafe {
            if IsClipboardFormatAvailable(format) == 0 {
                return Ok(None);
            }
            let handle = GetClipboardData(format);
            if handle.is_null() {
                return Err(system_error("GetClipboardData"));
            }
            let length = GlobalSize(handle);
            if length == 0 || length > MAX_BYTES {
                return Err(failed("clipboard allocation is empty or exceeds 256 MiB"));
            }
            let data = GlobalLock(handle);
            if data.is_null() {
                return Err(system_error("GlobalLock"));
            }
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), length).to_vec();
            GlobalUnlock(handle);
            // Clipboard handles are borrowed, NEVER GlobalFree'd by readers.
            Ok(Some(bytes))
        }
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: This guard uniquely owns this thread's open clipboard.
        unsafe { CloseClipboard() };
        // The owner field is destroyed only after the clipboard is closed.
    }
}

struct Allocation(Handle);

impl Allocation {
    fn new(bytes: &[u8]) -> Result<Self, ClipboardError> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(failed("clipboard data is empty or exceeds 256 MiB"));
        }
        // SAFETY: GMEM_MOVEABLE is required by SetClipboardData.
        let handle = unsafe { GlobalAlloc(2, bytes.len()) };
        if handle.is_null() {
            return Err(system_error("GlobalAlloc"));
        }
        let allocation = Self(handle);
        // SAFETY: This owned allocation has exactly bytes.len() writable bytes.
        unsafe {
            let destination = GlobalLock(handle);
            if destination.is_null() {
                return Err(system_error("GlobalLock"));
            }
            ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), bytes.len());
            GlobalUnlock(handle);
        }
        Ok(allocation)
    }

    fn publish(mut self, _open: &OpenClipboardGuard, format: u32) -> Result<(), ClipboardError> {
        // SAFETY: Allocation is unlocked and movable; clipboard is open.
        if unsafe { SetClipboardData(format, self.0) }.is_null() {
            return Err(system_error("SetClipboardData"));
        }
        // Only a successful SetClipboardData transfers ownership to Windows.
        self.0 = ptr::null_mut();
        Ok(())
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: This handle has not been transferred to the clipboard.
            unsafe { GlobalFree(self.0) };
        }
    }
}

fn png_format() -> Result<u32, ClipboardError> {
    let name = [80_u16, 78, 71, 0];
    // SAFETY: name is a NUL-terminated UTF-16 string.
    let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    if format == 0 {
        Err(system_error("RegisterClipboardFormatW(PNG)"))
    } else {
        Ok(format)
    }
}

pub(super) fn copy_text(text: &str) -> Result<(), ClipboardError> {
    let length = text
        .encode_utf16()
        .count()
        .checked_add(1)
        .and_then(|length| length.checked_mul(2))
        .filter(|&length| length <= MAX_BYTES)
        .ok_or_else(|| failed("text exceeds clipboard size limit"))?;
    let mut bytes = Vec::with_capacity(length);
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let data = Allocation::new(&bytes)?;
    let open = OpenClipboardGuard::new()?;
    open.clear()?;
    data.publish(&open, CF_UNICODETEXT)
}

fn decode_text(bytes: &[u8]) -> Result<String, ClipboardError> {
    let mut units = Vec::new();
    for pair in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            return String::from_utf16(&units).map_err(|error| failed(error.to_string()));
        }
        units.push(unit);
    }
    Err(failed("Unicode clipboard text has no NUL terminator"))
}

pub(super) fn read_text() -> Result<Option<String>, ClipboardError> {
    OpenClipboardGuard::new()?
        .read(CF_UNICODETEXT)?
        .map(|bytes| decode_text(&bytes))
        .transpose()
}

fn dimensions(image: &NativeImage) -> Result<(u32, u32), ClipboardError> {
    let length = image
        .width
        .checked_mul(image.height)
        .and_then(|n| n.checked_mul(4));
    if image.width == 0
        || image.height == 0
        || length != Some(image.bytes.len())
        || image.bytes.len() > MAX_BYTES - 124
    {
        return Err(failed(
            "invalid RGBA dimensions or image exceeds 256 MiB minus 124 bytes",
        ));
    }
    let width = u32::try_from(image.width).map_err(|error| failed(error.to_string()))?;
    let height = u32::try_from(image.height).map_err(|error| failed(error.to_string()))?;
    if width > i32::MAX.unsigned_abs() || height > i32::MAX.unsigned_abs() {
        return Err(failed("DIB dimensions exceed signed 32-bit range"));
    }
    Ok((width, height))
}

fn encode_dib(image: &NativeImage, v5: bool) -> Result<Vec<u8>, ClipboardError> {
    let (width, height) = dimensions(image)?;
    let header_size = if v5 { 124_u32 } else { 40 };
    let mut bytes = vec![0; header_size as usize];
    bytes[0..4].copy_from_slice(&header_size.to_le_bytes());
    bytes[4..8].copy_from_slice(&width.to_le_bytes());
    // Positive height and bottom-up rows interoperate with Word/WordPad.
    bytes[8..12].copy_from_slice(&height.to_le_bytes());
    bytes[12..14].copy_from_slice(&1_u16.to_le_bytes());
    bytes[14..16].copy_from_slice(&32_u16.to_le_bytes());
    let size = u32::try_from(image.bytes.len()).map_err(|error| failed(error.to_string()))?;
    bytes[20..24].copy_from_slice(&size.to_le_bytes());
    if v5 {
        bytes[16..20].copy_from_slice(&3_u32.to_le_bytes()); // BI_BITFIELDS
        for (offset, mask) in [
            (40, 0x00ff_0000_u32),
            (44, 0x0000_ff00),
            (48, 0xff),
            (52, 0xff00_0000),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
        }
        bytes[56..60].copy_from_slice(&0x7352_4742_u32.to_le_bytes()); // LCS_sRGB
        bytes[108..112].copy_from_slice(&4_u32.to_le_bytes()); // LCS_GM_IMAGES
    }
    bytes.reserve(image.bytes.len());
    for row in image.bytes.chunks_exact(image.width * 4).rev() {
        for pixel in row.chunks_exact(4) {
            bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0], if v5 { pixel[3] } else { 0 }]);
        }
    }
    Ok(bytes)
}

fn decode_dib(mut bytes: Vec<u8>) -> Result<NativeImage, ClipboardError> {
    if bytes.len() < 40 {
        return Err(failed("truncated DIB header"));
    }
    let header_size = u32::from_le_bytes(bytes[0..4].try_into().expect("four-byte slice"));
    if !matches!(header_size, 40 | 52 | 56 | 108 | 124) || bytes.len() < header_size as usize {
        return Err(failed("unsupported or truncated DIB header"));
    }
    // Some applications label a V5 bitmap BI_RGB but supply an explicit alpha
    // mask. Honor that mask; ordinary BI_RGB ignores its reserved alpha byte.
    if header_size >= 108
        && bytes[14..16] == 32_u16.to_le_bytes()
        && bytes[16..20] == 0_u32.to_le_bytes()
        && bytes[52..56] == 0xff00_0000_u32.to_le_bytes()
    {
        bytes[16..20].copy_from_slice(&3_u32.to_le_bytes());
        if bytes[40..52].iter().all(|&byte| byte == 0) {
            for (offset, mask) in [(40, 0x00ff_0000_u32), (44, 0x0000_ff00), (48, 0xff)] {
                bytes[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
            }
        }
    }
    // Use an explicit BMP pixel offset. image 0.25.10's headerless decoder
    // incorrectly skips 12 extra bytes for V4/V5 BI_BITFIELDS even though
    // those masks are INSIDE the header. A file header avoids that ambiguity.
    let bit_count = u16::from_le_bytes(bytes[14..16].try_into().expect("two-byte slice"));
    let compression = u32::from_le_bytes(bytes[16..20].try_into().expect("four-byte slice"));
    let colors = u32::from_le_bytes(bytes[32..36].try_into().expect("four-byte slice"));
    let palette_entries = if colors != 0 {
        colors
    } else if bit_count <= 8 {
        1_u32 << bit_count
    } else {
        0
    };
    let external_masks = if header_size == 40 && compression == 3 {
        12
    } else {
        0
    };
    let offset = header_size
        .checked_add(external_masks)
        .and_then(|offset| {
            palette_entries
                .checked_mul(4)
                .and_then(|palette| offset.checked_add(palette))
        })
        .filter(|&offset| offset as usize <= bytes.len())
        .ok_or_else(|| failed("invalid DIB palette or mask offset"))?;
    let offset = if header_size == 124 {
        let profile_offset =
            u32::from_le_bytes(bytes[112..116].try_into().expect("four-byte slice"));
        let profile_size = u32::from_le_bytes(bytes[116..120].try_into().expect("four-byte slice"));
        // Packed clipboard DIBs can put the profile directly before the bits;
        // BMP files instead put it after. Honor the packed layout when present.
        if profile_size != 0 && profile_offset == offset {
            offset
                .checked_add(profile_size)
                .filter(|&end| end as usize <= bytes.len())
                .ok_or_else(|| failed("truncated DIBV5 profile"))?
        } else {
            offset
        }
    } else {
        offset
    };
    let file_length = u32::try_from(bytes.len())
        .ok()
        .and_then(|n| n.checked_add(14))
        .ok_or_else(|| failed("DIB file length overflow"))?;
    let mut bmp = Vec::with_capacity(file_length as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_length.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(offset + 14).to_le_bytes());
    bmp.extend_from_slice(&bytes);
    let mut decoder = image::codecs::bmp::BmpDecoder::new(Cursor::new(bmp))
        .map_err(|error| failed(error.to_string()))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_BYTES as u64);
    decoder
        .set_limits(limits)
        .map_err(|error| failed(error.to_string()))?;
    let (width, height) = decoder.dimensions();
    let rgba_length = u64::from(width) * u64::from(height) * 4;
    if decoder.total_bytes() > MAX_BYTES as u64 || rgba_length > MAX_BYTES as u64 {
        return Err(failed("decoded DIB exceeds 256 MiB"));
    }
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(|error| failed(error.to_string()))?
        .into_rgba8();
    Ok(NativeImage {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        bytes: rgba.into_raw(),
    })
}

fn decode_png(bytes: &[u8]) -> Result<NativeImage, ClipboardError> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_BYTES as u64);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| failed(error.to_string()))?;
    let (width, height) = decoder.dimensions();
    let rgba_length = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4));
    if rgba_length.is_none_or(|length| length > MAX_BYTES as u64) {
        return Err(failed("decoded PNG exceeds 256 MiB"));
    }
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(|error| failed(error.to_string()))?
        .into_rgba8();
    if rgba.len() > MAX_BYTES {
        return Err(failed("decoded PNG exceeds 256 MiB"));
    }
    Ok(NativeImage {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        bytes: rgba.into_raw(),
    })
}

pub(super) fn copy_image(image: &NativeImage) -> Result<(), ClipboardError> {
    let (width, height) = dimensions(image)?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&image.bytes, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| failed(error.to_string()))?;
    // Complete encoding/allocation before clearing the old clipboard.
    let format = png_format()?;
    let png = Allocation::new(&png)?;
    let dibv5 = Allocation::new(&encode_dib(image, true)?)?;
    let dib = Allocation::new(&encode_dib(image, false)?)?;
    let open = OpenClipboardGuard::new()?;
    open.clear()?;
    png.publish(&open, format)?;
    dibv5.publish(&open, CF_DIBV5)?;
    dib.publish(&open, CF_DIB)
}

pub(super) fn read_image() -> Result<Option<NativeImage>, ClipboardError> {
    let format = png_format()?;
    let open = OpenClipboardGuard::new()?;
    if let Some(bytes) = open.read(format)? {
        return decode_png(&bytes).map(Some);
    }
    for format in [CF_DIBV5, CF_DIB] {
        if let Some(bytes) = open.read(format)? {
            return decode_dib(bytes).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels() -> NativeImage {
        NativeImage {
            width: 2,
            height: 2,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 31, 47, 63, 64],
        }
    }

    #[test]
    fn dibv5_preserves_channels_orientation_and_straight_alpha() {
        let image = pixels();
        let decoded = decode_dib(encode_dib(&image, true).unwrap()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.bytes, image.bytes);
    }

    #[test]
    fn emitted_dibs_use_positive_height_and_bottom_up_fixture() {
        let image = pixels();
        for (v5, header_size) in [(false, 40), (true, 124)] {
            let dib = encode_dib(&image, v5).unwrap();
            assert_eq!(&dib[8..12], &2_i32.to_le_bytes());
            let expected = if v5 {
                [255, 0, 0, 0, 63, 47, 31, 64, 0, 0, 255, 255, 0, 255, 0, 128]
            } else {
                [255, 0, 0, 0, 63, 47, 31, 0, 0, 0, 255, 0, 0, 255, 0, 0]
            };
            assert_eq!(&dib[header_size..], &expected);
            let decoded = decode_dib(dib).unwrap();
            let mut expected = image.bytes.clone();
            if !v5 {
                for pixel in expected.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
            }
            assert_eq!(decoded.bytes, expected);
        }
    }

    #[test]
    fn legacy_dib_accepts_top_down_rows() {
        let mut dib = encode_dib(&pixels(), false).unwrap();
        dib[8..12].copy_from_slice(&(-2_i32).to_le_bytes());
        let (header, pixels) = dib.split_at_mut(40);
        assert_eq!(header.len(), 40);
        for column in 0..8 {
            pixels.swap(column, 8 + column);
        }
        let decoded = decode_dib(dib).unwrap();
        let mut expected = self::pixels().bytes;
        for pixel in expected.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        assert_eq!(decoded.bytes, expected);
    }

    #[test]
    fn mislabeled_v5_alpha_is_honored() {
        let mut dib = encode_dib(&pixels(), true).unwrap();
        dib[16..20].fill(0);
        dib[40..52].fill(0);
        assert_eq!(decode_dib(dib).unwrap().bytes, pixels().bytes);
    }

    #[test]
    fn rgb24_dib_honors_row_padding_and_bottom_up_order() {
        let mut dib = vec![0; 40];
        dib[0..4].copy_from_slice(&40_u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1_i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2_i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1_u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24_u16.to_le_bytes());
        dib[20..24].copy_from_slice(&8_u32.to_le_bytes());
        dib.extend_from_slice(&[255, 0, 0, 99, 0, 0, 255, 99]);
        let decoded = decode_dib(dib).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 2));
        assert_eq!(decoded.bytes, [255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn png_preserves_straight_alpha_and_row_order() {
        let image = pixels();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&image.bytes, 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.bytes, image.bytes);
    }

    #[test]
    fn malformed_images_and_dimension_mismatch_fail() {
        assert!(decode_dib(vec![0; 12]).is_err());
        assert!(decode_dib(vec![0; 124]).is_err());
        assert!(decode_png(b"not a png").is_err());
        let mut image = pixels();
        image.width = usize::MAX;
        assert!(encode_dib(&image, true).is_err());
    }

    #[test]
    fn unicode_text_requires_valid_terminated_utf16() {
        let bytes: Vec<u8> = "a\r\n😀\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_text(&bytes).unwrap(), "a\r\n😀");
        assert_eq!(decode_text(&[0, 0]).unwrap(), "");
        assert!(decode_text(&[0, 0xd8, 0, 0]).is_err());
        assert!(decode_text(&[65, 0]).is_err());
    }
}
