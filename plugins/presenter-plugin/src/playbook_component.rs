//! Thin live-playbook view. Retained display and lifecycle ownership stay with caller.
use crate::playbook_controller::{Control, Controller, LifecycleSnapshot};
use bmux_tui::{
    embedded_surface::{EmbeddedSurface, RetainedSurface},
    prelude::{
        Column, Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome,
        LayoutCx, LayoutNode, PaintCx, TextBlock,
    },
};
use std::hash::{Hash, Hasher};

fn bounded_diagnostic(source: &str) -> String {
    // Runner diagnostics are private operator output, never audience metadata.
    // Bound Unicode scalar count and strip terminal controls before rendering.
    let mut diagnostic = source
        .chars()
        .take(2048)
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .collect::<String>();
    if source.chars().nth(2048).is_some() {
        diagnostic.push_str("\n[diagnostic truncated]");
    }
    diagnostic
}

pub struct PlaybookDemo<'a> {
    title: &'a str,
    controller: &'a Controller,
    snapshot: &'a LifecycleSnapshot,
    audience: bool,
    surface: Option<&'a RetainedSurface>,
}
impl<'a> PlaybookDemo<'a> {
    pub fn new(
        title: &'a str,
        controller: &'a Controller,
        snapshot: &'a LifecycleSnapshot,
    ) -> Self {
        Self {
            title,
            controller,
            snapshot,
            audience: false,
            surface: None,
        }
    }
    pub fn audience(mut self, audience: bool) -> Self {
        self.audience = audience;
        self
    }
    /// Complete real committed attachment frame. Never runner stdout/diagnostics.
    pub fn retained_surface(mut self, surface: &'a RetainedSurface) -> Self {
        self.surface = Some(surface);
        self
    }
    /// Owner-event admission retains an explicit result for queue-full reporting.
    pub fn submit(&self, control: Control) -> Result<(), String> {
        if self.audience {
            return Err("audience view cannot admit playbook controls".into());
        }
        if self.snapshot.owner_epoch != self.controller.owner_epoch() {
            return Err("stale playbook controller epoch".into());
        }
        self.controller.admit(
            self.snapshot
                .attempt
                .as_ref()
                .map(|attempt| attempt.generation),
            control,
        )
    }
    fn tree(&self) -> Element<'_> {
        let status = self.snapshot.attempt.as_ref().map_or_else(
            || "Not running".to_owned(),
            |attempt| {
                format!(
                    "{} · step {}{}",
                    attempt.phase,
                    attempt.step,
                    if attempt.paused { " · paused" } else { "" }
                )
            },
        );
        let mut column = Column::new().child(TextBlock::new(format!("{}\n{status}", self.title)));
        if let Some(surface) = self.surface {
            column = column.child(EmbeddedSurface::new(surface));
        }
        if !self.audience {
            if let Some(diagnostic) = self
                .snapshot
                .attempt
                .as_ref()
                .and_then(|attempt| attempt.diagnostic.as_deref())
            {
                column = column.child(TextBlock::new(bounded_diagnostic(diagnostic)));
            }
            if let Some(error) = &self.snapshot.error {
                column = column.child(TextBlock::new(bounded_diagnostic(error)));
            }
            column = column.child(TextBlock::new(
                "r Run · s Stop · R Restart · p Pause · n Step · c Resume · Esc Hide · q Exit",
            ));
        }
        Element::new(column)
    }
}
impl Component for PlaybookDemo<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut revision = std::collections::hash_map::DefaultHasher::new();
        self.title.hash(&mut revision);
        self.snapshot.owner_epoch.hash(&mut revision);
        self.snapshot.revision.hash(&mut revision);
        // EmbeddedSurface contributes its own content revision through its tree.
        let child = self.tree().revision();
        child.hash(&mut revision);
        let token = revision.finish();
        ComponentRevision::new(token, token)
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
    }
    fn event(&self, event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        if self.audience || self.snapshot.owner_epoch != self.controller.owner_epoch() {
            return EventOutcome::Ignored;
        }
        let Event::Key(key) = event else {
            return EventOutcome::Ignored;
        };
        let control = match key.key {
            bmux_keyboard::KeyCode::Char('r') => Control::Run,
            bmux_keyboard::KeyCode::Char('s') => Control::Stop,
            bmux_keyboard::KeyCode::Char('R') => Control::Restart,
            bmux_keyboard::KeyCode::Char('p') => Control::Pause,
            bmux_keyboard::KeyCode::Char('n') => Control::Step,
            bmux_keyboard::KeyCode::Char('c') => Control::Resume,
            _ => return EventOutcome::Ignored,
        };
        match self.submit(control) {
            Ok(()) => EventOutcome::Redraw,
            Err(_) => EventOutcome::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_diagnostic;
    #[test]
    fn diagnostics_are_bounded_utf8_and_not_terminal_commands() {
        let text = format!("\u{1b}[31m{}", "é".repeat(4096));
        let bounded = bounded_diagnostic(&text);
        assert!(!bounded.contains('\u{1b}'));
        assert!(bounded.ends_with("[diagnostic truncated]"));
        assert!(bounded.len() < 4200);
    }
}
