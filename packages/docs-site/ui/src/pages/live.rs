//! Public, output-only broadcast page. The Canvas is mounted by a versioned module.

use hyperchad::template::{Containers, container};

/// Live audience view; no local credentials or input controls are exposed.
#[must_use]
pub fn live(_page: &hyperchad_docs_site::DocPage) -> Containers {
    let body = container! {
        div direction=(hyperchad::transformer::models::LayoutDirection::Column) gap=16 {
            h1 { "Live broadcast" }
            div { "Read-only output shared by the broadcaster. Paused and offline output is hidden." }
            div #live-status { "Connecting to broadcast…" }
            div #live-broadcast {}
        }
    };
    // HyperChad custom pages bypass DocsSite's shell; wrap explicitly.
    crate::home_layout::with_live_module(crate::home_layout::home_layout(&body))
}
