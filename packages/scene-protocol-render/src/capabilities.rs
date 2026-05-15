//! Capability predicates for scene-protocol rendering.

use bmux_scene_protocol::scene_protocol::{TerminalCapability, TerminalCapabilityQuery};

/// Per-attached-terminal capabilities relevant to scene rendering.
#[allow(clippy::struct_excessive_bools)] // Independent terminal feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneRenderCapabilities {
    pub truecolor: bool,
    pub unicode_box_drawing: bool,
    pub unicode_block_elements: bool,
    pub graphics_kitty: bool,
    pub graphics_sixel: bool,
    pub graphics_iterm2: bool,
    pub graphics_alpha: bool,
    pub cell_pixels: bool,
}

impl Default for SceneRenderCapabilities {
    fn default() -> Self {
        Self {
            truecolor: true,
            unicode_box_drawing: true,
            unicode_block_elements: true,
            graphics_kitty: false,
            graphics_sixel: false,
            graphics_iterm2: false,
            graphics_alpha: false,
            cell_pixels: false,
        }
    }
}

impl SceneRenderCapabilities {
    #[must_use]
    pub const fn supports(self, capability: TerminalCapability) -> bool {
        match capability {
            TerminalCapability::Truecolor => self.truecolor,
            TerminalCapability::UnicodeBoxDrawing => self.unicode_box_drawing,
            TerminalCapability::UnicodeBlockElements => self.unicode_block_elements,
            TerminalCapability::GraphicsKitty => self.graphics_kitty,
            TerminalCapability::GraphicsSixel => self.graphics_sixel,
            TerminalCapability::GraphicsIterm2 => self.graphics_iterm2,
            TerminalCapability::GraphicsAlpha => self.graphics_alpha,
            TerminalCapability::CellPixels => self.cell_pixels,
        }
    }
}

#[must_use]
pub fn capability_query_matches(
    query: Option<&TerminalCapabilityQuery>,
    capabilities: SceneRenderCapabilities,
) -> bool {
    let Some(query) = query else {
        return true;
    };
    query
        .all
        .iter()
        .all(|capability| capabilities.supports(*capability))
        && (query.any.is_empty()
            || query
                .any
                .iter()
                .any(|capability| capabilities.supports(*capability)))
        && query
            .none
            .iter()
            .all(|capability| !capabilities.supports(*capability))
}

#[cfg(test)]
mod tests {
    use super::{SceneRenderCapabilities, capability_query_matches};
    use bmux_scene_protocol::scene_protocol::{TerminalCapability, TerminalCapabilityQuery};

    #[test]
    fn capability_query_matches_all_any_and_none() {
        let caps = SceneRenderCapabilities {
            graphics_kitty: true,
            graphics_alpha: true,
            cell_pixels: true,
            ..SceneRenderCapabilities::default()
        };
        let query = TerminalCapabilityQuery {
            all: vec![TerminalCapability::GraphicsKitty],
            any: vec![
                TerminalCapability::GraphicsSixel,
                TerminalCapability::GraphicsAlpha,
            ],
            none: vec![TerminalCapability::GraphicsIterm2],
        };
        assert!(capability_query_matches(Some(&query), caps));
    }

    #[test]
    fn capability_query_rejects_forbidden_capability() {
        let caps = SceneRenderCapabilities {
            graphics_kitty: true,
            ..SceneRenderCapabilities::default()
        };
        let query = TerminalCapabilityQuery {
            all: Vec::new(),
            any: Vec::new(),
            none: vec![TerminalCapability::GraphicsKitty],
        };
        assert!(!capability_query_matches(Some(&query), caps));
    }
}
