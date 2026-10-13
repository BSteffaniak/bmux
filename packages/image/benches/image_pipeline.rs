//! Opt-in, terminal-free image pipeline workload. See docs/image-performance.md.
#[path = "support/sixel_reference.rs"]
mod sixel_reference;
mod support;

use std::collections::BTreeMap;
use std::error::Error;
use std::hint::black_box;
use std::time::Instant;

use bmux_image::codec::{base64, iterm2, kitty, sixel};
use bmux_image::compositor::{KittyHostState, PaneRect, render_pane_images};
use bmux_image::config::ImageDecodeMode;
use bmux_image::model::{
    ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, KittyFormat,
    PaneImage, PixelBuffer,
};
use bmux_image::tui::TuiImageCompositor;
use bmux_image::{HostImageCapabilities, ImageConfig};
use bmux_tui::geometry::Rect;
use bmux_tui::image::{
    ImageContribution, ImageKey, ImageLifecycle, ImagePayload as TuiPayload, ImagePixelFormat,
    ImagePlacement,
};
use bmux_tui::image_scene::ImageScene;
use image::ImageEncoder;

type Samples = BTreeMap<String, Vec<Sample>>;
type WorkCounts = (usize, usize, usize, usize, usize);

#[derive(Clone, Copy)]
struct Sample {
    nanos: u128,
    bytes: usize,
    chunks: usize,
    uploads: usize,
    decodes: usize,
    preparations: usize,
}

fn record(
    samples: &mut Samples,
    name: &str,
    operation: impl FnOnce() -> Result<WorkCounts, Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let (bytes, chunks, uploads, decodes, preparations) = black_box(operation()?);
    samples.entry(name.to_owned()).or_default().push(Sample {
        nanos: start.elapsed().as_nanos(),
        bytes,
        chunks,
        uploads,
        decodes,
        preparations,
    });
    Ok(())
}

fn output_counts(bytes: &[u8]) -> WorkCounts {
    let chunks = support::count_marker(bytes, b"\x1b_G")
        + support::count_marker(bytes, b"\x1bP")
        + support::count_marker(bytes, b"\x1b]1337;");
    let uploads = support::count_marker(bytes, b"\x1b_Ga=t,");
    (black_box(bytes).len(), chunks, uploads, 0, 0)
}

fn encoder_cases(
    samples: &mut Samples,
    pixels: &PixelBuffer,
    png: &[u8],
) -> Result<(), Box<dyn Error>> {
    record(samples, "encoder/base64", || {
        let encoded = base64::base64_encode(black_box(&pixels.data));
        Ok((black_box(&encoded).len(), 1, 0, 0, 0))
    })?;
    record(samples, "encoder/kitty", || {
        let chunks = kitty::encode_transmit_chunks(
            1,
            KittyFormat::Rgba,
            black_box(&pixels.data),
            pixels.width,
            pixels.height,
        );
        Ok((
            black_box(&chunks).iter().map(Vec::len).sum(),
            chunks.len(),
            chunks.len(),
            0,
            0,
        ))
    })?;
    record(samples, "encoder/sixel-reference", || {
        let encoded =
            sixel_reference::encode(black_box(pixels)).ok_or("reference fixture rejected")?;
        Ok((black_box(&encoded).len(), 1, 0, 0, 0))
    })?;
    record(samples, "encoder/sixel", || {
        let encoded = sixel::encode(black_box(pixels)).ok_or("Sixel fixture rejected")?;
        Ok((black_box(&encoded).len(), 1, 0, 0, 0))
    })?;
    record(samples, "encoder/iterm2-png", || {
        let encoded = iterm2::encode_body(black_box(png), true);
        Ok((black_box(&encoded).len(), 1, 0, 0, 0))
    })?;
    Ok(())
}

fn placement(pixels: &PixelBuffer) -> ImagePlacement {
    ImagePlacement {
        fit: bmux_tui::image::ImageFit::Stretch,
        allocation: None,
        key: ImageKey::new("fixed-image"),
        payload: TuiPayload::Pixels {
            bytes: pixels.data.clone(),
            width: pixels.width,
            height: pixels.height,
            format: ImagePixelFormat::Rgba8,
        },
        destination: Rect::new(2, 2, 80, 24),
        clip: Rect::new(0, 0, 100, 40),
        lifecycle: ImageLifecycle::Persistent,
    }
}

