//! Linked, opt-in counter demo. Generic remote viewers paint the owner's mirror;
//! they never instantiate this component or execute its effects.
use bmux_presentation::custom::CustomComponentFactory;
use bmux_presentation::{Block, BlockKind};
use bmux_tui::prelude::{
    Component, Constraints, Element, Event, EventCx, EventOutcome, LayoutCx, LayoutNode, PaintCx,
    TextBlock,
};
use std::sync::{Arc, Mutex};

pub(crate) struct CounterFactory(pub Arc<Mutex<i64>>);
impl CustomComponentFactory for CounterFactory {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        match &block.kind {
            BlockKind::Custom { name, .. } if name == "counter" => Ok(Element::new(Counter {
                id: block.id.as_str().to_owned(),
                value: Arc::clone(&self.0),
            })),
            _ => Err("unknown linked demo".into()),
        }
    }
}
struct Counter {
    id: String,
    value: Arc<Mutex<i64>>,
}
impl Counter {
    fn text(&self) -> TextBlock {
        let value = self.value.lock().map_or(0, |value| *value);
        TextBlock::new(format!("Counter: {value}\n+ increment · - decrement"))
            .id(format!("demo/{}/counter", self.id))
    }
}
impl Component for Counter {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.text().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.text().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if cx.find_rect(&layout.id).is_none() {
            return EventOutcome::Ignored;
        }
        let delta = match event {
            Event::Key(key) if key.key == bmux_keyboard::KeyCode::Char('+') => 1,
            Event::Key(key) if key.key == bmux_keyboard::KeyCode::Char('-') => -1,
            Event::Mouse(mouse)
                if mouse.kind
                    == bmux_tui::event::MouseEventKind::Down(
                        bmux_tui::event::MouseButton::Left,
                    ) =>
            {
                let Some(rect) = cx.find_rect(&layout.id) else {
                    return EventOutcome::Ignored;
                };
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
        if let Ok(mut value) = self.value.lock() {
            *value = value.saturating_add(delta);
        }
        EventOutcome::Redraw
    }
}

pub(crate) fn registry(value: &Arc<Mutex<i64>>) -> bmux_presentation::CustomRegistry<'static> {
    let mut registry = bmux_presentation::CustomRegistry::new();
    // A fixed linked allowlist, never a name-to-executable lookup.
    registry
        .register("counter", CounterFactory(Arc::clone(value)))
        .expect("fresh registry accepts counter");
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyCode, KeyStroke};
    #[test]
    fn component_event_uses_exact_painted_layout() {
        let value = Arc::new(Mutex::new(0));
        let component = Counter {
            id: "demo".into(),
            value: Arc::clone(&value),
        };
        let layout = component.layout(Constraints::for_width(30), &mut LayoutCx::default());
        let event = Event::Key(KeyStroke::simple(KeyCode::Char('+')));
        assert_eq!(
            component.event(&event, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Redraw
        );
        assert_eq!(*value.lock().unwrap(), 1);
    }
}
