//! Native `AppKit` pasteboard access through narrowly typed Objective-C FFI.
//!
//! Clipboard buffers are capped at 256 MiB; RGBA input is capped at 256 MiB
//! minus 124 bytes, matching the Windows DIBV5 input budget. Oversized data
//! fails rather than truncating. These bounds restrict acceptance vs Arboard.
//!
//! Every operation has its own autorelease pool; the pasteboard server retains
//! published data after the pool drains. PNG preserves straight RGBA; `AppKit`
//! handles TIFF conversion, avoiding an additional Rust TIFF codec dependency.
//! Native raster preflight reserves sixteen sample slots (at least 64 bytes/pixel)
//! plus row alignment, using first-image depth metadata from `ImageIO` for all
//! containers, including multi-page TIFF and `BigTIFF`. Only index zero is decoded.
//! This raster budget is not a total native allocator bound.
//! Arboard 3.6.1's platform implementation was consulted for format compatibility
//! (Copyright 2022 The Arboard contributors, MIT OR Apache-2.0); no code copied.

use super::NativeImage;
use crate::ClipboardError;
use image::{ImageDecoder, ImageEncoder};
use std::ffi::{CStr, c_char, c_void};
use std::io::Cursor;

type Id = *mut c_void;
type Sel = *mut c_void;
const MAX_BYTES: usize = 256 * 1024 * 1024;
const UTF8_ENCODING: usize = 4;
const TIFF_FILE_TYPE: usize = 0;
const PNG_FILE_TYPE: usize = 4;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSPasteboardTypeString: Id;
    static NSPasteboardTypePNG: Id;
    static NSPasteboardTypeTIFF: Id;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImagePropertyPixelWidth: Id;
    static kCGImagePropertyPixelHeight: Id;
    static kCGImagePropertyDepth: Id;
    fn CGImageSourceCreateWithData(data: Id, options: Id) -> Id;
    fn CGImageSourceCopyPropertiesAtIndex(source: Id, index: usize, options: Id) -> Id;
    fn CGImageSourceCreateImageAtIndex(source: Id, index: usize, options: Id) -> Id;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetBytesPerRow(image: Id) -> usize;
    fn CGImageGetHeight(image: Id) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dictionary: Id, key: Id) -> Id;
    fn CFNumberGetValue(number: Id, number_type: isize, value: *mut c_void) -> u8;
    fn CFRelease(value: Id);
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_autoreleasePoolPush() -> Id;
    fn objc_autoreleasePoolPop(pool: Id);
    fn objc_msgSend();
}

fn failed(message: impl Into<String>) -> ClipboardError {
    ClipboardError::BackendFailed {
        backend: "native-macos".into(),
        message: message.into(),
    }
}

struct Pool(Id);

impl Pool {
    fn new() -> Self {
        // SAFETY: Objective-C's pool runtime works on any calling thread.
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: Pools are pushed/popped in stack order on the same thread.
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

struct CoreOwned(Id);

impl Drop for CoreOwned {
    fn drop(&mut self) {
        // SAFETY: Create/Copy-rule CoreFoundation object with one owned reference.
        unsafe { CFRelease(self.0) };
    }
}

struct Owned(Id);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: This object came from alloc/init and has one owned reference.
        unsafe { send_void(self.0, c"release") };
    }
}

// objc_msgSend must be cast to the EXACT method ABI for every call. These
// wrappers cover only pointer, NSInteger/NSUInteger, void and BOOL results;
// no floating-point or struct-return ABI is used. Method names are internal
// constants and each call site establishes its receiver/argument types.
unsafe fn selector(name: &CStr) -> Sel {
    // SAFETY: name is a static NUL-terminated selector name.
    unsafe { sel_registerName(name.as_ptr()) }
}

unsafe fn send_id(receiver: Id, name: &CStr) -> Id {
    // SAFETY: Caller guarantees the method returns an Objective-C object.
    let send: unsafe extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(receiver, selector(name)) }
}

unsafe fn send_id_arg(receiver: Id, name: &CStr, argument: Id) -> Id {
    // SAFETY: Caller guarantees an object argument and object return type.
    let send: unsafe extern "C" fn(Id, Sel, Id) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(receiver, selector(name), argument) }
}

