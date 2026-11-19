use super::{decode_other, transcode_other};

// Color types and byte lengths recorded from DynamicImage::write_to(Png) in image 0.25.10.
fn assert_png_layout(
    bytes: &[u8],
    width: u32,
    height: u32,
    budget: u64,
    color_type: u8,
    png_len: usize,
) {
    let encoded = transcode_other(bytes, width, height, budget).unwrap();
    assert_eq!((encoded[24], encoded[25]), (8, color_type));
    assert_eq!(encoded.len(), png_len);
    let pixels = crate::decode(&encoded, width, height, 1 << 20).unwrap();
    assert_eq!(pixels, decode_other(bytes, width, height, 1 << 20).unwrap());
}

#[test]
fn jpeg_png_layout_and_native_allocation_limit() {
    for (jpeg, channels, color, len) in [
        (
            include_bytes!("fixtures/jpeg-gray.jpg").as_slice(),
            1,
            0,
            138,
        ),
        (include_bytes!("fixtures/jpeg.jpg").as_slice(), 3, 2, 264),
    ] {
        let budget = 9 * 7 * channels;
        assert_png_layout(jpeg, 9, 7, budget, color, len);
        assert!(matches!(
            transcode_other(jpeg, 9, 7, budget - 1),
            Err(crate::TranscodeError::Decode(_))
        ));
    }
}

#[test]
fn first_gif_frame_is_placed_on_logical_screen() {
    // Two 1x1 frames on a 2x1 screen. Uncovered canvas must be transparent.
    let gif=b"GIF89a\x02\0\x01\0\x80\0\0\xff\0\0\0\xff\0,\x01\0\0\0\x01\0\x01\0\0\x02\x02\x44\x01\0,\0\0\0\0\x01\0\x01\0\0\x02\x02\x4c\x01\0;";
    assert_png_layout(gif, 2, 1, 12, 6, 77);
    let raster = decode_other(gif, 2, 1, 12).unwrap();
    assert_eq!(raster.rgba, [0, 0, 0, 0, 255, 0, 0, 255]);
    assert!(decode_other(gif, 2, 1, 11).is_err());
    assert!(decode_other(gif, 1, 1, 12).is_err());
}

#[test]
fn bmp_bitfields_keep_alpha_and_bounds() {
    let mut bmp = vec![0; 74];
    bmp[..2].copy_from_slice(b"BM");
    for (offset, v) in [
        (10, 70u32),
        (14, 56),
        (18, 1),
        (22, 1),
        (30, 3),
        (54, 255),
        (58, 0xff00),
        (62, 0xff0000),
        (66, 0xff000000),
    ] {
        bmp[offset..offset + 4].copy_from_slice(&v.to_le_bytes());
    }
    bmp[26] = 1;
    bmp[28] = 32;
    bmp[70..].copy_from_slice(&[21, 47, 89, 128]);
    assert_eq!(decode_other(&bmp, 1, 1, 4).unwrap().rgba, [21, 47, 89, 128]);
    assert!(decode_other(&bmp[..73], 1, 1, 4).is_err());
    assert!(decode_other(&bmp, 1, 1, 3).is_err());
    assert_png_layout(&bmp, 1, 1, 4, 6, 73);
    bmp[66..70].fill(0); // No alpha mask: same 32-bit container, RGB output.
    assert_png_layout(&bmp, 1, 1, 3, 2, 72);
}

#[test]
fn zero_sized_first_gif_frame_leaves_a_transparent_screen() {
    // 4x1 screen; the first frame is 5x0 at (1, 3). The reference decoder
    // accepts it and returns the cleared logical screen.
    let gif = b"GIF89a\x04\0\x01\0\x80\0\0\x3c\x80\xeb\x48\xc8\xdd,\x01\0\x03\0\x05\0\0\0\x40\x02\x01\x2c\0;";
    let raster = decode_other(gif, 4, 1, 16).unwrap();
    assert_eq!((raster.width, raster.height), (4, 1));
    assert_eq!(raster.rgba, [0; 16]);
}

/// An 8-bit RLE BMP, `width` x `height`, whose palette index 1 is RGB (10, 20, 30).
fn rle8_bmp(width: u8, height: u8, data: &[u8]) -> Vec<u8> {
    let mut bmp = vec![0; 62];
    bmp[..2].copy_from_slice(b"BM");
    for (offset, v) in [
        (10, 62u32),
        (14, 40),
        (18, u32::from(width)),
        (22, u32::from(height)),
        (30, 1),
        (46, 2),
    ] {
        bmp[offset..offset + 4].copy_from_slice(&v.to_le_bytes());
    }
    bmp[26] = 1;
    bmp[28] = 8;
    bmp[58..61].copy_from_slice(&[30, 20, 10]);
    bmp.extend_from_slice(data);
    bmp
}

#[test]
fn over_long_rle8_runs_advance_past_the_row_like_the_reference() {
    // A run longer than the row paints to its end, and an end-of-line resumes.
    let raster = decode_other(&rle8_bmp(2, 2, &[12, 1, 0, 0, 0, 1]), 2, 2, 16).unwrap();
    assert_eq!(raster.rgba[8..], [10, 20, 30, 255, 10, 20, 30, 255]);
    assert_eq!(raster.rgba[..8], [0, 0, 0, 255, 0, 0, 0, 255]);
    // A same-row delta of zero needs no pixels, so it is still accepted.
    assert!(decode_other(&rle8_bmp(2, 2, &[12, 1, 0, 2, 0, 0, 0, 1]), 2, 2, 16).is_ok());
    // Moving down must black out the first `x` (here 12) target pixels: corrupt.
    assert!(decode_other(&rle8_bmp(2, 3, &[12, 1, 0, 2, 0, 2, 0, 1]), 2, 3, 24).is_err());
    // Absolute spans that cross the scanline are still rejected.
    assert!(decode_other(&rle8_bmp(2, 2, &[0, 3, 1, 1, 1, 0, 0, 1]), 2, 2, 16).is_err());
}
