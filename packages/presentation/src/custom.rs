//! Linked, opt-in component factories. There is no dynamic loading or implicit activation.
use crate::model::Block;
use bmux_tui::component::{Component, Element};
use std::collections::BTreeMap;

/// Factories can borrow caller-owned state, including canonical control state.
/// Build must be deterministic/read-only; effects belong to the host event loop.
pub trait CustomComponentFactory {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String>;
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
        let crate::model::BlockKind::Custom { name, .. } = &block.kind else {
            return Err("factory requires a custom block".into());
        };
        self.factories
            .get(name)
            .ok_or_else(|| format!("unregistered component: {name}"))?
            .build(block)
    }
    pub fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.factories.values().fold(
            bmux_tui::component::ComponentRevision::default(),
            |revision, factory| revision.combine(factory.revision()),
        )
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