unsafe fn send_void(receiver: Id, name: &CStr) {
    // SAFETY: Caller guarantees a zero-argument method returning void.
    let send: unsafe extern "C" fn(Id, Sel) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(receiver, selector(name)) };
}

unsafe fn send_integer(receiver: Id, name: &CStr) -> isize {
    // SAFETY: Caller guarantees an NSInteger-returning, zero-argument method.
    let send: unsafe extern "C" fn(Id, Sel) -> isize =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(receiver, selector(name)) }
}

unsafe fn send_length(receiver: Id, name: &CStr) -> usize {
    // SAFETY: Caller guarantees an NSUInteger-returning, zero-argument method.
    let send: unsafe extern "C" fn(Id, Sel) -> usize =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(receiver, selector(name)) }
}

unsafe fn send_boolean(receiver: Id, name: &CStr) -> bool {
    // SAFETY: Caller guarantees a zero-argument method returning BOOL.
    let send: unsafe extern "C" fn(Id, Sel) -> i8 =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(receiver, selector(name)) != 0 }
}

unsafe fn set_value(pasteboard: Id, name: &CStr, value: Id, format: Id) -> bool {
    // SAFETY: NSPasteboard's setData:forType: / setString:forType: take two
    // objects and return BOOL (one byte on both supported macOS architectures).
    let send: unsafe extern "C" fn(Id, Sel, Id, Id) -> i8 =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(pasteboard, selector(name), value, format) != 0 }
}

fn class(name: &CStr) -> Result<Id, ClipboardError> {
    // SAFETY: name is a NUL-terminated Objective-C class name.
    let class = unsafe { objc_getClass(name.as_ptr()) };
    if class.is_null() {
        Err(failed(format!(
            "Objective-C class {} unavailable",
            name.to_string_lossy()
        )))
    } else {
        Ok(class)
    }
}

fn pasteboard() -> Result<Id, ClipboardError> {
    let class = class(c"NSPasteboard")?;
    // SAFETY: NSPasteboard +generalPasteboard returns an autoreleased object.
    let pasteboard = unsafe { send_id(class, c"generalPasteboard") };
    if pasteboard.is_null() {
        Err(failed("NSPasteboard generalPasteboard returned nil"))
    } else {
        Ok(pasteboard)
    }
}

