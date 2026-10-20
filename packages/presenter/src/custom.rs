//! Linked, opt-in component factories. There is no dynamic loading or implicit activation.
use crate::model::Block;
use bmux_tui::component::{Component, Element};
use std::collections::BTreeMap;

/// Viewer-local build input; never stored in shared component state.
#[derive(Clone, Debug, Default)]
pub struct PresentationBuildContext {
    pub focused: bool,
    /// Caller-owned per-view transient interaction, never canonical linked state.
    pub interaction: Option<LinkedState>,
}

/// Factories can borrow caller-owned state, including canonical control state.
/// Build must be deterministic/read-only; effects belong to the host event loop.
pub trait CustomComponentFactory {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String>;
    fn build_with_context<'a>(
        &'a self,
        block: &'a Block,
        _context: PresentationBuildContext,
    ) -> Result<Element<'a>, String> {
        self.build(block)
    }
    fn focusable(&self, _block: &Block) -> bool {
        true
    }
    /// Invalidate derived geometry/paint when external caller-owned state changes.
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        bmux_tui::component::ComponentRevision::default()
    }
}

#[derive(Default)]
pub struct CustomRegistry<'a> {
    factories: BTreeMap<String, Box<dyn CustomComponentFactory + 'a>>,
}
impl<'registry> CustomRegistry<'registry> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(
        &mut self,
        name: impl Into<String>,
        factory: impl CustomComponentFactory + 'registry,
    ) -> Result<(), String> {
        let name = name.into();
        if name.is_empty() || self.factories.contains_key(&name) {
            return Err(format!(
                "empty or already registered component name: {name}"
            ));
        }
        self.factories.insert(name, Box::new(factory));
        Ok(())
    }
    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }
    pub fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        self.build_with_context(block, PresentationBuildContext::default())
    }
    pub fn focusable(&self, block: &Block) -> bool {
        let crate::model::BlockKind::Custom { name, .. } = &block.kind else {
            return false;
        };
        self.factories
            .get(name)
            .is_some_and(|factory| factory.focusable(block))
    }
    pub fn build_with_context<'a>(
        &'a self,
        block: &'a Block,
        context: PresentationBuildContext,
    ) -> Result<Element<'a>, String> {
        let crate::model::BlockKind::Custom { name, .. } = &block.kind else {
            return Err("factory requires a custom block".into());
        };
        self.factories
            .get(name)
            .ok_or_else(|| format!("unregistered component: {name}"))?
            .build_with_context(block, context)
    }
    pub fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.factories.values().fold(
            bmux_tui::component::ComponentRevision::default(),
            |revision, factory| revision.combine(factory.revision()),
        )
    }
}

/// Caller-owned state for a trusted linked component. Owners clone the value into
/// detached state during reload preparation; never share the live mutex with a candidate.
pub type LinkedState = std::sync::Arc<std::sync::Mutex<serde_json::Value>>;

/// Application-supplied Rust implementation, not a dynamic executable lookup.
/// A version change explicitly resets state. Initialization and build are read-only
/// with respect to existing state; only authorized component events may mutate it.
pub trait LinkedComponentFactory: Send + Sync {
    fn state_version(&self) -> u64;
    fn initial_state(&self, block: &Block) -> Result<serde_json::Value, String>;
    fn build<'a>(&'a self, block: &'a Block, state: LinkedState) -> Result<Element<'a>, String>;
    /// Read-only projection context. Focus may change paint/cursor, never layout
    /// IDs, sizes, child placement, or shared state. The host owns active focus.
    fn build_with_context<'a>(
        &'a self,
        block: &'a Block,
        state: LinkedState,
        _context: PresentationBuildContext,
    ) -> Result<Element<'a>, String> {
        self.build(block, state)
    }
    fn focusable(&self, _block: &Block) -> bool {
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompatibilityKey {
    pub id: String,
    pub component_type: String,
    pub state_version: u64,
}

/// Explicit linked allowlist supplied by the embedding application.
#[derive(Clone, Default)]
pub struct LinkedRegistry {
    factories: BTreeMap<String, std::sync::Arc<dyn LinkedComponentFactory>>,
}
impl LinkedRegistry {
    pub fn register(
        &mut self,
        name: impl Into<String>,
        factory: impl LinkedComponentFactory + 'static,
    ) -> Result<(), String> {
        let name = name.into();
        if name.is_empty() || self.factories.contains_key(&name) {
            return Err(format!(
                "empty or already registered component name: {name}"
            ));
        }
        self.factories.insert(name, std::sync::Arc::new(factory));
        Ok(())
    }
    pub fn factory(&self, name: &str) -> Option<&std::sync::Arc<dyn LinkedComponentFactory>> {
        self.factories.get(name)
    }
    pub fn compatibility_key(&self, block: &Block) -> Option<CompatibilityKey> {
        let crate::model::BlockKind::Custom { name, .. } = &block.kind else {
            return None;
        };
        Some(CompatibilityKey {
            id: block.id.as_str().to_owned(),
            component_type: name.clone(),
            state_version: self.factory(name)?.state_version(),
        })
    }
}

/// Convenient factory for components that own their inputs.
pub struct OwnedFactory<F, C> {
    build: F,
    marker: std::marker::PhantomData<fn() -> C>,
}
impl<F, C> OwnedFactory<F, C>
where
    F: Fn(&Block) -> Result<C, String>,
    C: Component + 'static,
{
    pub fn new(build: F) -> Self {
        Self {
            build,
            marker: std::marker::PhantomData,
        }
    }
}
impl<F, C> CustomComponentFactory for OwnedFactory<F, C>
where
    F: Fn(&Block) -> Result<C, String>,
    C: Component + 'static,
{
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        (self.build)(block).map(Element::new)
    }
}

