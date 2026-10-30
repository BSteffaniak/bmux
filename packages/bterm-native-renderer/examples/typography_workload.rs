//! Finite, headless CPU typography workload. No runtime, window, or session state.
//! Wall-clock percentiles are observations, not GPU/display/input-latency claims.
use bterm_native_renderer::text::{TextRasterizer, TextStyle};
use std::{error::Error, time::Instant};

const FRAMES: usize = 2304;

fn run(case: &str) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let mut raster = TextRasterizer::bundled()?;
    let setup_ns = start.elapsed().as_nanos();
    let mut samples = Vec::with_capacity(FRAMES);
    let mut peak_entries = 0;
    let mut peak_bytes = 0;
    for frame in 0..FRAMES {
        let size = if case == "animated" {
            12.25 + frame as f32 / 64.0
        } else {
            16.25
        };
        let start = Instant::now();
        for (text, columns, style) in [
            ("M", 1, TextStyle::default()),
            ("e\u{301}", 1, TextStyle::default()),
            ("界", 2, TextStyle::default()),
            (
                "B",
                1,
                TextStyle {
                    bold: true,
                    italic: false,
                },
            ),
            (
                "i",
                1,
                TextStyle {
                    bold: false,
                    italic: true,
                },
            ),
        ] {
            let cell = raster.rasterize_cell(text, columns, style, size)?;
            assert_eq!(cell.columns, columns);
            assert_eq!(cell.source_bytes, text.len());
        }
        samples.push(start.elapsed().as_nanos());
        let (entries, bytes) = raster.cache_usage();
        assert!(entries <= 2048 && bytes <= 8 * 1024 * 1024);
        peak_entries = peak_entries.max(entries);
        peak_bytes = peak_bytes.max(bytes);
    }
    let first_ns = samples[0];
    samples.sort_unstable();
    println!(
        "{case},{FRAMES},{setup_ns},{first_ns},{},{},{peak_entries},{peak_bytes}",
        samples[FRAMES / 2],
        samples[FRAMES * 95 / 100]
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("case,frames,setup_ns,first_ns,p50_ns,p95_ns,peak_cache_entries,peak_cache_bytes");
    run("warm")?;
    run("animated")
}