fn data(bytes: &[u8]) -> Result<Id, ClipboardError> {
    if bytes.len() > MAX_BYTES {
        return Err(failed("pasteboard data exceeds 256 MiB"));
    }
    let class = class(c"NSData")?;
    // SAFETY: NSData +dataWithBytes:length: copies the borrowed input buffer.
    let result = unsafe {
        let send: unsafe extern "C" fn(Id, Sel, *const u8, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(
            class,
            selector(c"dataWithBytes:length:"),
            bytes.as_ptr(),
            bytes.len(),
        )
    };
    if result.is_null() {
        Err(failed("NSData allocation failed"))
    } else {
        Ok(result)
    }
}

fn string(text: &str) -> Result<Owned, ClipboardError> {
    if text.len() > MAX_BYTES {
        return Err(failed("pasteboard text exceeds 256 MiB"));
    }
    let class = class(c"NSString")?;
    // SAFETY: NSString initWithBytes:length:encoding: copies the buffer,
    // accepts embedded NULs, and returns an owned reference (or nil).
    let result = unsafe {
        let allocated = send_id(class, c"alloc");
        let send: unsafe extern "C" fn(Id, Sel, *const u8, usize, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(
            allocated,
            selector(c"initWithBytes:length:encoding:"),
            text.as_ptr(),
            text.len(),
            UTF8_ENCODING,
        )
    };
    if result.is_null() {
        Err(failed("NSString UTF-8 allocation failed"))
    } else {
        Ok(Owned(result))
    }
}

fn bytes_from_data(value: Id) -> Result<Vec<u8>, ClipboardError> {
    // SAFETY: All callers supply an NSData instance retained by the pool or
    // pasteboard. The buffer is copied before either lifetime ends.
    unsafe {
        let length = send_length(value, c"length");
        if length > MAX_BYTES {
            return Err(failed("pasteboard data exceeds 256 MiB"));
        }
        if length == 0 {
            return Ok(Vec::new());
        }
        let bytes = send_id(value, c"bytes").cast::<u8>();
        if bytes.is_null() {
            return Err(failed("NSData bytes returned null"));
        }
        Ok(std::slice::from_raw_parts(bytes, length).to_vec())
    }
}

fn clear(pasteboard: Id) -> Result<(), ClipboardError> {
    // SAFETY: NSPasteboard -clearContents returns an NSInteger change count.
    if unsafe { send_integer(pasteboard, c"clearContents") } < 0 {
        Err(failed("NSPasteboard clearContents failed"))
    } else {
        Ok(())
    }
}

pub(super) fn copy_text(text: &str) -> Result<(), ClipboardError> {
    let _pool = Pool::new();
    let value = string(text)?;
    let pasteboard = pasteboard()?;
    clear(pasteboard)?;
    // SAFETY: Both receiver and arguments are live Foundation/AppKit objects.
    if unsafe {
        set_value(
            pasteboard,
            c"setString:forType:",
            value.0,
            NSPasteboardTypeString,
        )
    } {
        Ok(())
    } else {
        Err(failed("NSPasteboard setString:forType: failed"))
    }
}

fn decode_text(bytes: &[u8]) -> Result<String, ClipboardError> {
    String::from_utf8(bytes.to_vec()).map_err(|error| failed(error.to_string()))
}

pub(super) fn read_text() -> Result<Option<String>, ClipboardError> {
    let _pool = Pool::new();
    let pasteboard = pasteboard()?;
    // SAFETY: stringForType returns a live NSString or nil. The UTF8String
    // pointer remains valid while the autorelease pool and string are alive.
    unsafe {
        let value = send_id_arg(pasteboard, c"stringForType:", NSPasteboardTypeString);
        if value.is_null() {
            return Ok(None);
        }
        let send: unsafe extern "C" fn(Id, Sel, usize) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        let length = send(
            value,
            selector(c"lengthOfBytesUsingEncoding:"),
            UTF8_ENCODING,
        );
        if length > MAX_BYTES {
            return Err(failed("pasteboard text exceeds 256 MiB"));
        }
        let bytes = send_id(value, c"UTF8String").cast::<u8>();
        if bytes.is_null() {
            return Err(failed("NSString UTF-8 conversion failed"));
        }
        // Use the explicit length, not CStr: embedded NULs are valid NSString.
        decode_text(std::slice::from_raw_parts(bytes, length)).map(Some)
    }
}

fn encode_png(image: &NativeImage) -> Result<Vec<u8>, ClipboardError> {
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
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&image.bytes, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| failed(error.to_string()))?;
    if bytes.len() > MAX_BYTES {
        return Err(failed("encoded PNG exceeds 256 MiB"));
    }
    Ok(bytes)
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
        return Err(failed("decoded image exceeds 256 MiB"));
    }
    Ok(NativeImage {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        bytes: rgba.into_raw(),
    })
}

fn native_raster_budget(
    width: usize,
    height: usize,
    bits: usize,
    samples: usize,
) -> Result<(), ClipboardError> {
    if width == 0 || height == 0 || bits == 0 || bits > 64 || samples == 0 || samples > 16 {
        return Err(failed(
            "unsupported native bitmap dimensions, depth or sample count",
        ));
    }
    // Allow native expansion to at least four 32-bit samples, or the actual
    // source precision/channel count if larger. Include 64-byte row alignment
    // conservatively, even for planar inputs. This is a raster budget, not a
    // claim that ImageIO/AppKit's total process allocation is bounded to it.
    let sample_bytes = if bits <= 32 { 4_usize } else { 8 };
    let row = sample_bytes
        .checked_mul(samples.max(4))
        .and_then(|n| n.checked_mul(width))
        .and_then(|n| n.checked_add(63))
        .map(|n| n & !63);
    let length = row.and_then(|n| n.checked_mul(height));
    if length.is_none_or(|n| n > MAX_BYTES) {
        return Err(failed("native raster budget exceeds 256 MiB"));
    }
    Ok(())
}

