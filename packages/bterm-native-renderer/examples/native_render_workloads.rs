//! Isolated, finite native rendering workload; never starts/connects to BMUX.
//! Run explicitly with `--window CASE`. Timings are CPU wall times, not input latency.
use bmux_tui::{
    damage::Damage,
    geometry::Rect,
    image::{
        ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
        ImagePlacement,
    },
    paint::{LocalRect, PaintCx},
    presentation::{PresentationLimits, PresentationState},
    text::Line,
};
use bterm_native_platform::{
    PhysicalSize, PlatformError, PlatformEvent, WindowApplication, WindowContext, WindowMetrics,
    WindowOptions,
};
use bterm_native_renderer::{
    FrameTelemetry, NativeRenderer, RenderError, RendererOptions, text::RendererTypography,
};
use std::{
    error::Error,
    time::{Duration, Instant},
};

const FRAMES: usize = 60;
const WARMUP: usize = 5;
const MAX_STARTUP_DEFERRALS: usize = 40;
const STARTUP_YIELD: Duration = Duration::from_millis(50);
const LIMITS: PresentationLimits = PresentationLimits {
    max_cells: 80 * 24 + 2 * 24 * 12,
    max_entries: 128,
    max_bytes: 2 * 1024 * 1024,
};
const CASES: [&str; 11] = [
    "local-grids",
    "cold-text",
    "warm-text",
    "changing-text",
    "scroll",
    "resize",
    "scale",
    "fractional-zoom",
    "static-image",
    "static-png",
    "changing-image",
];

static PNG_PAYLOAD: std::sync::OnceLock<ImagePayload> = std::sync::OnceLock::new();

fn prepare_png() -> Result<(), image::ImageError> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        &[32, 128, 224, 255].repeat(128 * 128),
        128,
        128,
        image::ExtendedColorType::Rgba8,
    )?;
    let _ = PNG_PAYLOAD.set(ImagePayload::Png {
        bytes,
        width: 128,
        height: 128,
    });
    Ok(())
}

fn valid_case(case: &str) -> bool {
    CASES.contains(&case) || case == "idle"
}

fn install_local_grids(
    prepared: &mut bmux_tui::presentation::PreparedPresentation,
) -> Result<(), Box<dyn Error>> {
    use bmux_tui::{
        frame::Cursor,
        geometry::Point,
        presentation::{GridTransform, PreparedGridFragment},
    };
    use std::sync::Arc;
    let area = prepared.buffer().area();
    let mut fragments = Vec::new();
    for (id, x, scale) in [("small", 1, 875), ("large", 30, 1375)] {
        let local = PresentationState::new(Rect::new(0, 0, 24, 12))
            .prepare(Damage::Full, |cx| {
                cx.write_line(
                    LocalRect::new(0, 0, 24, 1),
                    &Line::raw("fractional grid 界"),
                );
            })
            .ok_or("missing local frame")?;
        // Two strips leave root row 4 visible as an overlay across both grids.
        let clips = [Rect::new(x, 1, 28, 3), Rect::new(x, 5, 28, 15)]
            .into_iter()
            .map(|clip| clip.intersection(area))
            .filter(|clip| !clip.is_empty())
            .collect();
        let destination = Rect::new(2, 2, 10, 7);
        let image = Arc::new(ImagePlacement {
            key: ImageKey::new(id),
            payload: ImagePayload::Pixels {
                bytes: [32, 180, 240, 255].repeat(16 * 16),
                width: 16,
                height: 16,
                format: ImagePixelFormat::Rgba8,
            },
            destination,
            allocation: None,
            clip: destination,
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Frame,
        });
        fragments.push(
            PreparedGridFragment::new(
                id,
                Arc::new(local.buffer().clone()),
                GridTransform::new(Point::new(x, 1), scale).ok_or("transform")?,
                clips,
            )
            .ok_or("fragment")?
            .with_cursor(Cursor::visible(Point::new(1, 1)))
            .ok_or("cursor")?
            .with_images(vec![image])
            .ok_or("image")?,
        );
    }
    prepared.set_grid_fragments(fragments, LIMITS)?;
    Ok(())
}

