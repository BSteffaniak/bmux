//! A non-presentation application: embed a fixed sensor raster in a dashboard.
//! No terminal runtime is needed; the same prepared output could go to any backend.
use std::sync::Arc;

use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    damage::Damage,
    embedded_surface::{EmbeddedSurface, RetainedSurface, SurfaceIdentity, SurfaceLimits},
    geometry::{Rect, Size},
    paint::LocalRect,
    presentation::{PresentationLimits, PresentationState},
    style::Style,
    text::Line,
};

fn main() -> Result<(), String> {
    // The producer uses the ordinary painter and prepared presentation, not a
    // special demo renderer. Capture is an inert projection of that output.
    let producer = PresentationState::new(Rect::new(0, 0, 12, 2));
    let prepared = producer
        .prepare_bounded(
            PresentationLimits {
                max_cells: 24,
                max_entries: 16,
                max_bytes: 4096,
            },
            Damage::Full,
            |cx| {
                cx.write_line(LocalRect::new(0, 0, 12, 1), &Line::raw("Temperature"));
                cx.write_line(LocalRect::new(0, 1, 12, 1), &Line::raw("23 °C  正常"));
            },
        )
        .map_err(|error| format!("producer admission: {error:?}"))?
        .ok_or_else(|| "producer emitted no output".to_owned())?;
    let surface = RetainedSurface::new(
        SurfaceIdentity {
            owner: "sensor/front-room".into(),
            generation: 1,
            revision: 7,
        },
        Arc::new(prepared.buffer().clone()),
        Arc::new(prepared.image_scene().clone()),
        SurfaceLimits {
            max_cells: 24,
            max_bytes: 4096,
        },
    )
    .map_err(|error| format!("sensor admission: {error:?}"))?;
    let component = EmbeddedSurface::new(&surface);
    let layout = component.layout(
        Constraints::tight(Size::new(18, 4)),
        &mut LayoutCx::default(),
    );
    let mut dashboard = PresentationState::new(Rect::new(0, 0, 18, 4));
    dashboard
        .present_bounded(
            PresentationLimits {
                max_cells: 72,
                max_entries: 16,
                max_bytes: 8192,
            },
            Damage::Full,
            |cx| {
                cx.fill(LocalRect::new(0, 0, 18, 4), " ", Style::new());
                component.paint(&layout, cx);
            },
            |output, _| {
                for y in 0..output.buffer().area().height {
                    println!("{}", output.buffer().row_symbols(y).unwrap_or_default());
                }
                Ok::<(), String>(())
            },
        )
        .map_err(|error| format!("dashboard output: {error:?}"))?;
    // Publish input geometry only after output succeeded; use this SAME layout.
    if let Some(point) = component.input_point(&layout, 3, 1) {
        println!(
            "sensor {}@{}: ({}, {})",
            point.identity.owner, point.identity.revision, point.x, point.y
        );
    }
    Ok(())
}
