//! Generic protocol tree renderer and event dispatcher.

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_component_protocol::event::{ComponentEvent, ComponentEventKind};
use bmux_tui_component_protocol::ids::ComponentId;
use bmux_tui_component_protocol::model::{
    ButtonRole, ComponentKind, ComponentNode, ComponentTree, PanelChrome, StackDirection,
    StatusLevel,
};
use bmux_tui_component_protocol::state::ComponentRuntimeState;
use bmux_tui_component_protocol::value::ComponentValue;

use crate::button::{Button, ButtonOutcome, ButtonState, ButtonStyles};
use crate::checkbox::{Checkbox, CheckboxOutcome, CheckboxState};
use crate::protocol::convert::radio_options;
use crate::protocol::{ProtocolBindings, ProtocolRuntime};
use crate::radio_group::{RadioGroup, RadioGroupOutcome, RadioGroupState};

/// Renderable view over a protocol component tree.
pub struct ProtocolTree<'a> {
    tree: &'a ComponentTree,
    bindings: Option<&'a ProtocolBindings>,
}

impl<'a> ProtocolTree<'a> {
    /// Create a protocol tree view with extension bindings.
    #[must_use]
    pub const fn new(tree: &'a ComponentTree, bindings: &'a ProtocolBindings) -> Self {
        Self {
            tree,
            bindings: Some(bindings),
        }
    }

    /// Create a protocol tree view with no extension bindings.
    #[must_use]
    pub const fn without_extensions(tree: &'a ComponentTree) -> Self {
        Self {
            tree,
            bindings: None,
        }
    }

    /// Render the tree with host-local component UI state.
    pub fn render_runtime(&self, area: Rect, runtime: &mut ProtocolRuntime, frame: &mut Frame<'_>) {
        ProtocolComponent::new(&self.tree.root, self.bindings).render_runtime(area, runtime, frame);
    }

    /// Render the tree.
    pub fn render(&self, area: Rect, state: &ComponentRuntimeState, frame: &mut Frame<'_>) {
        let mut runtime = ProtocolRuntime::from_state(state.clone());
        self.render_runtime(area, &mut runtime, frame);
    }

    /// Handle one input event for the tree with host-local component UI state.
    pub fn handle_event_runtime(
        &self,
        area: Rect,
        runtime: &mut ProtocolRuntime,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        ProtocolComponent::new(&self.tree.root, self.bindings)
            .handle_event_runtime(area, runtime, event)
    }

    /// Handle one input event for the tree.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut ComponentRuntimeState,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        let mut runtime = ProtocolRuntime::from_state(std::mem::take(state));
        let events = self.handle_event_runtime(area, &mut runtime, event);
        *state = runtime.into_state();
        events
    }
}

/// Renderable view over one protocol component node.
pub struct ProtocolComponent<'a> {
    node: &'a ComponentNode,
    bindings: Option<&'a ProtocolBindings>,
}

impl<'a> ProtocolComponent<'a> {
    /// Create a protocol component view with extension bindings.
    #[must_use]
    pub const fn new(node: &'a ComponentNode, bindings: Option<&'a ProtocolBindings>) -> Self {
        Self { node, bindings }
    }

    /// Render this node with host-local component UI state.
    pub fn render_runtime(&self, area: Rect, runtime: &mut ProtocolRuntime, frame: &mut Frame<'_>) {
        render_node(self.node, self.bindings, area, runtime, frame);
    }

    /// Render this node.
    pub fn render(&self, area: Rect, state: &ComponentRuntimeState, frame: &mut Frame<'_>) {
        let mut runtime = ProtocolRuntime::from_state(state.clone());
        self.render_runtime(area, &mut runtime, frame);
    }

    /// Handle one input event for this node with host-local component UI state.
    pub fn handle_event_runtime(
        &self,
        area: Rect,
        runtime: &mut ProtocolRuntime,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        handle_node_event(self.node, self.bindings, area, runtime, event)
    }

