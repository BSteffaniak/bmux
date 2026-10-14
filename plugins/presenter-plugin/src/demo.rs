//! Linked example controls and an inert playbook-demo allowlist. Viewers consume mirrors.
use bmux_presenter::Block;
use bmux_presenter::custom::{LinkedComponentFactory, LinkedRegistry, LinkedState};
use bmux_tui::prelude::{
    Component, Constraints, Element, Event, EventCx, EventOutcome, LayoutCx, LayoutNode, PaintCx,
    TextBlock,
};

struct CounterFactory;
impl LinkedComponentFactory for CounterFactory {
    fn state_version(&self) -> u64 {
        1
    }
    fn initial_state(&self, _block: &Block) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!(0))
    }
    fn build<'a>(&'a self, block: &'a Block, state: LinkedState) -> Result<Element<'a>, String> {
        Ok(Element::new(ExampleControl {
            id: block.id.as_str().to_owned(),
            state,
            toggle: false,
        }))
    }
}
struct ToggleFactory;
impl LinkedComponentFactory for ToggleFactory {
    fn state_version(&self) -> u64 {
        1
    }
    fn initial_state(&self, _block: &Block) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!(false))
    }
    fn build<'a>(&'a self, block: &'a Block, state: LinkedState) -> Result<Element<'a>, String> {
        Ok(Element::new(ExampleControl {
            id: block.id.as_str().to_owned(),
            state,
            toggle: true,
        }))
    }
}
struct ExampleControl {
    id: String,
    state: LinkedState,
    toggle: bool,
}
impl ExampleControl {
    fn text(&self) -> TextBlock {
        let state = self.state.lock().expect("example state mutex poisoned");
        let text = if self.toggle {
            format!(
                "Toggle: {}\nspace or click to switch",
                state.as_bool().unwrap_or(false)
            )
        } else {
            format!(
                "Counter: {}\n+ increment · - decrement",
                state.as_i64().unwrap_or(0)
            )
        };
        TextBlock::new(text).id(format!("demo/{}", self.id))
    }
}
impl Component for ExampleControl {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.text().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.text().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(rect) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let delta = match event {
            Event::Key(key) if self.toggle && key.key == bmux_keyboard::KeyCode::Char(' ') => 1,
            Event::Key(key) if !self.toggle && key.key == bmux_keyboard::KeyCode::Char('+') => 1,
            Event::Key(key) if !self.toggle && key.key == bmux_keyboard::KeyCode::Char('-') => -1,
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
                ) =>
            {
                if mouse.position.x < rect.x
                    || mouse.position.y < rect.y
                    || mouse.position.x >= rect.x.saturating_add(rect.width)
                    || mouse.position.y >= rect.y.saturating_add(rect.height)
                {
                    return EventOutcome::Ignored;
                }
                1
            }
            _ => return EventOutcome::Ignored,
        };
        let Ok(mut state) = self.state.lock() else {
            return EventOutcome::Ignored;
        };
        *state = if self.toggle {
            serde_json::json!(!state.as_bool().unwrap_or(false))
        } else {
            serde_json::json!(state.as_i64().unwrap_or(0).saturating_add(delta))
        };
        EventOutcome::Redraw
    }
}

/// Example application registry containing independent counter and toggle types.
/// Hosts retain authority; remote viewers never instantiate these factories.
pub fn linked_registry() -> LinkedRegistry {
    let mut registry = LinkedRegistry::default();
    registry
        .register("counter", CounterFactory)
        .expect("unique example registration");
    registry
        .register("toggle", ToggleFactory)
        .expect("unique example registration");
    registry
        .register("playbook-demo", crate::playbook_live::DemoFactory)
        .expect("unique example registration");
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[test]
    fn second_component_uses_exact_painted_layout() {
        let state = Arc::new(Mutex::new(serde_json::json!(false)));
        let component = ExampleControl {
            id: "switch".into(),
            state: Arc::clone(&state),
            toggle: true,
        };
        let layout = component.layout(Constraints::for_width(30), &mut LayoutCx::default());
        let event = Event::Key(bmux_keyboard::KeyStroke::simple(
            bmux_keyboard::KeyCode::Char(' '),
        ));
        assert_eq!(
            component.event(&event, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Redraw
        );
        assert_eq!(*state.lock().unwrap(), serde_json::json!(true));
    }
}
