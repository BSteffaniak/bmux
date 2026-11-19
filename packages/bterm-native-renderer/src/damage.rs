//! Reconstructible native damage relative to the last successfully displayed snapshot.
use bmux_tui::{buffer::Buffer, presentation::PreparedPresentation};

const MAX_REGIONS: usize = 4096;

#[derive(Debug, Default)]
pub(crate) struct DamageTracker {
    generation: u64,
    previous: Option<std::sync::Arc<PreparedPresentation>>,
}
impl DamageTracker {
    pub fn invalidate(&mut self) {
        self.previous = None;
    }
    pub fn committed(&mut self, prepared: std::sync::Arc<PreparedPresentation>) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.previous = Some(prepared);
        self.generation
    }

    /// Complete snapshots may skip producer revisions. Never apply their deltas to an
    /// unrelated baseline: compare with actual successful output, including fragments.
    pub fn regions(
        &self,
        next: &PreparedPresentation,
        cell: [f32; 2],
        screen: [f32; 4],
    ) -> Vec<[f32; 4]> {
        let Some(old) = &self.previous else {
            return vec![screen];
        };
        if old.buffer().area() != next.buffer().area()
            || old.grid_fragments().len() != next.grid_fragments().len()
        {
            return vec![screen];
        }
        let mut regions = Vec::new();
        buffer_damage(
            old.buffer(),
            next.buffer(),
            [0.0; 2],
            cell,
            screen,
            &mut regions,
        );
        for (old, next) in old.grid_fragments().iter().zip(next.grid_fragments()) {
            if old.id() != next.id()
                || old.transform() != next.transform()
                || old.visible_clips() != next.visible_clips()
                || old.buffer().area() != next.buffer().area()
                || old.overlays().len() != next.overlays().len()
            {
                return vec![screen];
            }
            let origin = next.transform().origin();
            let origin = [f32::from(origin.x) * cell[0], f32::from(origin.y) * cell[1]];
            let scaled = cell.map(|v| v * next.transform().scale() as f32);
            for clip in next.visible_clips() {
                let clip = pixels(*clip, cell);
                buffer_damage(
                    old.buffer(),
                    next.buffer(),
                    origin,
                    scaled,
                    clip,
                    &mut regions,
                );
                for (a, b) in old.overlays().iter().zip(next.overlays()) {
                    if a.clip() != b.clip()
                        || a.paint_background() != b.paint_background()
                        || a.buffer().area() != b.buffer().area()
                    {
                        return vec![screen];
                    }
                    if let Some(clip) = intersection(clip, pixels(b.clip(), cell)) {
                        buffer_damage(a.buffer(), b.buffer(), [0.0; 2], cell, clip, &mut regions);
                    }
                }
                if old.images() != next.images() {
                    for image in old.images().iter().chain(next.images()) {
                        let a = old.images().iter().find(|a| a.key == image.key);
                        let b = next.images().iter().find(|b| b.key == image.key);
                        if a != b {
                            let rect = image.clip.intersection(image.destination);
                            let mut rect = pixels(rect, scaled);
                            rect[0] += origin[0];
                            rect[1] += origin[1];
                            if let Some(rect) = intersection(rect, clip) {
                                regions.push(rect);
                            }
                        }
                    }
                }
                if old.cursor() != next.cursor() {
                    for cursor in [old.cursor(), next.cursor()]
                        .into_iter()
                        .flatten()
                        .filter(|c| c.visible)
                    {
                        let rect = [
                            origin[0] + f32::from(cursor.position.x) * scaled[0],
                            origin[1] + f32::from(cursor.position.y) * scaled[1],
                            scaled[0],
                            scaled[1],
                        ];
                        if let Some(rect) = intersection(rect, clip) {
                            regions.push(rect);
                        }
                    }
                }
            }
        }
        if old.image_scene() != next.image_scene() {
            for image in old
                .image_scene()
                .placements()
                .chain(next.image_scene().placements())
            {
                if old.image_scene().get(&image.key) != next.image_scene().get(&image.key) {
                    regions.push(pixels(image.clip.intersection(image.destination), cell));
                }
            }
        }
        if old.cursor() != next.cursor() {
            for cursor in [old.cursor(), next.cursor()]
                .into_iter()
                .flatten()
                .filter(|c| c.visible)
            {
                regions.push([
                    f32::from(cursor.position.x) * cell[0],
                    f32::from(cursor.position.y) * cell[1],
                    cell[0],
                    cell[1],
                ]);
            }
        }
        if regions.len() > MAX_REGIONS {
            return vec![screen];
        }
        disjoint_regions(&regions, screen)
    }
}

fn pixels(rect: bmux_tui::geometry::Rect, cell: [f32; 2]) -> [f32; 4] {
    [
        f32::from(rect.x) * cell[0],
        f32::from(rect.y) * cell[1],
        f32::from(rect.width) * cell[0],
        f32::from(rect.height) * cell[1],
    ]
}

