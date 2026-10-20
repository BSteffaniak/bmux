//! Adapter-created image storage. Owning the image here (rather than accepting
//! a caller Arc) is the proof that its wire bytes cannot subsequently change.
use std::ops::Deref;
use std::sync::Arc;
#[cfg(feature = "kitty")]
use std::sync::{Mutex, OnceLock, Weak};

use crate::model::PaneImage;

#[derive(Debug)]
pub(crate) struct FrozenImage {
    image: Arc<PaneImage>,
    #[cfg(feature = "kitty")]
    fingerprint: OnceLock<u64>,
    #[cfg(feature = "kitty")]
    binding: Mutex<Option<(u64, Weak<()>)>>,
}

impl FrozenImage {
    pub(crate) fn new(image: PaneImage) -> Arc<Self> {
        Arc::new(Self {
            image: Arc::new(image),
            #[cfg(feature = "kitty")]
            fingerprint: OnceLock::new(),
            #[cfg(feature = "kitty")]
            binding: Mutex::new(None),
        })
    }

    // The shared image is only borrowed for the generic fragment cache. There
    // is no owned Arc escape or mutable accessor from this module.
    pub(crate) fn shared(&self) -> &Arc<PaneImage> {
        &self.image
    }

    #[cfg(feature = "kitty")]
    pub(crate) fn wire(&self) -> (crate::model::KittyFormat, &[u8], u32, u32) {
        use crate::model::{KittyFormat, PixelFormat};
        let pixels = self.image.payload.pixels.as_ref().expect("validated image");
        let (format, bytes) = match pixels.format {
            PixelFormat::Png => (KittyFormat::Png, pixels.data.as_slice()),
            PixelFormat::Rgba8 => self
                .image
                .payload
                .raw
                .as_ref()
                .map_or((KittyFormat::Rgba, pixels.data.as_slice()), |png| {
                    (KittyFormat::Png, png.as_slice())
                }),
            PixelFormat::Rgb8 => (KittyFormat::Rgb, pixels.data.as_slice()),
        };
        (format, bytes, pixels.width, pixels.height)
    }

    // A hint can be shared across candidate clones, but only the authoritative
    // residency entry's generation makes it valid. It never keeps an evicted
    // resource alive, and there is only one bounded hint per representation.
    #[cfg(feature = "kitty")]
    pub(crate) fn binding(&self) -> Option<(u64, Arc<()>)> {
        self.binding
            .lock()
            .expect("frozen binding lock")
            .as_ref()
            .and_then(|(id, generation)| Some((*id, generation.upgrade()?)))
    }

    #[cfg(feature = "kitty")]
    pub(crate) fn bind(&self, id: u64, generation: &Arc<()>) {
        *self.binding.lock().expect("frozen binding lock") = Some((id, Arc::downgrade(generation)));
    }

    #[cfg(feature = "kitty")]
    pub(crate) fn fingerprint(&self) -> u64 {
        *self.fingerprint.get_or_init(|| {
            let (format, bytes, width, height) = self.wire();
            super::compositor::residency::Residency::fingerprint(format, width, height, bytes)
        })
    }
}

impl Deref for FrozenImage {
    type Target = PaneImage;

    fn deref(&self) -> &Self::Target {
        &self.image
    }
}
