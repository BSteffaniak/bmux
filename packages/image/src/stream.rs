//! Image strings collected as a terminal parser streams them.
//!
//! A parser that recognises image sequences itself (a Sixel DCS string, a Kitty graphics APC
//! string, an iTerm2 `OSC 1337;File=` string) hands their payload to an [`ImageCollector`] as it
//! arrives, and the collector turns a completed one into the [`ImageEvent`] the
//! [`ImageInterceptor`](crate::ImageInterceptor) would have extracted from the same bytes.

use crate::model::{ImageEvent, ImagePosition};

/// Which image sequence a streamed string is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageString {
    /// A Sixel DCS string; its payload follows the introducer's `q`.
    #[cfg(feature = "sixel")]
    Sixel,
    /// A Kitty graphics APC string; its payload begins with `G`.
    #[cfg(feature = "kitty")]
    Kitty,
    /// An iTerm2 inline image; its payload follows `OSC 1337;File=`.
    #[cfg(feature = "iterm2")]
    ITerm2,
}

/// The payload of the image string in progress, bounded: one larger than the limit is discarded
/// whole, as the interceptor discards it.
pub struct ImageCollector {
    kind: Option<ImageString>,
    buf: Vec<u8>,
    max_encoded_image_bytes: usize,
    oversized: bool,
}

impl ImageCollector {
    /// A collector discarding strings larger than `max_encoded_image_bytes` (at least 1).
    #[must_use]
    pub fn with_max_encoded_bytes(max_encoded_image_bytes: usize) -> Self {
        Self {
            kind: None,
            buf: Vec::new(),
            max_encoded_image_bytes: max_encoded_image_bytes.max(1),
            oversized: false,
        }
    }

    /// Begin collecting an image string of `kind` (abandoning any in progress).
    pub fn start(&mut self, kind: ImageString) {
        self.kind = Some(kind);
        self.buf.clear();
        self.oversized = false;
    }

    /// Whether a string is being collected.
    #[must_use]
    pub const fn collecting(&self) -> bool {
        self.kind.is_some()
    }

    /// More of the string's payload.
    pub fn push(&mut self, data: &[u8]) {
        if self.kind.is_none() || self.oversized {
            return;
        }
        if self.buf.len() + data.len() > self.max_encoded_image_bytes {
            self.buf = Vec::new();
            self.oversized = true;
        } else {
            self.buf.extend_from_slice(data);
        }
    }

    /// The string ended: `completed` by its terminator (it becomes an event, unless it was too
    /// large or is no valid command), or cancelled (it is dropped).
    pub fn finish(&mut self, completed: bool) -> Option<ImageEvent> {
        let kind = self.kind.take()?;
        let oversized = std::mem::take(&mut self.oversized);
        let data = std::mem::take(&mut self.buf);
        if !completed || oversized {
            return None;
        }
        image_event(kind, data, 0)
    }
}

/// The event a completed image string of `kind` with payload `data` makes; its position is set by
/// whoever places it.
pub(crate) fn image_event(
    kind: ImageString,
    data: Vec<u8>,
    filtered_byte_offset: usize,
) -> Option<ImageEvent> {
    let position = ImagePosition { row: 0, col: 0 };
    match kind {
        #[cfg(feature = "sixel")]
        ImageString::Sixel => {
            let pixel_size = crate::codec::sixel::estimate_pixel_size(&data);
            Some(ImageEvent::SixelImage {
                data,
                position,
                pixel_size,
                filtered_byte_offset,
            })
        }
        #[cfg(feature = "kitty")]
        ImageString::Kitty => crate::codec::kitty::parse_command(&data, position).map(|command| {
            ImageEvent::KittyCommand {
                command,
                filtered_byte_offset,
            }
        }),
        #[cfg(feature = "iterm2")]
        ImageString::ITerm2 => Some(ImageEvent::ITerm2Image {
            data,
            position,
            filtered_byte_offset,
        }),
    }
}