fn preflight_bitmap(input: Id) -> Result<CoreOwned, ClipboardError> {
    // ImageIO creates a metadata source without decoding pixels. Preflight the
    // dimensions BEFORE NSBitmapImageRep can allocate a native raster. NSData
    // is toll-free bridged to CFData; options are deliberately null defaults.
    unsafe {
        let source = CGImageSourceCreateWithData(input, std::ptr::null_mut());
        if source.is_null() {
            return Err(failed("ImageIO could not inspect image metadata"));
        }
        let source = CoreOwned(source);
        let properties = CGImageSourceCopyPropertiesAtIndex(source.0, 0, std::ptr::null_mut());
        if properties.is_null() {
            return Err(failed("ImageIO image dimensions are unavailable"));
        }
        let properties = CoreOwned(properties);
        let mut dimensions = [0_i64; 3];
        for (key, dimension) in [
            kCGImagePropertyPixelWidth,
            kCGImagePropertyPixelHeight,
            kCGImagePropertyDepth,
        ]
        .into_iter()
        .zip(&mut dimensions)
        {
            let number = CFDictionaryGetValue(properties.0, key);
            // kCFNumberSInt64Type = 4, with an exactly sized output pointer.
            if number.is_null()
                || CFNumberGetValue(number, 4, std::ptr::from_mut(dimension).cast()) == 0
            {
                return Err(failed("ImageIO image dimensions are invalid"));
            }
        }
        let width = usize::try_from(dimensions[0]).map_err(|error| failed(error.to_string()))?;
        let height = usize::try_from(dimensions[1]).map_err(|error| failed(error.to_string()))?;
        let depth = usize::try_from(dimensions[2]).map_err(|error| failed(error.to_string()))?;
        // ImageIO exposes depth for the FIRST image independently of TIFF
        // directory encoding. Reserve all sixteen supported native sample slots
        // rather than assuming RGBA or relying on classic-TIFF tag parsing.
        native_raster_budget(width, height, depth, 16)?;
        Ok(source)
    }
}