fn buffer_damage(
    old: &Buffer,
    next: &Buffer,
    origin: [f32; 2],
    cell: [f32; 2],
    clip: [f32; 4],
    regions: &mut Vec<[f32; 4]>,
) {
    if std::ptr::eq(old, next) {
        return;
    }
    let area = next.area();
    for row in 0..usize::from(area.height) {
        let width = usize::from(area.width);
        let mut x = 0;
        while x < width {
            let index = row * width + x;
            if old.cells()[index] == next.cells()[index] {
                x += 1;
                continue;
            }
            // Include both old/new wide leaders before clipping the replacement.
            let start = x.saturating_sub(usize::from(
                old.cells()[index].is_wide_continuation()
                    || next.cells()[index].is_wide_continuation(),
            ));
            x += 1;
            while x < width && old.cells()[row * width + x] != next.cells()[row * width + x] {
                x += 1;
            }
            let end = (x + 1).min(width);
            let rect = [
                origin[0] + (f32::from(area.x) + start as f32) * cell[0],
                origin[1] + (f32::from(area.y) + row as f32) * cell[1],
                (end - start) as f32 * cell[0],
                cell[1],
            ];
            if let Some(rect) = intersection(rect, clip) {
                regions.push(rect);
            }
            if regions.len() > MAX_REGIONS {
                return;
            }
        }
    }
}

fn intersection(a: [f32; 4], b: [f32; 4]) -> Option<[f32; 4]> {
    let x = a[0].max(b[0]);
    let y = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let bottom = (a[1] + a[3]).min(b[1] + b[3]);
    (right > x && bottom > y).then_some([x, y, right - x, bottom - y])
}

/// Pixel-cover first, then disjoint scan bands: never blend the same translucent
/// pixel twice, and never merge a sparse border into its interior bounding box.
fn disjoint_regions(regions: &[[f32; 4]], screen: [f32; 4]) -> Vec<[f32; 4]> {
    let regions = regions
        .iter()
        .filter_map(|r| {
            intersection(
                [
                    r[0].floor(),
                    r[1].floor(),
                    (r[0] + r[2]).ceil() - r[0].floor(),
                    (r[1] + r[3]).ceil() - r[1].floor(),
                ],
                screen,
            )
        })
        .collect::<Vec<_>>();
    let mut edges = regions
        .iter()
        .flat_map(|r| [r[1], r[1] + r[3]])
        .collect::<Vec<_>>();
    edges.sort_by(f32::total_cmp);
    edges.dedup();
    let mut result: Vec<[f32; 4]> = Vec::new();
    let mut work = 0_usize;
    for band in edges.windows(2) {
        work = work
            .saturating_add(regions.len())
            .saturating_add(result.len());
        if work > 1_000_000 {
            return vec![screen];
        }
        let mut spans = regions
            .iter()
            .filter(|r| r[1] <= band[0] && r[1] + r[3] >= band[1])
            .map(|r| (r[0], r[0] + r[2]))
            .collect::<Vec<_>>();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for (left, right) in spans {
            if let Some(last) = merged.last_mut()
                && left <= last.1
            {
                last.1 = last.1.max(right);
            } else {
                merged.push((left, right));
            }
        }
        for (left, right) in merged {
            if let Some(last) = result
                .iter_mut()
                .rev()
                .find(|r| r[0] == left && r[2] == right - left && r[1] + r[3] == band[0])
            {
                last[3] += band[1] - band[0];
            } else {
                result.push([left, band[0], right - left, band[1] - band[0]]);
            }
        }
        if result.len() > MAX_REGIONS {
            return vec![screen];
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn border_preserves_interior_and_disjoint_pixel_coverage() {
        let regions = disjoint_regions(
            &[
                [0.0, 0.0, 100.0, 1.0],
                [0.0, 49.0, 100.0, 1.0],
                [0.0, 0.0, 1.0, 50.0],
                [99.0, 0.0, 1.0, 50.0],
            ],
            [0.0, 0.0, 100.0, 50.0],
        );
        assert_eq!(regions.iter().map(|r| r[2] * r[3]).sum::<f32>(), 296.0);
        for (i, a) in regions.iter().enumerate() {
            for b in &regions[i + 1..] {
                assert!(intersection(*a, *b).is_none());
            }
        }
    }
    fn snapshot(text: &str) -> std::sync::Arc<PreparedPresentation> {
        let state = bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(
            0, 0, 8, 4,
        ));
        std::sync::Arc::new(
            state
                .prepare(bmux_tui::damage::Damage::Full, |cx| {
                    cx.write_line(
                        bmux_tui::paint::LocalRect::new(0, 0, 8, 1),
                        &bmux_tui::text::Line::raw(text),
                    );
                })
                .unwrap(),
        )
    }

    #[test]
    fn damage_uses_successful_baseline_not_skipped_or_failed_producer_frame() {
        let mut tracker = DamageTracker::default();
        let a = snapshot("A");
        let b = snapshot("B");
        let c = snapshot("C");
        let cell = [8.25, 16.5];
        let screen = [0.0, 0.0, 66.0, 66.0];
        tracker.committed(a.clone());
        assert!(tracker.regions(&a, cell, screen).is_empty());
        let before = tracker.regions(&c, cell, screen);
        let _failed = tracker.regions(&b, cell, screen);
        assert_eq!(tracker.regions(&c, cell, screen), before);
        assert!(before.iter().map(|r| r[2] * r[3]).sum::<f32>() < screen[2] * screen[3]);
        tracker.committed(c.clone());
        assert!(tracker.regions(&c, cell, screen).is_empty());
        tracker.invalidate();
        assert_eq!(tracker.regions(&c, cell, screen), vec![screen]);
    }

    #[test]
    fn failed_output_does_not_advance_baseline() {
        let mut damage = DamageTracker::default();
        damage.invalidate();
        assert_eq!(damage.generation, 0);
        assert!(damage.previous.is_none());
    }
}