fn paint(cx: &mut PaintCx<'_, '_>, case: &str, frame: usize) {
    let area = cx.area();
    for row in 0..area.height {
        let source = if case == "scroll" {
            usize::from(row) + frame
        } else {
            usize::from(row)
        };
        let revision = if case == "changing-text" { frame } else { 0 };
        cx.write_line(
            LocalRect::new(0, i64::from(row), area.width, 1),
            &Line::from(format!(
                "{source:05} rev={revision:04} fn render() {{ retained text 0123456789 }} café e\u{301} 界"
            )),
        );
    }
    if matches!(case, "static-image" | "static-png" | "changing-image") {
        // Stable identity; changing content replaces it instead of accumulating images.
        let phase = if case == "changing-image" {
            frame % 2
        } else {
            0
        };
        let pixel = if phase == 0 {
            [32, 128, 224, 255]
        } else {
            [224, 64, 32, 255]
        };
        let destination = Rect::new(2, 2, 20, 10);
        cx.push_image(ImageContribution::Present(ImagePlacement {
            key: ImageKey::new("native-workload-image"),
            payload: if case == "static-png" {
                PNG_PAYLOAD
                    .get()
                    .expect("PNG initialized before event loop")
                    .clone()
            } else {
                ImagePayload::Pixels {
                    bytes: pixel.repeat(128 * 128),
                    width: 128,
                    height: 128,
                    format: ImagePixelFormat::Rgba8,
                }
            },
            destination,
            allocation: None,
            clip: destination,
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Frame,
        }));
    }
}

#[derive(Default)]
struct Sample {
    setup_ns: u128,
    configure_ns: u128,
    prepare_ns: u128,
    output_commit_ns: u128,
    submitted: bool,
    telemetry: FrameTelemetry,
}

struct Workload {
    case: String,
    renderer: Option<NativeRenderer>,
    presentation: PresentationState,
    iteration: usize,
    samples: Vec<Sample>,
    started: Instant,
    finished: bool,
    startup_ready: bool,
    startup_wait_until: Option<Instant>,
    startup_deferrals: usize,
    startup_rejections: usize,
}

fn startup_occluded(error: &RenderError) -> bool {
    matches!(error, RenderError::Surface(reason) if reason == "surface acquisition: Occluded")
}

fn application_error(error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Application(error.to_string())
}

fn validate_metrics(metrics: WindowMetrics) -> Result<(), PlatformError> {
    if metrics.physical_size.is_empty()
        || metrics.physical_size.width > 4096
        || metrics.physical_size.height > 4096
        || !(0.5..=4.0).contains(&metrics.scale_factor)
    {
        return Err(application_error(
            "drawable must be 1..=4096 pixels per axis at scale 0.5..=4",
        ));
    }
    Ok(())
}

impl Workload {
    fn new(case: String) -> Self {
        Self {
            case,
            renderer: None,
            presentation: PresentationState::new(Rect::new(0, 0, 80, 24)),
            iteration: 0,
            samples: Vec::with_capacity(FRAMES),
            started: Instant::now(),
            finished: false,
            startup_ready: false,
            startup_wait_until: None,
            startup_deferrals: 0,
            startup_rejections: 0,
        }
    }

    fn defer_startup(&mut self, context: &WindowContext<'_>) -> Result<(), PlatformError> {
        if self.startup_deferrals >= MAX_STARTUP_DEFERRALS {
            return Err(application_error(
                "startup surface never became drawable; deferral budget exhausted",
            ));
        }
        self.startup_deferrals += 1;
        self.startup_ready = false;
        self.startup_wait_until = Some(Instant::now() + STARTUP_YIELD);
        let proxy = context.proxy();
        // One finite sleeper per deferral (at most 40), no frame queue or polling loop.
        // Return to the event loop so Host::dispatch can show the window first.
        std::thread::Builder::new()
            .name("native-workload-startup".into())
            .spawn(move || {
                std::thread::sleep(STARTUP_YIELD);
                let _ = proxy.wake();
            })
            .map_err(application_error)?;
        Ok(())
    }

