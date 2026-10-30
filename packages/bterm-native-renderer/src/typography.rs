//! Resolved typography and reusable physical geometry. No output-commit authority lives here.

use super::{MAX_LOGICAL_FONT_SIZE, TextError, TextRasterizer};

/// Window typography in logical pixels; DPI is applied separately by the renderer.
#[derive(Clone, Debug, PartialEq)]
pub struct RendererTypography {
    pub font_size: f32,
    pub family: Option<String>,
    pub fallback: Vec<String>,
    /// Multiplier of the face's natural ascent + descent + leading.
    pub line_height: f32,
    /// Additional logical pixels per terminal column (not per shaped glyph).
    pub letter_spacing: f32,
}

impl Default for RendererTypography {
    fn default() -> Self {
        Self {
            font_size: 16.0,
            family: None,
            fallback: Vec::new(),
            line_height: 1.0,
            letter_spacing: 0.0,
        }
    }
}

impl RendererTypography {
    pub(crate) fn validate(&self) -> Result<(), TextError> {
        let valid_name = |name: &str| !name.trim().is_empty() && name.len() <= 256;
        if !self.font_size.is_finite()
            || !(1.0..=MAX_LOGICAL_FONT_SIZE).contains(&self.font_size)
            || !self.line_height.is_finite()
            || !(0.25..=8.0).contains(&self.line_height)
            || !self.letter_spacing.is_finite()
            || !(-128.0..=128.0).contains(&self.letter_spacing)
            || self.family.as_deref().is_some_and(|name| !valid_name(name))
            || self.fallback.len() > 32
            || self.fallback.iter().any(|name| !valid_name(name))
        {
            return Err(TextError::InvalidTypography);
        }
        Ok(())
    }
}

/// Unrounded physical-pixel geometry shared by painting and pointer inversion.
/// Consumers must retain the geometry belonging to the last committed output,
/// rather than using a pending typography change after a failed presentation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RendererGeometry {
    pub cell_size: [f32; 2],
    pub baseline: f32,
}

impl RendererGeometry {
    pub(crate) fn resolve(
        text: &TextRasterizer,
        typography: &RendererTypography,
        scale: f32,
    ) -> Result<Self, TextError> {
        typography.validate()?;
        if !scale.is_finite() || scale <= 0.0 {
            return Err(TextError::InvalidPixelSize);
        }
        let metrics = text.metrics(typography.font_size * scale)?;
        let natural_height = metrics.ascent + metrics.descent + metrics.leading;
        let cell_size = [
            metrics.advance + typography.letter_spacing * scale,
            natural_height * typography.line_height,
        ];
        if cell_size
            .iter()
            .any(|size| !size.is_finite() || *size <= 0.0)
        {
            return Err(TextError::InvalidTypography);
        }
        Ok(Self {
            cell_size,
            // Distribute additional (or reduced) leading equally around the em box.
            baseline: metrics.ascent + (cell_size[1] - natural_height) * 0.5,
        })
    }

    pub fn cell_origin(self, column: f32, row: f32) -> [f32; 2] {
        [column * self.cell_size[0], row * self.cell_size[1]]
    }

    /// Physical bounds for accessibility/IME or other caller-owned projection.
    /// Uses the same promoted cell dimensions as f64 pointer inversion. Convert
    /// to logical window coordinates only once, using the committed frame's DPI.
    pub fn cell_rect_f64(self, column: u16, row: u16, columns: u16, rows: u16) -> [f64; 4] {
        let [width, height] = self.cell_size.map(f64::from);
        [
            f64::from(column) * width,
            f64::from(row) * height,
            f64::from(columns) * width,
            f64::from(rows) * height,
        ]
    }

    /// Continuous f64 coordinates: floor only after checking the committed
    /// presentation's origin, allocation, and clip. Do not narrow pointer input
    /// to f32 before division; that can move a point across a cell boundary.
    pub fn physical_to_cell_f64(self, point: [f64; 2]) -> Option<[f64; 2]> {
        if point.iter().any(|value| !value.is_finite() || *value < 0.0)
            || self
                .cell_size
                .iter()
                .any(|size| !size.is_finite() || *size <= 0.0)
        {
            return None;
        }
        let position = [
            point[0] / f64::from(self.cell_size[0]),
            point[1] / f64::from(self.cell_size[1]),
        ];
        position
            .iter()
            .all(|value| value.is_finite())
            .then_some(position)
    }

