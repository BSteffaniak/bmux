//! Draws attach UI by publishing a presentation; works the same bundled or
//! packaged as a dynamic library, because only retained JSON crosses over.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

/// Plugin declaration generated from the BPDL package and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/presentation-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

use bmux_plugin_sdk::author::{self, Activation, Call, Declarations, Plugin};
use declaration::commands::PRESENTATION;

/// The retained state kind attach clients draw this plugin's presentation from.
pub const PRESENTATION_KIND: &str = "example.presentation/presentation";

/// A right-hand panel, split off the attach viewport, with a title and one
/// button region. Only the fields the presentation uses are stated.
#[must_use]
pub fn presentation() -> serde_json::Value {
    let owner = "example.presentation";
    let layout = serde_json::json!({ "owner_plugin_id": owner, "local_id": "panel" });
    serde_json::json!({
        "revision": 1,
        "layouts": [{
            "id": layout,
            "order": 0,
            "operation": { "Split": { "edge": "Right", "extent": { "Cells": 18 } } },
        }],
        "surfaces": [{
            "id": {
                "owner_plugin_id": owner,
                "local_id": "panel",
                "retained_id": "6578616d-706c-652e-7072-6573656e7401",
            },
            "revision": 1,
            "target": { "Layout": layout },
            "opaque": true,
            "ops": [
                { "Border": {
                    "rect": { "x": 0, "y": 0, "w": 18, "h": 3 },
                    "glyphs": {
                        "top_left": "╭", "top_right": "╮",
                        "bottom_left": "╰", "bottom_right": "╯",
                        "horizontal": "─", "vertical": "│",
                    },
                    "style": {},
                } },
                { "TextRun": { "x": 1, "y": 1, "text": "from a plugin", "style": { "bold": true } } },
            ],
        }],
    })
}

#[derive(Default)]
pub struct PresentationExample;

async fn print_presentation(
    _cx: &Call,
    _args: declaration::commands::PresentationArgs,
) -> author::Result<()> {
    println!("{}", presentation());
    Ok(())
}

impl Plugin for PresentationExample {
    fn declare(d: &mut Declarations<Self>) {
        d.command(PRESENTATION, print_presentation);
    }

    /// Publish the presentation once; attach clients keep drawing the retained
    /// value, including clients that attach later.
    async fn activate(&self, cx: &mut Activation) -> author::Result<()> {
        let host = cx.host()?;
        host.register_event_channel(PRESENTATION_KIND, true)?;
        host.publish_event(PRESENTATION_KIND, presentation().to_string().as_bytes())?;
        eprintln!("presentation: published");
        Ok(())
    }
}

bmux_plugin_sdk::plugin!(PresentationExample);