    fn frame(&mut self, context: &mut WindowContext<'_>) -> Result<(), PlatformError> {
        if self.started.elapsed().as_secs() >= 120 {
            return Err(application_error("workload exceeded 120 seconds"));
        }
        let cold = self.case == "cold-text";
        let mut sample = Sample::default();
        let metrics = context.metrics();
        validate_metrics(metrics)?;
        if self.iteration == 0 {
            eprintln!(
                "drawable={}x{} scale={} font_size=16 cell_ceiling=80x24 warmup={} samples={}",
                metrics.physical_size.width,
                metrics.physical_size.height,
                metrics.scale_factor,
                if cold { 0 } else { WARMUP },
                if cold { 8 } else { FRAMES },
            );
        }
        if cold || self.renderer.is_none() {
            // Drop old window-local caches/resources before creating their replacement.
            self.renderer = None;
            let start = Instant::now();
            self.renderer = Some(
                pollster::block_on(NativeRenderer::new(
                    context.surface_target(),
                    RendererOptions {
                        size: metrics.physical_size,
                        scale_factor: metrics.scale_factor as f32,
                        ..RendererOptions::default()
                    },
                ))
                .map_err(application_error)?,
            );
            sample.setup_ns = start.elapsed().as_nanos();
            self.presentation.reset();
        }
        let renderer = self.renderer.as_mut().expect("renderer initialized");
        // Also enables every cold recreation; this is a no-op once enabled.
        renderer.set_telemetry_enabled(true);
        let start = Instant::now();
        let mut size = metrics.physical_size;
        if self.case == "resize" && self.iteration % 2 == 1 {
            // Synthetic surface reconfiguration, not an OS window resize request.
            size = PhysicalSize {
                width: size.width / 4 * 3,
                height: size.height / 4 * 3,
            };
        }
        if self.case == "resize" {
            renderer.resize(size);
        }
        if self.case == "scale" {
            let factor = if self.iteration.is_multiple_of(2) {
                1.0
            } else {
                1.5
            };
            renderer
                .set_scale_factor(metrics.scale_factor as f32 * factor)
                .map_err(application_error)?;
        }
        if self.case == "fractional-zoom" {
            // Finite ascending/descending sweep, with stable text and bounded
            // frame count. Measures animated raster churn rather than bitmap scaling.
            let phase = self.iteration as f32 / (FRAMES + WARMUP) as f32;
            let wave = (phase * std::f32::consts::PI).sin();
            renderer
                .set_typography(RendererTypography {
                    font_size: 12.25 + wave * 11.5,
                    line_height: 1.15,
                    letter_spacing: 0.25,
                    ..Default::default()
                })
                .map_err(application_error)?;
        }
        let [cw, ch] = renderer.cell_size();
        // Fixed cell ceiling, even on a large/HiDPI display.
        let width = (size.width as f32 / cw).clamp(1.0, 80.0) as u16;
        let height = (size.height as f32 / ch).clamp(1.0, 24.0) as u16;
        self.presentation.resize(Rect::new(0, 0, width, height));
        sample.configure_ns = start.elapsed().as_nanos();
        let damage = if self.case == "idle" && self.iteration > 0 {
            Damage::None
        } else {
            Damage::Full
        };
        let start = Instant::now();
        let mut prepared = self
            .presentation
            .prepare_bounded(LIMITS, damage, |cx| {
                paint(cx, &self.case, self.iteration);
            })
            .map_err(application_error)?;
        if self.case == "local-grids"
            && let Some(prepared) = &mut prepared
        {
            install_local_grids(prepared).map_err(application_error)?;
        }
        sample.prepare_ns = start.elapsed().as_nanos();
        let start = Instant::now();
        // Sole commit point: output error propagates, and no sample is recorded as success.
        let output = self
            .presentation
            .present_prepared(prepared, |prepared, _| renderer.present(prepared));
        let output_commit_ns = start.elapsed().as_nanos();
        let report = match output {
            Ok(report) => report,
            Err(error) if self.iteration == 0 && startup_occluded(&error) => {
                // present_prepared already rejected the transaction. Do not advance
                // iteration, warmup, samples, or interaction metadata on this path.
                self.startup_rejections += 1;
                eprintln!(
                    "startup_rejected={} reason={error}; no commit/sample; yielding event loop",
                    self.startup_rejections
                );
                return self.defer_startup(context);
            }
            Err(error) => return Err(application_error(error)),
        };
        if self.iteration == 0 {
            eprintln!(
                "startup_drawable=successful_surface_output deferrals={} rejected_attempts={}",
                self.startup_deferrals, self.startup_rejections
            );
        }
        sample.output_commit_ns = output_commit_ns;
        sample.submitted = report.is_some();
        if let Some(report) = report {
            sample.telemetry = report.telemetry.ok_or_else(|| {
                application_error("enabled renderer did not return per-frame telemetry")
            })?;
        }
        if cold || self.iteration >= WARMUP {
            self.samples.push(sample);
        }
        self.iteration += 1;
        let count = if cold { 8 } else { FRAMES };
        if self.samples.len() == count {
            // Buffer only this bounded set; stdout does not perturb individual frame timings.
            println!(concat!(
                "case,sample,setup_ns,configure_ns,prepare_ns,output_commit_ns,submitted,",
                "text_cpu_ns,image_cpu_ns,gpu_wait_ns,gpu_validation_ns,total_ns,",
                "atlas_upload_count,atlas_upload_bytes,cpu_vertex_allocations,",
                "cpu_vertex_allocation_bytes,vertex_count,vertex_bytes,",
                "gpu_vertex_allocations,gpu_vertex_bytes,image_cache_hits,image_cache_misses,",
                "image_decode_count,glyph_rgba_cache_hits,glyph_rgba_cache_misses"
            ));
            for (index, sample) in self.samples.iter().enumerate() {
                println!(
                    "{},{index},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    self.case,
                    sample.setup_ns,
                    sample.configure_ns,
                    sample.prepare_ns,
                    sample.output_commit_ns,
                    sample.submitted,
                    sample.telemetry.text_cpu_ns,
                    sample.telemetry.image_cpu_ns,
                    sample.telemetry.gpu_wait_ns,
                    sample.telemetry.gpu_validation_ns,
                    sample.telemetry.total_ns,
                    sample.telemetry.atlas_upload_count,
                    sample.telemetry.atlas_upload_bytes,
                    sample.telemetry.cpu_vertex_allocations,
                    sample.telemetry.cpu_vertex_allocation_bytes,
                    sample.telemetry.vertex_count,
                    sample.telemetry.vertex_bytes,
                    sample.telemetry.gpu_vertex_allocations,
                    sample.telemetry.gpu_vertex_bytes,
                    sample.telemetry.image_cache_hits,
                    sample.telemetry.image_cache_misses,
                    sample.telemetry.image_decode_count,
                    sample.telemetry.glyph_rgba_cache_hits,
                    sample.telemetry.glyph_rgba_cache_misses,
                );
            }
            self.finished = true;
            context.exit();
        } else {
            context.request_redraw();
        }
        Ok(())
    }
}