fn bitmap_representation(input: Id, file_type: usize) -> Result<Id, ClipboardError> {
    let source = preflight_bitmap(input)?;
    let bitmap_class = class(c"NSBitmapImageRep")?;
    // SAFETY: input is NSData. AppKit decodes compatible image formats into a
    // bitmap, then emits a lossless representation with native color/alpha
    // conversion. The returned rep/data are autoreleased within our pool.
    unsafe {
        // Decode exactly image zero: do not ask AppKit to decode a whole
        // multi-page container or to choose another representation.
        let image = CGImageSourceCreateImageAtIndex(source.0, 0, std::ptr::null_mut());
        if image.is_null() {
            return Err(failed("ImageIO could not decode first pasteboard image"));
        }
        let image = CoreOwned(image);
        if CGImageGetBytesPerRow(image.0)
            .checked_mul(CGImageGetHeight(image.0))
            .is_none_or(|length| length > MAX_BYTES)
        {
            return Err(failed("decoded native image storage exceeds 256 MiB"));
        }
        let bitmap = send_id_arg(
            send_id(bitmap_class, c"alloc"),
            c"initWithCGImage:",
            image.0,
        );
        if bitmap.is_null() {
            return Err(failed("NSBitmapImageRep could not initialize first image"));
        }
        let owned_bitmap = Owned(bitmap);
        let width = usize::try_from(send_integer(bitmap, c"pixelsWide"))
            .map_err(|error| failed(error.to_string()))?;
        let height = usize::try_from(send_integer(bitmap, c"pixelsHigh"))
            .map_err(|error| failed(error.to_string()))?;
        let bits = usize::try_from(send_integer(bitmap, c"bitsPerSample"))
            .map_err(|error| failed(error.to_string()))?;
        let samples = usize::try_from(send_integer(bitmap, c"samplesPerPixel"))
            .map_err(|error| failed(error.to_string()))?;
        native_raster_budget(width, height, bits, samples)?;
        let row = usize::try_from(send_integer(bitmap, c"bytesPerRow"))
            .map_err(|error| failed(error.to_string()))?;
        let planes = if send_boolean(bitmap, c"isPlanar") {
            samples
        } else {
            1
        };
        if row
            .checked_mul(height)
            .and_then(|n| n.checked_mul(planes))
            .is_none_or(|n| n > MAX_BYTES)
        {
            return Err(failed("native bitmap storage exceeds 256 MiB"));
        }
        let properties = send_id(class(c"NSDictionary")?, c"dictionary");
        if properties.is_null() {
            return Err(failed("NSDictionary allocation failed"));
        }
        let send: unsafe extern "C" fn(Id, Sel, usize, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        let output = send(
            owned_bitmap.0,
            selector(c"representationUsingType:properties:"),
            file_type,
            properties,
        );
        if output.is_null() {
            Err(failed("NSBitmapImageRep representation conversion failed"))
        } else {
            // Reject an oversized encoded representation before publication.
            if send_length(output, c"length") > MAX_BYTES {
                return Err(failed("native encoded image exceeds 256 MiB"));
            }
            Ok(output)
        }
    }
}

pub(super) fn copy_image(image: &NativeImage) -> Result<(), ClipboardError> {
    let png = encode_png(image)?;
    let _pool = Pool::new();
    let png = data(&png)?;
    // Prepare BOTH representations before clearing existing contents. TIFF is
    // necessary for applications (including Arboard) that only request TIFF.
    let tiff = bitmap_representation(png, TIFF_FILE_TYPE)?;
    let pasteboard = pasteboard()?;
    clear(pasteboard)?;
    // SAFETY: NSData and NSPasteboardType constants remain valid in this pool;
    // setData copies/retains the published data into the pasteboard server.
    unsafe {
        if !set_value(pasteboard, c"setData:forType:", png, NSPasteboardTypePNG) {
            return Err(failed("NSPasteboard failed to publish PNG"));
        }
        if !set_value(pasteboard, c"setData:forType:", tiff, NSPasteboardTypeTIFF) {
            return Err(failed("NSPasteboard failed to publish TIFF"));
        }
    }
    Ok(())
}

pub(super) fn read_image() -> Result<Option<NativeImage>, ClipboardError> {
    let _pool = Pool::new();
    let pasteboard = pasteboard()?;
    // SAFETY: dataForType returns NSData or nil; objects remain valid until the
    // pool drains. PNG takes priority to avoid unnecessary native conversion.
    unsafe {
        let png = send_id_arg(pasteboard, c"dataForType:", NSPasteboardTypePNG);
        if !png.is_null() {
            return decode_png(&bytes_from_data(png)?).map(Some);
        }
        let tiff = send_id_arg(pasteboard, c"dataForType:", NSPasteboardTypeTIFF);
        if tiff.is_null() {
            return Ok(None);
        }
        if send_length(tiff, c"length") > MAX_BYTES {
            return Err(failed("pasteboard TIFF exceeds 256 MiB"));
        }
        let png = bitmap_representation(tiff, PNG_FILE_TYPE)?;
        decode_png(&bytes_from_data(png)?).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_budget_accounts_for_precision_channels_padding_and_overflow() {
        assert!(native_raster_budget(1, 1, 8, 4).is_ok());
        assert!(native_raster_budget(4096, 4096, 8, 4).is_ok());
        // First-image preflight reserves sixteen slots without parsing the
        // container's TIFF directory form or counting/decompressing later pages.
        assert!(native_raster_budget(1024, 1024, 16, 16).is_ok());
        assert!(native_raster_budget(4096, 4096, 16, 16).is_err());
        // RGBA8 would fit, but eight 64-bit TIFF samples do not.
        assert!(native_raster_budget(4096, 4096, 64, 8).is_err());
        assert!(native_raster_budget(usize::MAX, 1, 8, 4).is_err());
        assert!(native_raster_budget(1, usize::MAX, 8, 4).is_err());
        assert!(native_raster_budget(0, 1, 8, 4).is_err());
        assert!(native_raster_budget(1, 1, 65, 4).is_err());
        assert!(native_raster_budget(1, 1, 16, 17).is_err());
    }

    #[test]
    fn png_preserves_orientation_channels_and_straight_alpha() {
        let image = NativeImage {
            width: 2,
            height: 2,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 31, 47, 63, 64],
        };
        let decoded = decode_png(&encode_png(&image).unwrap()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.bytes, image.bytes);
    }

    #[test]
    fn invalid_dimensions_and_corrupt_png_are_errors() {
        let image = NativeImage {
            width: usize::MAX,
            height: 2,
            bytes: vec![0; 4],
        };
        assert!(encode_png(&image).is_err());
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn utf8_retains_embedded_nuls_and_non_bmp_characters() {
        assert_eq!(
            decode_text("before\0😀\r\nafter".as_bytes()).unwrap(),
            "before\0😀\r\nafter"
        );
        assert_eq!(decode_text(b"").unwrap(), "");
        assert!(decode_text(&[0xff]).is_err());
    }

    #[test]
    fn first_image_conversion_accepts_classic_and_big_multipage_tiff() {
        // Two uncompressed grayscale 1x1 images: first white, second black.
        // All fields are inline; only strip offsets point outside directories.
        for big in [false, true] {
            let header_size = if big { 16_u64 } else { 8 };
            let directory_size = if big { 196_u64 } else { 114 };
            let pixels = header_size + directory_size * 2;
            let mut bytes = b"II".to_vec();
            if big {
                bytes.extend_from_slice(&43_u16.to_le_bytes());
                bytes.extend_from_slice(&8_u16.to_le_bytes());
                bytes.extend_from_slice(&0_u16.to_le_bytes());
                bytes.extend_from_slice(&header_size.to_le_bytes());
            } else {
                bytes.extend_from_slice(&42_u16.to_le_bytes());
                bytes.extend_from_slice(&u32::try_from(header_size).unwrap().to_le_bytes());
            }
            for page in 0..2_u64 {
                if big {
                    bytes.extend_from_slice(&9_u64.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&9_u16.to_le_bytes());
                }
                for (tag, kind, value) in [
                    (256_u16, 4_u16, 1_u64),
                    (257, 4, 1),
                    (258, 3, 8),
                    (259, 3, 1),
                    (262, 3, 1),
                    (273, if big { 16 } else { 4 }, pixels + page),
                    (277, 3, 1),
                    (278, 4, 1),
                    (279, 4, 1),
                ] {
                    bytes.extend_from_slice(&tag.to_le_bytes());
                    bytes.extend_from_slice(&kind.to_le_bytes());
                    if big {
                        bytes.extend_from_slice(&1_u64.to_le_bytes());
                        bytes.extend_from_slice(&value.to_le_bytes());
                    } else {
                        bytes.extend_from_slice(&1_u32.to_le_bytes());
                        bytes.extend_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
                    }
                }
                let next = if page == 0 {
                    header_size + directory_size
                } else {
                    0
                };
                if big {
                    bytes.extend_from_slice(&next.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&u32::try_from(next).unwrap().to_le_bytes());
                }
            }
            assert_eq!(bytes.len(), usize::try_from(pixels).unwrap());
            bytes.extend_from_slice(&[255, 0]);
            let _pool = Pool::new();
            let source = data(&bytes).unwrap();
            let png = bitmap_representation(source, PNG_FILE_TYPE).unwrap();
            let decoded = decode_png(&bytes_from_data(png).unwrap()).unwrap();
            assert_eq!((decoded.width, decoded.height), (1, 1));
            assert_eq!(decoded.bytes, [255, 255, 255, 255]);
        }
    }

    #[test]
    fn appkit_tiff_conversion_preserves_pixels_without_touching_clipboard() {
        let image = NativeImage {
            width: 2,
            height: 2,
            // Saturated channels distinguish straight from premultiplied alpha
            // without depending on lossy rounding of intermediate components.
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 0, 0, 255, 0, 255, 85],
        };
        let _pool = Pool::new();
        let png = data(&encode_png(&image).unwrap()).unwrap();
        let tiff = bitmap_representation(png, TIFF_FILE_TYPE).unwrap();
        let png = bitmap_representation(tiff, PNG_FILE_TYPE).unwrap();
        let decoded = decode_png(&bytes_from_data(png).unwrap()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.bytes, image.bytes);
    }
}