    /// Continuous terminal coordinates, before caller-owned origin/clip checks.
    /// Negative, NaN and infinite physical points have no terminal target.
    pub fn physical_to_cell(self, point: [f32; 2]) -> Option<[f32; 2]> {
        if point.iter().any(|value| !value.is_finite() || *value < 0.0)
            || self
                .cell_size
                .iter()
                .any(|size| !size.is_finite() || *size <= 0.0)
        {
            return None;
        }
        let position = [point[0] / self.cell_size[0], point[1] / self.cell_size[1]];
        position
            .iter()
            .all(|value| value.is_finite())
            .then_some(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_transition_resolves_requested_pair_not_intermediate_dpi() {
        let text = TextRasterizer::bundled().unwrap();
        let old = RendererTypography {
            font_size: 128.0,
            ..Default::default()
        };
        let new = RendererTypography {
            font_size: 256.0,
            ..Default::default()
        };
        let before = RendererGeometry::resolve(&text, &old, 2.0).unwrap();
        assert!(RendererGeometry::resolve(&text, &new, 2.0).is_err());
        let after = RendererGeometry::resolve(&text, &new, 1.0).unwrap();
        assert_eq!(before, after);
        // The reverse display transition must resolve its new clamped size
        // with the new DPI too, instead of scaling the old 256-logical size.
        assert!(RendererGeometry::resolve(&text, &new, 2.0).is_err());
        assert_eq!(RendererGeometry::resolve(&text, &old, 2.0).unwrap(), after);
    }

    #[test]
    fn f64_pointer_floor_and_accessibility_bounds_use_identical_cells() {
        let geometry = RendererGeometry {
            cell_size: [9.73125, 18.3125],
            baseline: 14.25,
        };
        for column in [1, 37, 1024, u16::MAX - 1] {
            let rect = geometry.cell_rect_f64(column, 7, 1, 1);
            for (offset, expected) in [(-0.000001, column - 1), (0.000001, column)] {
                let hit = geometry
                    .physical_to_cell_f64([rect[0] + offset, rect[1] + rect[3] * 0.5])
                    .unwrap();
                assert_eq!(hit[0].floor(), f64::from(expected));
                assert_eq!(hit[1].floor(), 7.0);
            }
        }
        assert!(
            geometry
                .physical_to_cell_f64([f64::INFINITY, 0.0])
                .is_none()
        );
        assert!(geometry.physical_to_cell_f64([-0.01, 0.0]).is_none());
    }

    #[test]
    fn fractional_geometry_is_smooth_and_invertible() {
        let text = TextRasterizer::bundled().unwrap();
        let mut typography = RendererTypography::default();
        let a = RendererGeometry::resolve(&text, &typography, 1.0).unwrap();
        typography.font_size = 16.01;
        let b = RendererGeometry::resolve(&text, &typography, 1.0).unwrap();
        for axis in 0..2 {
            assert!(b.cell_size[axis] > a.cell_size[axis]);
            assert!(b.cell_size[axis] - a.cell_size[axis] < 0.1);
        }
        let point = b.cell_origin(37.25, 12.5);
        let inverse = b.physical_to_cell(point).unwrap();
        assert!((inverse[0] - 37.25).abs() < 0.0001);
        assert!((inverse[1] - 12.5).abs() < 0.0001);
        assert!(b.physical_to_cell([-1.0, 0.0]).is_none());
        assert!(b.physical_to_cell([f32::NAN, 0.0]).is_none());
    }

    #[test]
    fn line_height_spacing_and_dpi_share_one_geometry() {
        let text = TextRasterizer::bundled().unwrap();
        let base = RendererTypography::default();
        let a = RendererGeometry::resolve(&text, &base, 1.0).unwrap();
        let typography = RendererTypography {
            line_height: 1.5,
            letter_spacing: 0.75,
            ..base
        };
        let b = RendererGeometry::resolve(&text, &typography, 2.0).unwrap();
        assert!((b.cell_size[0] - (a.cell_size[0] + 0.75) * 2.0).abs() < 0.001);
        assert!((b.cell_size[1] - a.cell_size[1] * 3.0).abs() < 0.001);
        assert!((b.baseline - (a.baseline * 2.0 + a.cell_size[1] * 0.5)).abs() < 0.001);
    }

    #[test]
    fn invalid_typography_is_rejected_before_resolution() {
        let text = TextRasterizer::bundled().unwrap();
        for typography in [
            RendererTypography {
                font_size: f32::NAN,
                ..Default::default()
            },
            RendererTypography {
                line_height: 0.0,
                ..Default::default()
            },
            RendererTypography {
                letter_spacing: -128.0,
                ..Default::default()
            },
            RendererTypography {
                family: Some(String::new()),
                ..Default::default()
            },
            RendererTypography {
                fallback: vec!["x".into(); 33],
                ..Default::default()
            },
        ] {
            assert!(RendererGeometry::resolve(&text, &typography, 1.0).is_err());
        }
    }
}
