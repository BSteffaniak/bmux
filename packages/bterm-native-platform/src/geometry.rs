//! Pixel geometry at the native window boundary. No terminal-cell coordinates live here.

/// A drawable extent in physical framebuffer pixels (zero means minimized).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

impl PhysicalSize {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// An extent in window-system logical pixels, not execution cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    pub width: f64,
    pub height: f64,
}

/// A physical rectangle used for native IME positioning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Atomic geometry snapshot. Consumers must resize presentation independently of PTYs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowMetrics {
    pub physical_size: PhysicalSize,
    pub scale_factor: f64,
}

impl WindowMetrics {
    #[must_use]
    pub fn logical_size(self) -> LogicalSize {
        LogicalSize {
            width: f64::from(self.physical_size.width) / self.scale_factor,
            height: f64::from(self.physical_size.height) / self.scale_factor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_and_logical_extents_are_distinct() {
        let metrics = WindowMetrics {
            physical_size: PhysicalSize {
                width: 1500,
                height: 900,
            },
            scale_factor: 1.5,
        };
        assert_eq!(
            metrics.logical_size(),
            LogicalSize {
                width: 1000.0,
                height: 600.0
            }
        );
        assert!(!metrics.physical_size.is_empty());
        assert!(
            PhysicalSize {
                width: 0,
                height: 900
            }
            .is_empty()
        );
    }
}