/// Rich providers supply prepared code or assets already authorized/resolved by their caller.
/// Code providers return canonical components directly; None uses Markdown fallback.
/// Return None for unavailable assets as well. No provider is installed by default.
pub trait RichContent {
    fn build<'a>(&'a self, block: &'a Block) -> Option<Element<'a>>;
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        bmux_tui::component::ComponentRevision::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::TextBlock;
    #[test]
    fn focus_context_is_build_local_and_invalidates_paint_without_layout() {
        struct ContextFactory;
        struct ContextDisplay(bool);
        impl Component for ContextDisplay {
            fn revision(&self) -> bmux_tui::component::ComponentRevision {
                bmux_tui::component::ComponentRevision::new(0, u64::from(self.0))
            }
            fn layout(
                &self,
                c: bmux_tui::prelude::Constraints,
                cx: &mut bmux_tui::prelude::LayoutCx,
            ) -> bmux_tui::prelude::LayoutNode {
                TextBlock::new("BLUR!").layout(c, cx)
            }
            fn paint(
                &self,
                layout: &bmux_tui::prelude::LayoutNode,
                cx: &mut bmux_tui::prelude::PaintCx<'_, '_>,
            ) {
                TextBlock::new(if self.0 { "FOCUS" } else { "BLUR!" }).paint(layout, cx);
            }
        }
        impl CustomComponentFactory for ContextFactory {
            fn build<'a>(&'a self, _block: &'a Block) -> Result<Element<'a>, String> {
                Ok(Element::new(TextBlock::new("legacy")))
            }
            fn build_with_context<'a>(
                &'a self,
                _block: &'a Block,
                context: PresentationBuildContext,
            ) -> Result<Element<'a>, String> {
                Ok(Element::new(ContextDisplay(context.focused)))
            }
            fn focusable(&self, _block: &Block) -> bool {
                false
            }
        }
        let deck = crate::DeckBuilder::new("context")
            .slide(
                crate::SlideBuilder::new("s")
                    .custom("b", "context", serde_json::Value::Null)
                    .build(),
            )
            .build()
            .unwrap();
        let block = &deck.slides[0].blocks[0];
        let mut registry = CustomRegistry::new();
        registry.register("context", ContextFactory).unwrap();
        assert!(!registry.focusable(block));
        let theme = crate::PresentationTheme::for_deck(&deck);
        let normal = crate::SlideComponent::new(&deck.slides[0], 0, &theme, &registry);
        let focused = crate::SlideComponent::new(&deck.slides[0], 0, &theme, &registry)
            .focused_block(&block.id);
        use bmux_tui::prelude::{Buffer, Constraints, Frame, LayoutCx, PaintCx};
        let layout = normal.layout(Constraints::for_width(30), &mut LayoutCx::default());
        assert_eq!(normal.revision().layout, focused.revision().layout);
        assert_ne!(normal.revision().paint, focused.revision().paint);
        for (component, expected) in [(&normal, "BLUR!"), (&focused, "FOCUS"), (&normal, "BLUR!")] {
            let mut buffer = Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 30, 10));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            let text = buffer
                .cells()
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>();
            assert!(text.contains(expected));
        }
    }
    #[test]
    fn registry_rejects_duplicates_and_unknown_components() {
        let mut registry = CustomRegistry::new();
        registry
            .register(
                "demo",
                OwnedFactory::new(|_block: &Block| Ok(TextBlock::new("demo"))),
            )
            .unwrap();
        assert!(
            registry
                .register(
                    "demo",
                    OwnedFactory::new(|_block: &Block| Ok(TextBlock::new("duplicate")))
                )
                .is_err()
        );
        let block = Block {
            id: "b".into(),
            source: None,
            reveal_step: 0,
            kind: crate::model::BlockKind::Custom {
                name: "missing".into(),
                config: serde_json::Value::Null,
            },
        };
        assert!(registry.build(&block).is_err());
        assert!(registry.contains("demo"));
    }
}