fn tui_cases(
    samples: &mut Samples,
    pixels: &PixelBuffer,
    protocol: &str,
    caps: &HostImageCapabilities,
) -> Result<(), Box<dyn Error>> {
    let mut compositor = TuiImageCompositor::new();
    let mut scene = ImageScene::default();
    let mut image = placement(pixels);
    let mut terminal = PaneRect {
        x: 0,
        y: 0,
        w: 100,
        h: 40,
    };
    let config = ImageConfig::default();
    for phase in [
        "cold",
        "warm",
        "movement",
        "crop",
        "hide",
        "reveal",
        "resize",
        "replacement",
        "delete",
    ] {
        match phase {
            "movement" => image.destination.x += 1,
            "crop" => image.clip = Rect::new(10, 5, 40, 12),
            "hide" => image.clip = Rect::new(0, 0, 0, 0),
            "reveal" => image.clip = Rect::new(0, 0, 100, 40),
            "resize" => {
                terminal.w = 70;
                image.destination.width = 60;
            }
            "replacement" => {
                if let TuiPayload::Pixels { bytes, .. } = &mut image.payload {
                    bytes[0] ^= 255;
                }
            }
            _ => {}
        }
        // Scene production is deliberately outside compositor timing.
        let contributions = if phase == "delete" {
            vec![ImageContribution::Remove(image.key.clone())]
        } else {
            vec![ImageContribution::Present(image.clone())]
        };
        let delta = scene.reconcile(&contributions);
        compositor.apply_delta(&delta);
        record(samples, &format!("tui/{protocol}/{phase}"), || {
            let mut out = Vec::new();
            compositor.render(&mut out, black_box(&scene), terminal, caps, &config)?;
            Ok(output_counts(&out))
        })?;
    }
    Ok(())
}

fn clipped_comparison(
    samples: &mut Samples,
    image: &PaneImage,
    pane: PaneRect,
    caps: &HostImageCapabilities,
) -> Result<(), Box<dyn Error>> {
    let mut committed = KittyHostState::default();
    let mut warm_work = None;
    for phase in [
        "clipped-cold",
        "retained-warm",
        "invalidated-upload-baseline",
    ] {
        // Both warm candidates start from the exact same committed cold frame.
        // Invalidation is outside timing; its repair output remains inside timing.
        let mut base = committed.clone();
        if phase == "invalidated-upload-baseline" {
            base.invalidate_after_failed_output(&committed);
        }
        record(samples, &format!("attach/kitty/{phase}"), || {
            let before = base.work_counts();
            let mut candidate = base.clone();
            candidate.begin_frame();
            let mut out = Vec::new();
            bmux_image::compositor::render_pane_images_clipped(
                &mut out,
                std::slice::from_ref(black_box(image)),
                pane,
                &[],
                caps,
                &mut candidate,
            )?;
            candidate.finish_frame(&mut out)?;
            let after = candidate.work_counts();
            let mut counts = output_counts(&out);
            counts.3 = after.0.saturating_sub(before.0);
            counts.4 = after.1.saturating_sub(before.1);
            if phase == "retained-warm" {
                warm_work = Some((counts.3, counts.4));
            } else if phase == "invalidated-upload-baseline" {
                assert_eq!(
                    Some((counts.3, counts.4)),
                    warm_work,
                    "comparison cache work must match"
                );
            }
            if phase == "clipped-cold" {
                committed = candidate;
            }
            Ok(counts)
        })?;
    }
    Ok(())
}

fn attach_cases(samples: &mut Samples, pixels: &PixelBuffer) -> Result<(), Box<dyn Error>> {
    let mut state = KittyHostState::default();
    let caps = HostImageCapabilities {
        kitty_graphics: true,
        ..Default::default()
    };
    let terminal = PaneRect {
        x: 0,
        y: 0,
        w: 100,
        h: 40,
    };
    let mut image = PaneImage {
        id: 1,
        protocol: ImageProtocol::KittyGraphics,
        payload: ImagePayload {
            raw: None,
            pixels: Some(pixels.clone()),
        },
        position: ImagePosition { row: 2, col: 2 },
        cell_size: ImageCellSize { rows: 24, cols: 80 },
        pixel_size: ImagePixelSize {
            width: pixels.width,
            height: pixels.height,
        },
    };
    let original = image.clone();
    clipped_comparison(samples, &original, terminal, &caps)?;
    for phase in ["cold", "warm", "movement", "fresh-id-baseline"] {
        if phase == "movement" {
            image.position.col += 1;
        }
        if phase == "fresh-id-baseline" {
            image.id += 1;
        }
        record(samples, &format!("attach/kitty/{phase}"), || {
            let mut out = Vec::new();
            render_pane_images(
                &mut out,
                std::slice::from_ref(black_box(&image)),
                terminal,
                &caps,
                ImageDecodeMode::Server,
                &mut state,
            )?;
            Ok(output_counts(&out))
        })?;
    }
    for phase in [
        "crop",
        "hide",
        "reveal",
        "resize",
        "replacement",
        "overlay-occlusion",
        "delete",
    ] {
        let mut pane = terminal;
        let mut covers = Vec::new();
        match phase {
            "crop" => pane.w = 40,
            "hide" => image.position.col = 100,
            "reveal" => image.position.col = 3,
            "resize" => {
                pane.w = 60;
                image.cell_size.cols = 50;
            }
            "replacement" => {
                image.id += 1;
                image.payload.pixels = Some(support::pixels(17, 13));
            }
            "overlay-occlusion" => covers.push(Rect::new(10, 5, 10, 10)),
            _ => {}
        }
        record(samples, &format!("attach/kitty/{phase}"), || {
            let before = state.work_counts();
            let mut candidate = state.clone();
            candidate.begin_frame();
            let mut out = Vec::new();
            if phase != "delete" {
                bmux_image::compositor::render_pane_images_clipped(
                    &mut out,
                    std::slice::from_ref(black_box(&image)),
                    pane,
                    &covers,
                    &caps,
                    &mut candidate,
                )?;
            }
            candidate.finish_frame(&mut out)?;
            let after = candidate.work_counts();
            state = candidate;
            let mut counts = output_counts(&out);
            counts.3 = after.0.saturating_sub(before.0);
            counts.4 = after.1.saturating_sub(before.1);
            Ok(counts)
        })?;
    }
    Ok(())
}

