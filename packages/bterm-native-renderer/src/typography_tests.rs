//! CPU-only quality/regression tests; no GPU or host-font assumptions except explicit resolution.
use super::*;

#[test]
fn physical_raster_limit_remains_explicit_for_large_zoom_and_dpi() {
    let raster = TextRasterizer::bundled().unwrap();
    let dpi = 2.0;
    let requested = 128.0 * 3.0;
    let upper = MAX_LOGICAL_FONT_SIZE.min(MAX_RASTER_PIXEL_SIZE / dpi);
    let effective = f32::min(requested, upper);
    assert_eq!(effective, 128.0);
    assert!(raster.metrics(effective * dpi).is_ok());
    assert_eq!(
        raster.metrics(requested * dpi).unwrap_err(),
        TextError::InvalidPixelSize
    );
    assert!(raster.metrics(MAX_RASTER_PIXEL_SIZE + 0.01).is_err());
    assert!(raster.metrics(MIN_RASTER_PIXEL_SIZE - 0.01).is_err());
}

#[test]
fn fractional_sizes_quantize_cache_identity_without_bitmap_scaling() {
    let mut raster = TextRasterizer::bundled().unwrap();
    let a = raster
        .rasterize_cell("M", 1, TextStyle::default(), 16.251)
        .unwrap();
    let b = raster
        .rasterize_cell("M", 1, TextStyle::default(), 16.252)
        .unwrap();
    let c = raster
        .rasterize_cell("M", 1, TextStyle::default(), 16.75)
        .unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert!(!Arc::ptr_eq(&a, &c));
    assert_ne!(a.glyphs[0].pixels, c.glyphs[0].pixels);
    assert!(
        a.glyphs[0]
            .pixels
            .iter()
            .any(|coverage| (1..255).contains(coverage))
    );
}

#[test]
fn fractional_baseline_bakes_coverage_into_integer_bitmap_quads() {
    let mut raster = TextRasterizer::bundled().unwrap();
    let mut masks = Vec::new();
    for step in 0..8 {
        let origin = [10.0 + step as f32 / 8.0, 23.375];
        let (baseline, phase) = raster_origin(origin);
        let cell = raster
            .rasterize_cell_phased("e\u{301}", 1, TextStyle::default(), 17.25, phase)
            .unwrap();
        for glyph in &cell.glyphs {
            assert!((baseline[0] + glyph.x).fract().abs() < 0.0001);
            assert!((baseline[1] + glyph.y).fract().abs() < 0.0001);
            assert!("e\u{301}".is_char_boundary(glyph.source.start));
            assert!("e\u{301}".is_char_boundary(glyph.source.end));
        }
        masks.push(cell.glyphs[0].pixels.clone());
    }
    assert!(masks.windows(2).any(|pair| pair[0] != pair[1]));
}

#[test]
fn fractional_vertical_coverage_moves_down_with_the_baseline() {
    let mut raster = TextRasterizer::bundled().unwrap();
    let mut centers = Vec::new();
    for phase in [0, 4] {
        let baseline = 23.0 + f32::from(phase) / 8.0;
        let cell = raster
            .rasterize_cell_phased("M", 1, TextStyle::default(), 17.25, [0, phase])
            .unwrap();
        let glyph = &cell.glyphs[0];
        assert_eq!(glyph.format, BitmapFormat::Mask);
        let mut mass = 0.0;
        let mut moment = 0.0;
        for (index, coverage) in glyph.pixels.iter().enumerate() {
            let weight = f64::from(*coverage);
            let y = f64::from(baseline + glyph.y - glyph.top as f32)
                + (index / glyph.width as usize) as f64
                + 0.5;
            mass += weight;
            moment += y * weight;
        }
        assert!(mass > 0.0);
        centers.push(moment / mass);
    }
    assert!((centers[1] - centers[0] - 0.5).abs() < 0.1);
}

#[test]
fn animated_sizes_and_phases_remain_bounded() {
    let mut raster = TextRasterizer::bundled().unwrap();
    for step in 0..2200 {
        let size = 10.0 + step as f32 / 64.0;
        let phase = [(step % 8) as u8, ((step / 8) % 8) as u8];
        raster
            .rasterize_cell_phased("M", 1, TextStyle::default(), size, phase)
            .unwrap();
        let (entries, bytes) = raster.cache_usage();
        assert!(entries <= 2048);
        assert!(bytes <= 8 * 1024 * 1024);
    }
}

#[test]
fn unavailable_family_uses_bundled_metrics_and_style_faces() {
    let typography = RendererTypography {
        family: Some("BMUX nonexistent font 0123456789".into()),
        fallback: vec!["BMUX nonexistent fallback 0123456789".into()],
        ..Default::default()
    };
    let configured = TextRasterizer::with_typography(&typography).unwrap();
    let bundled = TextRasterizer::bundled().unwrap();
    assert_eq!(
        configured.metrics(16.25).unwrap().advance,
        bundled.metrics(16.25).unwrap().advance
    );
    for style in [
        TextStyle::default(),
        TextStyle {
            bold: true,
            italic: false,
        },
        TextStyle {
            bold: false,
            italic: true,
        },
        TextStyle {
            bold: true,
            italic: true,
        },
    ] {
        let index = configured.fonts.select("ABC", style);
        assert_ne!(configured.fonts.get(index).font().charmap().map('A'), 0);
        let expected = bundled.fonts.select("ABC", style);
        assert_eq!(
            configured.fonts.get(index).font().data,
            bundled.fonts.get(expected).font().data
        );
    }
}
