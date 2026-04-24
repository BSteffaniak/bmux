//! Client-side render extension for the bmux decoration plugin.
//!
//! The decoration plugin runs in the bmux server process. Every time
//! its internal state changes it publishes a
//! [`bmux_scene_protocol::scene_protocol::DecorationScene`] on the
//! typed plugin event bus; the server relays the event to streaming
//! clients as a `ServerEvent::PluginBusEvent` over the attach IPC
//! stream.
//!
//! This crate consumes those relayed scenes on the client side:
//!
//! 1. [`install`] registers an
//!    [`bmux_plugin::AttachRenderExtension`] and spawns a subscriber
//!    that listens on the client-side
//!    [`bmux_plugin::global_event_bus`] for the
//!    `bmux.scene/scene-protocol` broadcast.
//! 2. Every incoming scene replaces the extension's cached scene
//!    (revision-guarded so stale wire events can't downgrade).
//! 3. On every attach-render pass, the extension's `apply_surface`
//!    looks up the matching surface and hands its paint commands to
//!    [`bmux_scene_protocol_render::paint::apply_paint_commands`].
//!
//! The CLI's streaming loop is responsible for decoding the IPC
//! `PluginBusEvent` payloads and re-emitting them onto the local
//! event bus; this crate subscribes locally and has no direct IPC
//! awareness.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use bmux_plugin::{AttachRenderExtension, ExtensionRect};
use bmux_scene_protocol::scene_protocol::{
    DecorationScene, EVENT_KIND as SCENE_EVENT_KIND, Rect as SceneRect, SurfaceDecoration,
};
use bmux_scene_protocol_render::paint::apply_paint_commands;
use uuid::Uuid;

/// Default broadcast-channel capacity for the client-side
/// scene-protocol subscription.
const SCENE_CHANNEL_CAPACITY: usize = 64;

/// Shared cache of the decoration plugin's latest scene. Stored
/// under `Arc<Mutex<_>>` so both the subscriber thread and the
/// render extension can read/write without unwrapping poisoned
/// locks at every call site.
#[derive(Default)]
struct DecorationRendererCache {
    revision: u64,
    surfaces: BTreeMap<Uuid, SurfaceDecoration>,
}

impl DecorationRendererCache {
    fn replace_if_newer(&mut self, scene: DecorationScene) -> bool {
        if scene.revision < self.revision {
            return false;
        }
        self.revision = scene.revision;
        self.surfaces = scene.surfaces;
        true
    }

    fn surface(&self, surface_id: &Uuid) -> Option<&SurfaceDecoration> {
        self.surfaces.get(surface_id)
    }
}

/// Render extension that applies the decoration plugin's
/// per-surface paint commands to the attach render stream.
struct DecorationRenderExtension {
    name: String,
    cache: Arc<Mutex<DecorationRendererCache>>,
}

impl AttachRenderExtension for DecorationRenderExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn apply_surface(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        _surface_rect: &ExtensionRect,
    ) -> io::Result<bool> {
        let Ok(cache) = self.cache.lock() else {
            return Ok(false);
        };
        let Some(surface) = cache.surface(&surface_id) else {
            return Ok(false);
        };
        if surface.paint_commands.is_empty() {
            return Ok(false);
        }
        // `apply_paint_commands` is generic over `W: io::Write` and
        // requires `Sized` because of the `crossterm::queue!` macro's
        // internals. Rebinding our `&mut dyn io::Write` to a local
        // `&mut impl io::Write` (the reborrow `&mut *stdout` creates
        // a fresh `&mut dyn io::Write`, which is itself Sized) lets
        // the generic bound see a Sized writer.
        let mut writer: &mut dyn io::Write = &mut *stdout;
        apply_paint_commands(&mut writer, surface)
            .map(|()| true)
            .map_err(|err| io::Error::other(err.to_string()))
    }

    fn content_rect_override(&self, surface_id: Uuid) -> Option<ExtensionRect> {
        let cache = self.cache.lock().ok()?;
        let surface = cache.surface(&surface_id)?;
        Some(extension_rect_from_scene(&surface.content_rect))
    }
}