impl WindowApplication for Workload {
    fn event(
        &mut self,
        context: &mut WindowContext<'_>,
        event: PlatformEvent,
    ) -> Result<(), PlatformError> {
        match event {
            PlatformEvent::Resumed => self.defer_startup(context)?,
            PlatformEvent::Wake => {
                if self
                    .startup_wait_until
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    self.startup_wait_until = None;
                    self.startup_ready = true;
                    context.request_redraw();
                }
            }
            PlatformEvent::RedrawRequested if !self.finished && self.startup_ready => {
                self.frame(context)?
            }
            PlatformEvent::Resized(metrics) => {
                validate_metrics(metrics)?;
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(metrics.physical_size);
                    renderer
                        .set_scale_factor(metrics.scale_factor as f32)
                        .map_err(application_error)?;
                }
            }
            PlatformEvent::Suspended | PlatformEvent::CloseRequested => {
                return Err(application_error(
                    "workload interrupted; discard incomplete run",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).take(3).collect();
    let [flag, case] = args.as_slice() else {
        return Err(
            "usage: native_render_workloads --window CASE (explicit native window opt-in)".into(),
        );
    };
    if flag != "--window" || !valid_case(case) {
        return Err(format!("CASE must be one of {CASES:?} or idle; --window is required").into());
    }
    eprintln!(
        "Synthetic native workload: {case}; no BMUX runtime; CPU timings, not input/display latency"
    );
    prepare_png()?;
    bterm_native_platform::run(
        WindowOptions {
            title: format!("Isolated Bterm workload: {case}"),
            ..WindowOptions::default()
        },
        Workload::new(case.clone()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_local_grid_workload_has_images_cursors_and_overlay_gap() {
        let mut frame = PresentationState::new(Rect::new(0, 0, 80, 24))
            .prepare(Damage::Full, |cx| paint(cx, "local-grids", 0))
            .unwrap();
        install_local_grids(&mut frame).unwrap();
        assert_eq!(frame.grid_fragments().len(), 2);
        assert_eq!(frame.grid_fragments()[0].transform().scale(), 0.875);
        assert_eq!(frame.grid_fragments()[1].transform().scale(), 1.375);
        for fragment in frame.grid_fragments() {
            assert_eq!(fragment.images().len(), 1);
            assert!(fragment.cursor().unwrap().visible);
            assert!(
                fragment
                    .visible_clips()
                    .iter()
                    .all(|clip| !clip.contains(bmux_tui::geometry::Point::new(clip.x, 4)))
            );
        }
        frame.validate_limits(LIMITS).unwrap();
    }

    #[test]
    fn only_explicit_occlusion_is_a_startup_retry() {
        assert!(startup_occluded(&RenderError::Surface(
            "surface acquisition: Occluded".into()
        )));
        for error in [
            RenderError::Surface("surface outdated; reconfigured".into()),
            RenderError::Surface("surface acquisition: Timeout".into()),
            RenderError::Device("device lost".into()),
            RenderError::Suspended,
        ] {
            assert!(!startup_occluded(&error));
        }
    }

    #[test]
    fn workloads_are_bounded_and_content_changes_are_real() {
        prepare_png().unwrap();
        for case in CASES.into_iter().chain(["idle"]) {
            assert!(valid_case(case));
            let state = PresentationState::new(Rect::new(0, 0, 80, 24));
            let first = state
                .prepare_bounded(LIMITS, Damage::Full, |cx| paint(cx, case, 0))
                .unwrap()
                .unwrap();
            let next = state
                .prepare_bounded(LIMITS, Damage::Full, |cx| paint(cx, case, 1))
                .unwrap()
                .unwrap();
            match case {
                "changing-text" | "scroll" => assert_ne!(first.buffer(), next.buffer()),
                "changing-image" => {
                    assert_eq!(first.image_scene().placements().len(), 1);
                    assert_ne!(first.image_scene(), next.image_scene());
                }
                _ => assert_eq!(first, next),
            }
        }
        assert!(!valid_case("unknown"));
    }

    #[test]
    fn failed_output_does_not_commit_and_idle_does_not_submit() {
        let mut state = PresentationState::new(Rect::new(0, 0, 80, 24));
        state
            .present_bounded(
                LIMITS,
                Damage::Full,
                |cx| {
                    paint(cx, "static-image", 0);
                    cx.push_selection_scope(bmux_tui::selection::SelectionScope::new(
                        "committed",
                        Rect::new(0, 0, 80, 24),
                    ));
                },
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let committed = state.image_scene().clone();
        let committed_selection = state.selection().clone();
        let failed = state.present_bounded(
            LIMITS,
            Damage::Full,
            |cx| paint(cx, "changing-image", 1),
            |_, _| Err::<(), _>("injected failure"),
        );
        assert!(failed.is_err());
        assert_eq!(state.image_scene(), &committed);
        assert_eq!(state.selection(), &committed_selection);
        state
            .present_bounded(
                LIMITS,
                Damage::Full,
                |cx| paint(cx, "warm-text", 0),
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        let result = state.present_bounded(
            LIMITS,
            Damage::None,
            |_| panic!("idle must not paint"),
            |_, _| -> Result<(), ()> { panic!("idle must not output") },
        );
        assert_eq!(result.unwrap(), None);
    }
}