fn report(samples: &Samples, json: bool) {
    for (name, values) in samples {
        let mut timings: Vec<_> = values.iter().map(|sample| sample.nanos).collect();
        timings.sort_unstable();
        let first = values[0];
        let stable = values.iter().all(|sample| {
            (
                sample.bytes,
                sample.chunks,
                sample.uploads,
                sample.decodes,
                sample.preparations,
            ) == (
                first.bytes,
                first.chunks,
                first.uploads,
                first.decodes,
                first.preparations,
            )
        });
        let median = timings[timings.len() / 2];
        let p95 = timings[(timings.len() - 1) * 95 / 100];
        assert!(stable, "non-deterministic work counts for {name}");
        if json {
            println!(
                "{{\"case\":\"{name}\",\"decodes\":{},\"preparations\":{},\"samples\":{},\"min_ns\":{},\"median_ns\":{median},\"p95_ns\":{p95},\"max_ns\":{},\"bytes\":{},\"chunks\":{},\"upload_chunks\":{},\"counts_stable\":{stable}}}",
                first.decodes,
                first.preparations,
                values.len(),
                timings[0],
                timings[timings.len() - 1],
                first.bytes,
                first.chunks,
                first.uploads
            );
        } else {
            println!(
                "{name}: decodes={} preparations={} n={} min/median/p95/max_ns={}/{median}/{p95}/{} bytes={} chunks={} upload_chunks={} counts_stable={stable}",
                first.decodes,
                first.preparations,
                values.len(),
                timings[0],
                timings[timings.len() - 1],
                first.bytes,
                first.chunks,
                first.uploads
            );
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut iterations = 10_usize;
    let mut width = 1280_u32;
    let mut height = 720_u32;
    let mut fixture = "palette".to_owned();
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => fixture = args.next().ok_or("missing fixture")?,
            "--json" => json = true,
            "--iterations" => iterations = args.next().ok_or("missing iterations")?.parse()?,
            "--width" => width = args.next().ok_or("missing width")?.parse()?,
            "--height" => height = args.next().ok_or("missing height")?.parse()?,
            "--bench" => {} // Cargo may append this for harness=false.
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if iterations == 0
        || iterations > 1000
        || width == 0
        || height == 0
        || width > 4096
        || height > 4096
    {
        return Err("require 1..=1000 iterations and 1..=4096 dimensions".into());
    }
    let pixels = support::fixture(width, height, &fixture)?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        &pixels.data,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    let reference = sixel_reference::encode(&pixels).ok_or("reference fixture rejected")?;
    let optimized = sixel::encode(&pixels).ok_or("optimized fixture rejected")?;
    assert_eq!(
        optimized, reference,
        "selected fixture must preserve Sixel wire bytes"
    );
    // Correctness check is outside timing; do not retain its buffers during measurement.
    drop((reference, optimized));
    let mut samples = Samples::new();
    for _ in 0..iterations {
        encoder_cases(&mut samples, &pixels, &png)?;
        attach_cases(&mut samples, &pixels)?;
        for (protocol, caps) in [
            (
                "kitty",
                HostImageCapabilities {
                    kitty_graphics: true,
                    ..Default::default()
                },
            ),
            (
                "sixel",
                HostImageCapabilities {
                    sixel: true,
                    ..Default::default()
                },
            ),
            (
                "iterm2",
                HostImageCapabilities {
                    iterm2_inline: true,
                    ..Default::default()
                },
            ),
        ] {
            tui_cases(&mut samples, &pixels, protocol, &caps)?;
        }
    }
    let samples = samples
        .into_iter()
        .map(|(name, values)| (format!("{fixture}/{width}x{height}/{name}"), values))
        .collect();
    report(&samples, json);
    Ok(())
}