fn extension_rect_from_scene(rect: &SceneRect) -> ExtensionRect {
    ExtensionRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

/// Process-wide handle to the installed extension's cache. `install`
/// stores it on first call; the scene-event relay (living in the
/// CLI's streaming loop) uses [`push_scene`] to update the cache.
static INSTALLED_CACHE: OnceLock<Arc<Mutex<DecorationRendererCache>>> = OnceLock::new();

/// Install the decoration render extension.
///
/// Idempotent: the first call registers an `AttachRenderExtension`
/// with [`bmux_plugin::register_render_extension`] and remembers the
/// installed cache handle; subsequent calls return immediately.
///
/// Call this once during CLI bootstrap when the decoration plugin is
/// bundled. No `install` call means no decoration painting —
/// deployments that don't bundle the decoration plugin can simply
/// skip this crate.
pub fn install() {
    // SAFETY: both `OnceLock`s coordinate single-shot initialisation;
    // repeat calls are no-ops after the first.
    static SUBSCRIBER_SPAWNED: OnceLock<()> = OnceLock::new();
    let cache = INSTALLED_CACHE.get_or_init(|| {
        let cache: Arc<Mutex<DecorationRendererCache>> =
            Arc::new(Mutex::new(DecorationRendererCache::default()));
        let ext = Arc::new(DecorationRenderExtension {
            name: "bmux.decoration.renderer".to_string(),
            cache: cache.clone(),
        }) as Arc<dyn AttachRenderExtension>;
        bmux_plugin::register_render_extension(ext);
        // Register a local broadcast channel for scene events. The
        // CLI's streaming loop re-publishes IPC-delivered
        // `PluginBusEvent`s onto this channel so any render
        // extension can subscribe without touching transport.
        let _ = bmux_plugin::global_event_bus().register_channel_with_capacity::<DecorationScene>(
            SCENE_EVENT_KIND,
            SCENE_CHANNEL_CAPACITY,
        );
        tracing::debug!("decoration render extension installed");
        cache
    });
    // Spawn the subscriber thread lazily alongside `cache` init so a
    // second `install()` doesn't double-spawn.
    if SUBSCRIBER_SPAWNED.get().is_none() {
        let _ = SUBSCRIBER_SPAWNED.set(());
        spawn_scene_subscriber(cache.clone());
    }
}

/// Manual push path. Callers that receive scene payloads from a
/// transport other than the local event bus (e.g. the CLI's
/// streaming loop decoding IPC `PluginBusEvent`s) call this to
/// update the cache directly.
pub fn push_scene(scene: DecorationScene) -> bool {
    let Some(cache) = INSTALLED_CACHE.get() else {
        return false;
    };
    let Ok(mut guard) = cache.lock() else {
        return false;
    };
    guard.replace_if_newer(scene)
}

fn spawn_scene_subscriber(cache: Arc<Mutex<DecorationRendererCache>>) {
    let receiver = bmux_plugin::global_event_bus().subscribe::<DecorationScene>(&SCENE_EVENT_KIND);
    let Ok(mut rx) = receiver else {
        tracing::warn!(
            "decoration render extension: scene-protocol broadcast channel not registered; \
             events pushed via push_scene only"
        );
        return;
    };
    std::thread::spawn(move || {
        // Construct a dedicated current-thread tokio runtime so we
        // can await the `broadcast::Receiver::recv` future without
        // requiring the extension crate to be tokio-aware at its
        // call sites.
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::error!(
                "decoration render extension: failed to build tokio runtime for scene subscriber"
            );
            return;
        };
        runtime.block_on(async move {
            while let Ok(scene) = rx.recv().await {
                let scene = (*scene).clone();
                if let Ok(mut guard) = cache.lock() {
                    guard.replace_if_newer(scene);
                }
            }
            tracing::debug!("decoration render extension subscriber loop exited");
        });
    });
}