    /// Handle one input event for this node.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut ComponentRuntimeState,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        let mut runtime = ProtocolRuntime::from_state(std::mem::take(state));
        let events = self.handle_event_runtime(area, &mut runtime, event);
        *state = runtime.into_state();
        events
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn render_node(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    frame: &mut Frame<'_>,
) {
    match &node.kind {
        ComponentKind::Text { text, .. } | ComponentKind::Markdown { markdown: text } => {
            frame.write_line(area, &Line::from(text.clone()));
        }
        ComponentKind::Stack { direction, gap } => {
            render_stack(node, bindings, area, runtime, frame, *direction, *gap);
        }
        ComponentKind::Panel { title, chrome } => {
            render_panel(
                node,
                bindings,
                area,
                runtime,
                frame,
                title.as_deref(),
                *chrome,
            );
        }
        ComponentKind::Divider => {
            frame.write_line(area, &Line::from("─".repeat(usize::from(area.width))));
        }
        ComponentKind::Spacer { .. } => {}
        ComponentKind::Button {
            label,
            role,
            disabled,
            ..
        } => {
            let button = Button::new(label).styles(button_styles(*role));
            let mut button_state = ButtonState::new();
            button_state.set_disabled(*disabled);
            button_state.set_focused(is_focused(node, runtime.state()));
            button.render(area, &button_state, frame);
        }
        ComponentKind::TextInput {
            value,
            placeholder,
            disabled,
            ..
        }
        | ComponentKind::TextArea {
            value,
            placeholder,
            disabled,
            ..
        } => render_input_like(
            node,
            runtime.state(),
            area,
            frame,
            value,
            placeholder.as_deref(),
            *disabled,
        ),
        ComponentKind::RadioGroup {
            options,
            selected,
            disabled,
            ..
        } => {
            let options = radio_options(options);
            let selected = selected
                .as_ref()
                .and_then(|selected| options.iter().position(|option| &option.id == selected));
            let mut group_state = RadioGroupState::new(selected);
            group_state.interaction.focused = is_focused(node, runtime.state());
            group_state.interaction.disabled = *disabled;
            RadioGroup::new(&options).render(area, &group_state, frame);
        }
        ComponentKind::CheckboxGroup {
            options, disabled, ..
        } => {
            for (index, option) in options.iter().take(usize::from(area.height)).enumerate() {
                let row = area
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
                let checkbox = Checkbox::new(option.option.label.as_str());
                let mut checkbox_state = CheckboxState::new(option.checked);
                checkbox_state.set_disabled(*disabled || option.option.disabled);
                checkbox_state.set_focused(is_focused(node, runtime.state()) && index == 0);
                checkbox.render(
                    Rect::new(area.x, row, area.width, 1),
                    &checkbox_state,
                    frame,
                );
            }
        }
        ComponentKind::Select {
            options, selected, ..
        } => {
            let label = selected
                .as_ref()
                .and_then(|selected| options.iter().find(|option| &option.id == selected))
                .map_or("<select>", |option| option.label.as_str());
            frame.write_line(area, &Line::from(format!("[ {label} ]")));
        }
        ComponentKind::Form { .. } => {
            render_children_vertical(node, bindings, area, runtime, frame, 0);
        }
        ComponentKind::Status { level, message } => {
            frame.write_line(
                area,
                &Line::from_spans(vec![Span::styled(message, status_style(*level))]),
            );
        }
        ComponentKind::Component { type_id, .. } => {
            if let Some(binding) = bindings.and_then(|bindings| bindings.component(type_id)) {
                let mut context =
                    crate::protocol::ProtocolRenderContext::new(bindings, runtime, frame);
                binding.render_with_context(node, area, &mut context);
            } else {
                frame.write_line(
                    area,
                    &Line::from(format!("unsupported component: {}", type_id.as_str())),
                );
            }
        }
        ComponentKind::Extension { kind, .. } => {
            if let Some(binding) = bindings.and_then(|bindings| bindings.extension(kind)) {
                let mut context =
                    crate::protocol::ProtocolRenderContext::new(bindings, runtime, frame);
                binding.render_with_context(node, area, &mut context);
            } else {
                frame.write_line(area, &Line::from(format!("unsupported component: {kind}")));
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn handle_node_event(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
) -> Vec<ComponentEvent> {
    match &node.kind {
        ComponentKind::Button {
            action,
            label,
            role,
            disabled,
        } => {
            let button = Button::new(label).styles(button_styles(*role));
            let mut button_state = ButtonState::new();
            button_state.set_disabled(*disabled);
            button_state.set_focused(is_focused(node, runtime.state()));
            if matches!(
                button.handle_event(area, &mut button_state, event),
                ButtonOutcome::Pressed
            ) {
                return vec![ComponentEvent::new(
                    node.id.clone(),
                    ComponentEventKind::Action {
                        action: action.clone(),
                    },
                )];
            }
            Vec::new()
        }
        ComponentKind::RadioGroup { options, .. } => {
            let radio_options = radio_options(options);
            let selected = selected_radio_index(node, runtime.state(), &radio_options);
            let mut group_state = RadioGroupState::new(selected);
            group_state.interaction.focused = is_focused(node, runtime.state());
            let outcome =
                RadioGroup::new(&radio_options).handle_event(area, &mut group_state, event);
            if matches!(outcome, RadioGroupOutcome::Selected(_))
                && let Some(selected) = group_state
                    .selected()
                    .and_then(|index| radio_options.get(index))
                    .map(|option| option.id.clone())
            {
                set_node_value(
                    node,
                    runtime.state_mut(),
                    ComponentValue::String(selected.clone()),
                );
                return vec![ComponentEvent::new(
                    node.id.clone(),
                    ComponentEventKind::ValueChanged {
                        value: ComponentValue::String(selected),
                    },
                )];
            }
            Vec::new()
        }
        ComponentKind::CheckboxGroup { options, .. } => {
            let mut events = Vec::new();
            for (index, option) in options.iter().take(usize::from(area.height)).enumerate() {
                let row = area
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
                let option_area = Rect::new(area.x, row, area.width, 1);
                let mut checkbox_state = CheckboxState::new(option.checked);
                checkbox_state.set_focused(is_focused(node, runtime.state()) && index == 0);
                if matches!(
                    Checkbox::new(option.option.label.as_str()).handle_event(
                        option_area,
                        &mut checkbox_state,
                        event,
                    ),
                    CheckboxOutcome::Toggled(_)
                ) {
                    let value = ComponentValue::String(option.option.id.clone());
                    events.push(ComponentEvent::new(
                        node.id.clone(),
                        ComponentEventKind::ValueChanged { value },
                    ));
                }
            }
            events
        }
        ComponentKind::Form { submit, cancel } => match event {
            Event::Key(stroke) if stroke.modifiers.is_empty() => match stroke.key {
                bmux_keyboard::KeyCode::Enter => vec![ComponentEvent::new(
                    node.id.clone(),
                    ComponentEventKind::Action {
                        action: submit.clone(),
                    },
                )],
                bmux_keyboard::KeyCode::Escape => cancel.as_ref().map_or_else(Vec::new, |action| {
                    vec![ComponentEvent::new(
                        node.id.clone(),
                        ComponentEventKind::Action {
                            action: action.clone(),
                        },
                    )]
                }),
                _ => children_events(node, bindings, area, runtime, event),
            },
            _ => children_events(node, bindings, area, runtime, event),
        },
        ComponentKind::Component { type_id, .. } => bindings
            .and_then(|bindings| bindings.component(type_id))
            .map_or_else(Vec::new, |binding| {
                let mut context = crate::protocol::ProtocolEventContext::new(bindings, runtime);
                binding.handle_event_with_context(node, area, event, &mut context)
            }),
        ComponentKind::Extension { kind, .. } => bindings
            .and_then(|bindings| bindings.extension(kind))
            .map_or_else(Vec::new, |binding| {
                let mut context = crate::protocol::ProtocolEventContext::new(bindings, runtime);
                binding.handle_event_with_context(node, area, event, &mut context)
            }),
        _ => children_events(node, bindings, area, runtime, event),
    }
}

fn render_stack(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    frame: &mut Frame<'_>,
    direction: StackDirection,
    gap: u16,
) {
    match direction {
        StackDirection::Vertical => {
            render_children_vertical(node, bindings, area, runtime, frame, gap);
        }
        StackDirection::Horizontal => {
            render_children_horizontal(node, bindings, area, runtime, frame, gap);
        }
    }
}

fn render_panel(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    frame: &mut Frame<'_>,
    title: Option<&str>,
    chrome: PanelChrome,
) {
    if chrome == PanelChrome::Border && area.width > 1 && area.height > 1 {
        let horizontal = "─".repeat(usize::from(area.width.saturating_sub(2)));
        frame.write_line(area, &Line::from(format!("┌{horizontal}┐")));
        frame.write_line(
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            &Line::from(format!("└{horizontal}┘")),
        );
        for y in area.y.saturating_add(1)..area.bottom().saturating_sub(1) {
            frame.write_line(Rect::new(area.x, y, 1, 1), &Line::from("│"));
            frame.write_line(
                Rect::new(area.right().saturating_sub(1), y, 1, 1),
                &Line::from("│"),
            );
        }
        if let Some(title) = title {
            frame.write_line(
                Rect::new(
                    area.x.saturating_add(2),
                    area.y,
                    area.width.saturating_sub(4),
                    1,
                ),
                &Line::from(title),
            );
        }
        render_children_vertical(
            node,
            bindings,
            area.inset(Insets::new(1, 1, 1, 1)),
            runtime,
            frame,
            0,
        );
    } else {
        if let Some(title) = title {
            frame.write_line(area, &Line::from(title));
        }
        render_children_vertical(node, bindings, area, runtime, frame, 0);
    }
}

fn render_children_vertical(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    frame: &mut Frame<'_>,
    gap: u16,
) {
    let mut y = area.y;
    for child in &node.children {
        if y >= area.bottom() {
            break;
        }
        let height = node_height(child).min(area.bottom().saturating_sub(y));
        render_node(
            child,
            bindings,
            Rect::new(area.x, y, area.width, height),
            runtime,
            frame,
        );
        y = y.saturating_add(height).saturating_add(gap);
    }
}

fn render_children_horizontal(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    frame: &mut Frame<'_>,
    gap: u16,
) {
    let count = u16::try_from(node.children.len())
        .unwrap_or(u16::MAX)
        .max(1);
    let width = area
        .width
        .saturating_sub(gap.saturating_mul(count.saturating_sub(1)))
        / count;
    let mut x = area.x;
    for child in &node.children {
        if x >= area.right() {
            break;
        }
        render_node(
            child,
            bindings,
            Rect::new(x, area.y, width, area.height),
            runtime,
            frame,
        );
        x = x.saturating_add(width).saturating_add(gap);
    }
}

fn children_events(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
) -> Vec<ComponentEvent> {
    let mut output = Vec::new();
    let mut y = area.y;
    for child in &node.children {
        let height = node_height(child).min(area.bottom().saturating_sub(y));
        output.extend(handle_node_event(
            child,
            bindings,
            Rect::new(area.x, y, area.width, height),
            runtime,
            event,
        ));
        y = y.saturating_add(height);
    }
    output
}

fn node_height(node: &ComponentNode) -> u16 {
    match &node.kind {
        ComponentKind::Stack {
            direction: StackDirection::Vertical,
            gap,
        } => node
            .children
            .iter()
            .map(node_height)
            .sum::<u16>()
            .saturating_add(gap.saturating_mul(
                u16::try_from(node.children.len().saturating_sub(1)).unwrap_or(u16::MAX),
            )),
        ComponentKind::Panel { chrome, .. } => node
            .children
            .iter()
            .map(node_height)
            .sum::<u16>()
            .saturating_add(if *chrome == PanelChrome::Border { 2 } else { 0 }),
        ComponentKind::RadioGroup { options, .. } => {
            u16::try_from(options.len()).unwrap_or(u16::MAX)
        }
        ComponentKind::CheckboxGroup { options, .. } => {
            u16::try_from(options.len()).unwrap_or(u16::MAX)
        }
        ComponentKind::TextArea { rows, .. } => *rows,
        ComponentKind::Form { .. } => node.children.iter().map(node_height).sum(),
        ComponentKind::Spacer { size } => *size,
        _ => 1,
    }
}

fn render_input_like(
    node: &ComponentNode,
    state: &ComponentRuntimeState,
    area: Rect,
    frame: &mut Frame<'_>,
    value: &str,
    placeholder: Option<&str>,
    disabled: bool,
) {
    let displayed = if value.is_empty() {
        placeholder.unwrap_or("")
    } else {
        value
    };
    let style = if disabled {
        Style::new().add_modifier(Modifier::DIM)
    } else if is_focused(node, state) {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    frame.write_line(
        area,
        &Line::from_spans(vec![Span::styled(format!(" {displayed}"), style)]),
    );
}

fn button_styles(role: ButtonRole) -> ButtonStyles {
    match role {
        ButtonRole::Normal => ButtonStyles::default(),
        ButtonRole::Primary => ButtonStyles {
            normal: Style::new().fg(Color::Green),
            ..ButtonStyles::default()
        },
        ButtonRole::Danger => ButtonStyles {
            normal: Style::new().fg(Color::Red),
            ..ButtonStyles::default()
        },
    }
}

const fn status_style(level: StatusLevel) -> Style {
    match level {
        StatusLevel::Info => Style::new().fg(Color::Cyan),
        StatusLevel::Success => Style::new().fg(Color::Green),
        StatusLevel::Warning => Style::new().fg(Color::Yellow),
        StatusLevel::Error => Style::new().fg(Color::Red),
    }
}

fn is_focused(node: &ComponentNode, state: &ComponentRuntimeState) -> bool {
    node.id
        .as_ref()
        .is_some_and(|id| state.focus.focused.as_ref() == Some(id))
}

fn selected_radio_index(
    node: &ComponentNode,
    state: &ComponentRuntimeState,
    options: &[crate::radio_group::RadioOption],
) -> Option<usize> {
    let id = node.id.as_ref()?;
    let ComponentValue::String(selected) = state.values.get(id)? else {
        return None;
    };
    options.iter().position(|option| &option.id == selected)
}

fn set_node_value(node: &ComponentNode, state: &mut ComponentRuntimeState, value: ComponentValue) {
    if let Some(id) = &node.id {
        state.values.insert(ComponentId::new(id.as_str()), value);
    }
}
