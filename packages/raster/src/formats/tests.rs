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
