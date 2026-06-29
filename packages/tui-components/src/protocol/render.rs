//! Generic protocol tree renderer and event dispatcher.

use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_component_protocol::event::{ComponentEvent, ComponentEventKind};
use bmux_tui_component_protocol::ids::{ComponentId, ComponentTypeId};
use bmux_tui_component_protocol::model::{
    ButtonRole, CheckboxOption, ComponentKind, ComponentNode, ComponentTree, PanelChrome,
    StackDirection, StatusLevel,
};
use bmux_tui_component_protocol::state::ComponentRuntimeState;
use bmux_tui_component_protocol::value::ComponentValue;

use crate::button::{Button, ButtonOutcome, ButtonState, ButtonStyles};
use crate::checkbox::{Checkbox, CheckboxState};
use crate::protocol::convert::radio_options;
use crate::protocol::{ProtocolBindings, ProtocolLocalStateKey, ProtocolRuntime};
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

    /// Estimate rendered height for this tree in terminal rows.
    #[must_use]
    pub fn measure_height(&self, _width: u16) -> u16 {
        node_height(&self.tree.root)
    }

    /// Render the tree with host-local component UI state.
    pub fn render_runtime(&self, area: Rect, runtime: &mut ProtocolRuntime, frame: &mut Frame<'_>) {
        ensure_initial_focus(&self.tree.root, runtime);
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
        ensure_initial_focus(&self.tree.root, runtime);
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
        ensure_initial_focus(self.node, runtime);
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
        ensure_initial_focus(self.node, runtime);
        handle_node_event_root(self.node, self.bindings, area, runtime, event)
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CheckboxGroupLocalState {
    focused: usize,
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
            let selected_values = checkbox_group_selected(node, runtime.state(), options);
            let focused_index = checkbox_group_local_state(node, runtime).focused;
            for (index, option) in options.iter().take(usize::from(area.height)).enumerate() {
                let row = area
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
                let checkbox = Checkbox::new(option.option.label.as_str());
                let mut checkbox_state = CheckboxState::new(
                    selected_values.as_ref().map_or(option.checked, |selected| {
                        selected.contains(&option.option.id)
                    }),
                );
                checkbox_state.set_disabled(*disabled || option.option.disabled);
                checkbox_state
                    .set_focused(is_focused(node, runtime.state()) && index == focused_index);
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
pub(super) fn handle_node_event_root(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
) -> Vec<ComponentEvent> {
    handle_node_event(node, bindings, area, runtime, event, true)
}

pub(super) fn handle_node_event_child(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
) -> Vec<ComponentEvent> {
    handle_node_event(node, bindings, area, runtime, event, false)
}

#[allow(clippy::too_many_lines)]
fn handle_node_event(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
    allow_focus_traversal: bool,
) -> Vec<ComponentEvent> {
    if allow_focus_traversal && handle_focus_traversal(node, runtime, event) {
        return Vec::new();
    }
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
            let Event::Key(stroke) = event else {
                return Vec::new();
            };
            if stroke.modifiers.ctrl || stroke.modifiers.alt || !is_focused(node, runtime.state()) {
                return Vec::new();
            }
            match stroke.key {
                bmux_keyboard::KeyCode::Up | bmux_keyboard::KeyCode::Left => {
                    move_checkbox_group_focus(node, runtime, options, Direction::Previous);
                    return Vec::new();
                }
                bmux_keyboard::KeyCode::Down | bmux_keyboard::KeyCode::Right => {
                    move_checkbox_group_focus(node, runtime, options, Direction::Next);
                    return Vec::new();
                }
                _ => {}
            }
            let Some(target_index) =
                checkbox_group_target_index(node, runtime, options, stroke.key)
            else {
                return Vec::new();
            };
            let Some(target) = options.get(target_index) else {
                return Vec::new();
            };
            if target.option.disabled {
                return Vec::new();
            }
            let mut selected = checkbox_group_selected(node, runtime.state(), options)
                .unwrap_or_else(|| checkbox_group_initial_selected(options));
            if !selected.insert(target.option.id.clone()) {
                selected.remove(&target.option.id);
            }
            let value = ComponentValue::List(
                selected
                    .iter()
                    .cloned()
                    .map(ComponentValue::String)
                    .collect(),
            );
            set_node_value(node, runtime.state_mut(), value.clone());
            vec![ComponentEvent::new(
                node.id.clone(),
                ComponentEventKind::ValueChanged { value },
            )]
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

fn handle_focus_traversal(
    root: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    event: &Event,
) -> bool {
    let Event::Key(stroke) = event else {
        return false;
    };
    if stroke.key != bmux_keyboard::KeyCode::Tab || stroke.modifiers.ctrl || stroke.modifiers.alt {
        return false;
    }
    let mut order = Vec::new();
    collect_focusable(root, &mut order);
    runtime.state_mut().focus.traversal_order.clone_from(&order);
    if order.is_empty() {
        runtime.state_mut().focus.focused = None;
        return true;
    }
    let current = runtime.state().focus.focused.as_ref();
    let current_index = current.and_then(|focused| order.iter().position(|id| id == focused));
    let next_index = if stroke.modifiers.shift {
        current_index.map_or_else(
            || order.len().saturating_sub(1),
            |index| {
                if index == 0 {
                    order.len().saturating_sub(1)
                } else {
                    index.saturating_sub(1)
                }
            },
        )
    } else {
        current_index.map_or(0, |index| (index + 1) % order.len())
    };
    runtime.state_mut().focus.focused = order.get(next_index).cloned();
    true
}

fn ensure_initial_focus(root: &ComponentNode, runtime: &mut ProtocolRuntime) {
    if runtime.state().focus.focused.is_some() {
        return;
    }
    let mut order = Vec::new();
    collect_focusable(root, &mut order);
    runtime.state_mut().focus.traversal_order.clone_from(&order);
    runtime.state_mut().focus.focused = order.first().cloned();
}

fn collect_focusable(node: &ComponentNode, output: &mut Vec<ComponentId>) {
    if let Some(id) = &node.id
        && is_focusable(node)
    {
        output.push(id.clone());
    }
    for child in &node.children {
        collect_focusable(child, output);
    }
}

fn is_focusable(node: &ComponentNode) -> bool {
    match &node.kind {
        ComponentKind::Select { disabled, .. }
        | ComponentKind::Button { disabled, .. }
        | ComponentKind::TextInput { disabled, .. }
        | ComponentKind::TextArea { disabled, .. }
        | ComponentKind::RadioGroup { disabled, .. }
        | ComponentKind::CheckboxGroup { disabled, .. } => !disabled,
        ComponentKind::Component { type_id, props } => {
            matches!(
                type_id.as_str(),
                "bmux.action_row"
                    | "bmux.button"
                    | "bmux.checkbox"
                    | "bmux.radio_group"
                    | "bmux.select_dropdown"
                    | "bmux.text_input"
                    | "bmux.text_input_box"
            ) && !component_disabled(props)
        }
        ComponentKind::Extension { kind, payload } => {
            matches!(
                kind.as_str(),
                "bmux.action_row"
                    | "bmux.button"
                    | "bmux.checkbox"
                    | "bmux.radio_group"
                    | "bmux.select_dropdown"
                    | "bmux.text_input"
                    | "bmux.text_input_box"
            ) && !component_disabled(payload)
        }
        ComponentKind::Text { .. }
        | ComponentKind::Markdown { .. }
        | ComponentKind::Stack { .. }
        | ComponentKind::Panel { .. }
        | ComponentKind::Divider
        | ComponentKind::Spacer { .. }
        | ComponentKind::Form { .. }
        | ComponentKind::Status { .. } => false,
    }
}

fn component_disabled(props: &ComponentValue) -> bool {
    props
        .as_map()
        .and_then(|map| map.get("disabled"))
        .and_then(|value| match value {
            ComponentValue::Bool(value) => Some(*value),
            ComponentValue::Null
            | ComponentValue::I64(_)
            | ComponentValue::U64(_)
            | ComponentValue::F64(_)
            | ComponentValue::String(_)
            | ComponentValue::List(_)
            | ComponentValue::Map(_) => None,
        })
        .unwrap_or(false)
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
    match &node.kind {
        ComponentKind::Stack {
            direction: StackDirection::Vertical,
            gap,
        } => children_events_vertical(node, bindings, area, runtime, event, *gap),
        ComponentKind::Stack {
            direction: StackDirection::Horizontal,
            gap,
        } => children_events_horizontal(node, bindings, area, runtime, event, *gap),
        ComponentKind::Panel { chrome, .. } if *chrome == PanelChrome::Border => {
            children_events_vertical(
                node,
                bindings,
                area.inset(Insets::new(1, 1, 1, 1)),
                runtime,
                event,
                0,
            )
        }
        _ => children_events_vertical(node, bindings, area, runtime, event, 0),
    }
}

fn children_events_vertical(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
    gap: u16,
) -> Vec<ComponentEvent> {
    let mut output = Vec::new();
    let mut y = area.y;
    for child in &node.children {
        if y >= area.bottom() {
            break;
        }
        let height = node_height(child).min(area.bottom().saturating_sub(y));
        output.extend(handle_node_event(
            child,
            bindings,
            Rect::new(area.x, y, area.width, height),
            runtime,
            event,
            false,
        ));
        y = y.saturating_add(height).saturating_add(gap);
    }
    output
}

fn children_events_horizontal(
    node: &ComponentNode,
    bindings: Option<&ProtocolBindings>,
    area: Rect,
    runtime: &mut ProtocolRuntime,
    event: &Event,
    gap: u16,
) -> Vec<ComponentEvent> {
    let count = u16::try_from(node.children.len())
        .unwrap_or(u16::MAX)
        .max(1);
    let width = area
        .width
        .saturating_sub(gap.saturating_mul(count.saturating_sub(1)))
        / count;
    let mut output = Vec::new();
    let mut x = area.x;
    for child in &node.children {
        if x >= area.right() {
            break;
        }
        output.extend(handle_node_event(
            child,
            bindings,
            Rect::new(x, area.y, width, area.height),
            runtime,
            event,
            false,
        ));
        x = x.saturating_add(width).saturating_add(gap);
    }
    output
}

fn component_node_height(type_id: &ComponentTypeId, node: &ComponentNode) -> u16 {
    match type_id.as_str() {
        "bmux.text_input_box" => 3,
        "bmux.radio_group" | "bmux.checkbox_group" => match &node.kind {
            ComponentKind::Component { props, .. }
            | ComponentKind::Extension { payload: props, .. } => props
                .as_map()
                .and_then(|props| props.get("options"))
                .and_then(|value| match value {
                    ComponentValue::List(values) => Some(values.as_slice()),
                    _ => None,
                })
                .map_or(1, |options| {
                    u16::try_from(options.len()).unwrap_or(u16::MAX)
                }),
            _ => 1,
        },
        _ => node.children.iter().map(node_height).sum::<u16>().max(1),
    }
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
        ComponentKind::Component { type_id, .. } => component_node_height(type_id, node),
        ComponentKind::Extension { kind, .. } => {
            component_node_height(&ComponentTypeId::new(kind.clone()), node)
        }
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

fn checkbox_group_selected(
    node: &ComponentNode,
    state: &ComponentRuntimeState,
    options: &[CheckboxOption],
) -> Option<std::collections::BTreeSet<String>> {
    let id = node.id.as_ref()?;
    let ComponentValue::List(selected) = state.values.get(id)? else {
        return None;
    };
    let allowed = options
        .iter()
        .map(|option| option.option.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    Some(
        selected
            .iter()
            .filter_map(|value| match value {
                ComponentValue::String(value) if allowed.contains(value.as_str()) => {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect(),
    )
}

fn checkbox_group_initial_selected(
    options: &[CheckboxOption],
) -> std::collections::BTreeSet<String> {
    options
        .iter()
        .filter(|option| option.checked)
        .map(|option| option.option.id.clone())
        .collect()
}

fn checkbox_group_target_index(
    node: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    options: &[CheckboxOption],
    key: bmux_keyboard::KeyCode,
) -> Option<usize> {
    match key {
        bmux_keyboard::KeyCode::Char(' ') | bmux_keyboard::KeyCode::Enter => {
            let focused = checkbox_group_local_state(node, runtime).focused;
            (focused < options.len())
                .then_some(focused)
                .or_else(|| options.iter().position(|option| !option.option.disabled))
        }
        bmux_keyboard::KeyCode::Char(value) if value.is_ascii_digit() => value
            .to_digit(10)
            .and_then(|digit| usize::try_from(digit).ok())
            .and_then(|digit| digit.checked_sub(1))
            .filter(|index| *index < options.len()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Previous,
    Next,
}

fn move_checkbox_group_focus(
    node: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    options: &[CheckboxOption],
    direction: Direction,
) {
    if options.is_empty() {
        checkbox_group_local_state(node, runtime).focused = 0;
        return;
    }
    let current = checkbox_group_local_state(node, runtime)
        .focused
        .min(options.len().saturating_sub(1));
    let next = next_enabled_checkbox_index(options, current, direction).unwrap_or(current);
    checkbox_group_local_state(node, runtime).focused = next;
}

fn next_enabled_checkbox_index(
    options: &[CheckboxOption],
    current: usize,
    direction: Direction,
) -> Option<usize> {
    if options.iter().all(|option| option.option.disabled) {
        return None;
    }
    let mut index = current;
    for _ in 0..options.len() {
        index = match direction {
            Direction::Previous => {
                if index == 0 {
                    options.len().saturating_sub(1)
                } else {
                    index.saturating_sub(1)
                }
            }
            Direction::Next => (index + 1) % options.len(),
        };
        if !options[index].option.disabled {
            return Some(index);
        }
    }
    None
}

fn checkbox_group_local_state<'a>(
    node: &ComponentNode,
    runtime: &'a mut ProtocolRuntime,
) -> &'a mut CheckboxGroupLocalState {
    let key = ProtocolLocalStateKey::new(
        node.id
            .clone()
            .unwrap_or_else(|| ComponentId::new("anonymous-checkbox-group")),
        ComponentTypeId::new("bmux.checkbox_group"),
    );
    runtime.local_state_or_insert_with(&key, CheckboxGroupLocalState::default)
}

fn set_node_value(node: &ComponentNode, state: &mut ComponentRuntimeState, value: ComponentValue) {
    if let Some(id) = &node.id {
        state.values.insert(ComponentId::new(id.as_str()), value);
    }
}
