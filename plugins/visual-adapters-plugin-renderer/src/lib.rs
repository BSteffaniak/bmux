//! Bundled attach-local visual adapters.
//!
//! This crate intentionally lives outside core. It registers common adapter
//! implementations that operate on the generic borrowed visual frame view from
//! `bmux_plugin` and emit compact, plugin-owned payloads.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::sync::{Arc, Once};

use bmux_plugin::{
    AttachVisualAdapter, AttachVisualAdapterOutput, AttachVisualAdapterRequest,
    AttachVisualSurfaceView, register_visual_adapter,
};
use serde::Serialize;

const PRESENCE_BITSET_ADAPTER_ID: &str = "bmux.visual.presence-bitset";

#[derive(Debug, Serialize)]
struct PresenceBitsetPayload {
    request_id: String,
    adapter: String,
    surface_id: uuid::Uuid,
    pane_id: uuid::Uuid,
    grid_revision: u64,
    encoding: String,
    width: u16,
    height: u16,
    words_per_row: u16,
    words: Vec<u32>,
}

struct PresenceBitsetAdapter;

impl AttachVisualAdapter for PresenceBitsetAdapter {
    fn id(&self) -> &str {
        PRESENCE_BITSET_ADAPTER_ID
    }

    fn project(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualAdapterOutput, String> {
        let width = surface.width();
        let height = surface.height();
        let words_per_row = width.saturating_add(31) / 32;
        let mut words = vec![0_u32; usize::from(words_per_row).saturating_mul(usize::from(height))];
        for y in 0..height {
            for x in 0..width {
                let occupied = surface
                    .cell(x, y)
                    .is_some_and(|cell| !cell.wide_continuation && !cell.text.trim().is_empty());
                if occupied {
                    let word_index = usize::from(y)
                        .saturating_mul(usize::from(words_per_row))
                        .saturating_add(usize::from(x / 32));
                    let bit = u32::from(x % 32);
                    if let Some(word) = words.get_mut(word_index) {
                        *word |= 1_u32 << bit;
                    }
                }
            }
        }
        let payload = PresenceBitsetPayload {
            request_id: request.id.clone(),
            adapter: request.adapter.clone(),
            surface_id: surface.surface_id(),
            pane_id: surface.pane_id(),
            grid_revision: surface.grid_revision(),
            encoding: "u32-row-bitset-v1".to_string(),
            width,
            height,
            words_per_row,
            words,
        };
        serde_json::to_writer(&mut *out, &payload).map_err(|error| error.to_string())?;
        Ok(AttachVisualAdapterOutput {
            encoding: "json".to_string(),
            payload: std::mem::take(out),
        })
    }
}

/// Register bundled visual adapters. Idempotent.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        register_visual_adapter(Arc::new(PresenceBitsetAdapter));
        tracing::debug!(
            adapter = PRESENCE_BITSET_ADAPTER_ID,
            "registered visual adapter"
        );
    });
}
