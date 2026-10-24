//! Reconstructible, byte-bounded source and fragment caches.
use std::sync::Arc;

use crate::model::{ImagePayload, ImageProtocol, PaneImage, PixelBuffer};
use bmux_tui::geometry::Rect;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;

#[derive(Clone, Debug)]
struct Source {
    protocol: ImageProtocol,
    payload: Arc<PaneImage>,
    pixels: Arc<PixelBuffer>,
}

#[derive(Clone, Debug)]
pub(super) struct Fragment {
    source: Arc<PaneImage>,
    protocol: ImageProtocol,
    crop: Rect,
    size: (u16, u16),
    host: (Option<ImageProtocol>, u16, u16),
    pub pixels: Arc<PixelBuffer>,
    pub encoded: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Cache {
    sources: Vec<Source>,
    fragments: Vec<Fragment>,
    pub decodes: usize,
    pub preparations: usize,
}

impl Cache {
    pub fn clear(&mut self) {
        self.sources.clear();
        self.fragments.clear();
    }

    fn bytes(&self) -> usize {
        self.sources
            .iter()
            .map(|source| payload_bytes(&source.payload.payload) + source.pixels.data.len())
            .sum::<usize>()
            + self
                .fragments
                .iter()
                .map(|fragment| {
                    payload_bytes(&fragment.source.payload)
                        + fragment.pixels.data.len()
                        + fragment.encoded.len()
                })
                .sum::<usize>()
    }

    pub fn trim(&mut self) {
        while self.bytes() > MAX_BYTES || self.sources.len() + self.fragments.len() > MAX_ENTRIES {
            if !self.sources.is_empty() {
                self.sources.remove(0);
            } else if !self.fragments.is_empty() {
                self.fragments.remove(0);
            } else {
                break;
            }
        }
    }

    pub fn decoded(
        &mut self,
        image: &PaneImage,
        shared: Option<&Arc<PaneImage>>,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> std::io::Result<Arc<PixelBuffer>> {
        if let Some(index) = self.sources.iter().position(|source| {
            source.protocol == image.protocol && same(&source.payload, image, shared)
        }) {
            let source = self.sources.remove(index);
            let pixels = Arc::clone(&source.pixels);
            self.sources.push(source);
            return Ok(pixels);
        }
        let pixels = Arc::new(if let Some(token) = cancelled {
            let png = image
                .payload
                .pixels
                .as_ref()
                .filter(|p| p.format == crate::model::PixelFormat::Png)
                .map(|p| p.data.as_slice())
                .or_else(|| {
                    (image.protocol == crate::model::ImageProtocol::KittyGraphics)
                        .then_some(image.payload.raw.as_deref())
                        .flatten()
                });
            if let Some(bytes) = png {
                super::clipping::decode_png_cancellable(bytes, 64 * 1024 * 1024, || {
                    token.load(std::sync::atomic::Ordering::Acquire)
                })?
            } else {
                super::clipping::decoded(image)?
            }
        } else {
            super::clipping::decoded(image)?
        });
        self.decodes += 1;
        self.sources.push(Source {
            protocol: image.protocol,
            payload: shared.cloned().unwrap_or_else(|| Arc::new(image.clone())),
            pixels: Arc::clone(&pixels),
        });
        self.trim();
        Ok(pixels)
    }

    pub fn find(
        &mut self,
        image: &PaneImage,
        shared: Option<&Arc<PaneImage>>,
        crop: Rect,
        size: (u16, u16),
        host: (Option<ImageProtocol>, u16, u16),
    ) -> Option<Fragment> {
        let matches_geometry = |fragment: &Fragment| {
            fragment.protocol == image.protocol
                && fragment.crop == crop
                && fragment.size == size
                && fragment.host == host
        };
        let index = shared
            .and_then(|shared| {
                self.fragments.iter().position(|fragment| {
                    matches_geometry(fragment) && Arc::ptr_eq(&fragment.source, shared)
                })
            })
            .or_else(|| {
                self.fragments.iter().position(|fragment| {
                    matches_geometry(fragment) && same(&fragment.source, image, shared)
                })
            })?;
        let fragment = self.fragments.remove(index);
        self.fragments.push(fragment.clone());
        Some(fragment)
    }

    #[allow(clippy::too_many_arguments)] // Explicit source, geometry, host negotiation and owned results form one cache entry.
    pub fn insert(
        &mut self,
        image: &PaneImage,
        shared: Option<&Arc<PaneImage>>,
        crop: Rect,
        size: (u16, u16),
        host: (Option<ImageProtocol>, u16, u16),
        pixels: Arc<PixelBuffer>,
        encoded: Vec<u8>,
    ) -> Fragment {
        let source = self
            .sources
            .iter()
            .find(|source| {
                source.protocol == image.protocol && same(&source.payload, image, shared)
            })
            .map_or_else(
                || shared.cloned().unwrap_or_else(|| Arc::new(image.clone())),
                |source| Arc::clone(&source.payload),
            );
        let fragment = Fragment {
            source,
            protocol: image.protocol,
            crop,
            size,
            host,
            pixels,
            encoded: Arc::new(encoded),
        };
        self.preparations += 1;
        self.fragments.push(fragment.clone());
        self.trim();
        fragment
    }
}

fn payload_bytes(payload: &ImagePayload) -> usize {
    payload.raw.as_ref().map_or(0, Vec::len)
        + payload
            .pixels
            .as_ref()
            .map_or(0, |pixels| pixels.data.len())
}

fn same(source: &Arc<PaneImage>, image: &PaneImage, shared: Option<&Arc<PaneImage>>) -> bool {
    shared.is_some_and(|shared| Arc::ptr_eq(source, shared)) || source.payload == image.payload
}
