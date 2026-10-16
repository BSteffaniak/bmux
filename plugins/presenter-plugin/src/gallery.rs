//! Native, bounded examples for the tour. No source evaluation or terminal subprocesses.
use std::cell::{Cell, RefCell};

use bmux_presenter::custom::{LinkedComponentFactory, LinkedState};
use bmux_presenter::{Block, BlockKind};
use bmux_tui::prelude::{
    Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome, LayoutCx,
    LayoutNode, Line, PaintCx, Text, TextBlock,
};
use bmux_tui_components::{
    action_row::{ActionButton, ActionRowState},
    badge::BadgeComponent,
    bar_chart::{BarChartComponent, BarChartItem},
    breadcrumbs::{BreadcrumbItem, BreadcrumbsComponent, BreadcrumbsState},
    button::{ButtonComponent, ButtonState},
    canvas::{Canvas, CanvasBounds, CanvasPoint},
    chart::{Chart, ChartBounds, ChartDataset, ChartPoint},
    checkbox::{CheckboxComponent, CheckboxState},
    dialog::{Dialog, DialogComponent},
    diff_viewer::{DiffViewerInput, DiffViewerLayout, diff_viewer_rows},
    form::{Form, FormFieldItem},
    form_field::FormFieldComponent,
    modal_frame::ModalTheme,
    progress_bar::{ProgressBarComponent, ProgressBarValue},
    radio_group::{RadioGroupComponent, RadioGroupState, RadioOption},
    scroll_view::{ScrollViewComponent, ScrollViewState},
    select_dropdown::{SelectDropdownComponent, SelectDropdownState, SelectOption},
    selectable_list::{SelectableListComponent, SelectableListItem, SelectableListState},
    source_viewer::{SourceViewerInput, source_viewer_rows},
    sparkline::SparklineComponent,
    tab_bar::{TabBarComponent, TabBarState, TabItem},
    table::{TableColumn, TableComponent, TableRow, TableState},
    terminal_viewer::{TerminalViewerInput, TerminalViewerSizing, terminal_viewer_rows},
    text_input::{TextInputPolicy, TextInputState},
    text_input_box::{TextInputBoxComponent, TextInputBoxPolicy},
    toast_stack::{ToastItem, ToastStackComponent, ToastStackState},
    tree_view::{TreeViewComponent, TreeViewItem, TreeViewState},
    virtual_list::{VirtualList, VirtualListState},
};

pub(crate) const KINDS: &[&str] = &[
    "button",
    "checkbox",
    "radio-group",
    "text-input-box",
    "select-dropdown",
    "dialog",
    "selectable-list",
    "tree-view",
    "table",
    "scroll-view",
    "bar-chart",
    "sparkline",
    "progress-bar",
    "badge",
    "tab-bar",
    "breadcrumbs",
    "toast-stack",
    "virtual-list",
    "source-viewer",
    "diff-viewer",
    "terminal-viewer",
    "canvas",
    "chart",
    "form",
];

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    kind: String,
}

fn kind(block: &Block) -> Result<String, String> {
    let BlockKind::Custom { config, .. } = &block.kind else {
        return Err("tui-gallery requires a custom block".into());
    };
    let config: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    if !KINDS.contains(&config.kind.as_str()) {
        return Err(format!("unknown TUI gallery kind: {}", config.kind));
    }
    Ok(config.kind)
}

pub(crate) struct GalleryFactory;
impl LinkedComponentFactory for GalleryFactory {
    fn state_version(&self) -> u64 {
        1
    }
    fn initial_state(&self, block: &Block) -> Result<serde_json::Value, String> {
        kind(block)?;
        Ok(serde_json::json!({"selected": 0, "checked": false}))
    }
    fn build<'a>(&'a self, block: &'a Block, state: LinkedState) -> Result<Element<'a>, String> {
        Ok(Element::new(Gallery {
            id: block.id.as_str().into(),
            kind: kind(block)?,
            state,
        }))
    }
}

struct Gallery {
    id: String,
    kind: String,
    state: LinkedState,
}

impl Gallery {
    // Every pass rebuilds a borrowed control from bounded canonical state. Only an
    // event pass commits changes; layout and paint cannot mutate owner state.
    fn visit<R>(&self, width: u16, commit: bool, f: impl FnOnce(&dyn Component) -> R) -> R {
        let saved = self.state.lock().expect("gallery state poisoned").clone();
        let selected = saved["selected"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0)
            .min(2);
        macro_rules! show {
            ($control:expr) => {
                f(&$control)
            };
        }
        macro_rules! selection {
            ($cell:ident, $control:expr, $getter:ident) => {{
                let result = f(&$control);
                if commit {
                    self.state.lock().expect("gallery state poisoned")["selected"] =
                        serde_json::json!($cell.get().$getter());
                }
                result
            }};
        }
        match self.kind.as_str() {
            "button" => {
                let mut value = ButtonState::new();
                value.set_focused(true);
                let state = Cell::new(value);
                show!(
                    bmux_tui::composition::Column::new()
                        .id(self.id.clone())
                        .child(ButtonComponent::new(
                            format!("{}.save", self.id),
                            "Save",
                            &state
                        ))
                        .child(TextBlock::new(format!(
                            "Activations: {}",
                            saved["activations"].as_u64().unwrap_or(0)
                        )))
                )
            }
            "text-input-box" => {
                let buffer = saved
                    .get("buffer")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<bmux_text_edit::TextEditBuffer>(v).ok())
                    .unwrap_or_else(|| bmux_text_edit::TextEditBuffer::from_text("Ada"));
                let state = RefCell::new(TextInputState::new(buffer));
                let control = TextInputBoxComponent::new(
                    self.id.clone(),
                    TextInputPolicy::chat_composer(),
                    &state,
                )
                .policy(TextInputBoxPolicy::field().focused(true));
                let result = f(&control);
                if commit && state.borrow().buffer().text().len() <= 4096 {
                    self.state.lock().expect("gallery state poisoned")["buffer"] =
                        serde_json::to_value(state.borrow().buffer())
                            .expect("text buffer serializes");
                }
                result
            }
            "virtual-list" => {
                let mut list = VirtualList::new(self.id.clone());
                for index in 0..40_u64 {
                    list = list.component(index, TextBlock::new(format!("Job {index:02} — ready")));
                }
                let mut state = VirtualListState::new(0);
                list.sync(u64::from(width), &mut state, &mut LayoutCx::default());
                show!(VirtualPreview { list, state })
            }
            "checkbox" => {
                let mut value = CheckboxState::new(saved["checked"].as_bool().unwrap_or(false));
                value.set_focused(true);
                let state = Cell::new(value);
                let result = f(&CheckboxComponent::new(self.id.clone(), "Enable", &state));
                if commit {
                    self.state.lock().expect("gallery state poisoned")["checked"] =
                        serde_json::json!(state.get().checked());
                }
                result
            }
            "breadcrumbs" => {
                let items = [
                    BreadcrumbItem::new("home", "Home"),
                    BreadcrumbItem::new("projects", "Projects"),
                    BreadcrumbItem::new("bmux", "BMUX"),
                ];
                let mut value = BreadcrumbsState::new(Some(selected));
                value.set_focused(true);
                let state = Cell::new(value);
                selection!(
                    state,
                    BreadcrumbsComponent::new(self.id.clone(), &items, &state),
                    current
                )
            }
            "radio-group" => {
                let options = [
                    RadioOption::new("live", "Live"),
                    RadioOption::new("frozen", "Frozen"),
                    RadioOption::new("auto", "Automatic"),
                ];
                let mut value = RadioGroupState::new(Some(selected));
                value.set_focused(
                    saved["focused"]
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .map(|n| n.min(2))
                        .or(Some(selected)),
                );
                value.interaction.focused = true;
                let state = Cell::new(value);
                let result = f(&RadioGroupComponent::new(self.id.clone(), &options, &state));
                if commit {
                    let value = state.get();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected());
                    saved["focused"] = serde_json::json!(value.focused());
                }
                result
            }
            "select-dropdown" => {
                let options = [
                    SelectOption::new("ready", "Ready"),
                    SelectOption::new("running", "Running"),
                    SelectOption::new("done", "Done"),
                ];
                let mut value = SelectDropdownState::new(Some(selected));
                value.interaction.focused = true;
                value.set_open(true);
                let state = Cell::new(value);
                let result = f(&SelectDropdownComponent::new(
                    self.id.clone(),
                    &options,
                    &state,
                ));
                if commit {
                    let value = state.get();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected());
                    saved["open"] = serde_json::json!(value.is_open());
                }
                result
            }
            "selectable-list" => {
                let items = [
                    SelectableListItem::new("build", "Build"),
                    SelectableListItem::new("test", "Test"),
                    SelectableListItem::new("ship", "Ship"),
                ];
                let mut value = SelectableListState::new(Some(selected));
                value.set_focused(
                    saved["focused"]
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .map(|n| n.min(2))
                        .or(Some(selected)),
                );
                value.interaction.focused = true;
                let state = Cell::new(value);
                let result = f(&SelectableListComponent::new(
                    self.id.clone(),
                    &items,
                    &state,
                ));
                if commit {
                    let value = state.get();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected());
                    saved["focused"] = serde_json::json!(value.focused());
                }
                result
            }
            "tab-bar" => {
                let items = [
                    TabItem::new("overview", "Overview"),
                    TabItem::new("activity", "Activity"),
                    TabItem::new("settings", "Settings"),
                ];
                let mut value = TabBarState::new(Some(selected));
                value.set_focused(true);
                let state = RefCell::new(value);
                let result = f(&TabBarComponent::new(self.id.clone(), &items, &state));
                if commit {
                    self.state.lock().expect("gallery state poisoned")["selected"] =
                        serde_json::json!(state.borrow().selected());
                }
                result
            }
            "tree-view" => {
                let items = [
                    TreeViewItem::new("src", "src", 0).expandable(true),
                    TreeViewItem::new("main", "main.rs", 1),
                    TreeViewItem::new("lib", "lib.rs", 1),
                ];
                let mut value = TreeViewState::new(Some(selected));
                value.set_focused(true);
                value.set_expanded("src", saved["expanded"].as_bool().unwrap_or(true));
                let state = RefCell::new(value);
                let result = f(&TreeViewComponent::new(self.id.clone(), &items, &state));
                if commit {
                    let value = state.borrow();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected_visible());
                    saved["expanded"] = serde_json::json!(value.is_expanded("src"));
                }
                result
            }
            "table" => {
                let columns = [
                    TableColumn::new("Job").fixed(16),
                    TableColumn::new("Status").fixed(16),
                ];
                let rows = [
                    TableRow::new(vec!["build", "passed"]),
                    TableRow::new(vec!["test", "running"]),
                    TableRow::new(vec!["release", "waiting"]),
                ];
                let mut value = TableState::new(Some(selected));
                value.interaction.focused = true;
                let state = RefCell::new(value);
                let result = f(&TableComponent::new(
                    self.id.clone(),
                    &columns,
                    &rows,
                    &state,
                ));
                if commit {
                    self.state.lock().expect("gallery state poisoned")["selected"] =
                        serde_json::json!(state.borrow().selected());
                }
                result
            }
            "dialog" => {
                let body = [Line::raw("Save changes before leaving?")];
                let actions = [
                    ActionButton::new("save", "Save"),
                    ActionButton::new("cancel", "Cancel"),
                ];
                let state = Cell::new(ActionRowState::new());
                let dialog = Dialog::new(
                    &body,
                    &actions,
                    ModalTheme::dark(bmux_tui::prelude::Color::Cyan),
                )
                .title("Unsaved changes");
                show!(
                    bmux_tui::composition::SizeBox::new(DialogComponent::new(
                        self.id.clone(),
                        dialog,
                        &state,
                    ))
                    .height(6)
                )
            }
            "bar-chart" => {
                let items = [
                    BarChartItem::new("build", 8),
                    BarChartItem::new("test", 5),
                    BarChartItem::new("lint", 3),
                ];
                show!(BarChartComponent::new(self.id.clone(), &items))
            }
            "sparkline" => show!(SparklineComponent::new(
                self.id.clone(),
                &[2, 4, 3, 7, 5, 9, 6, 8, 4, 6, 8, 10]
            )),
            "progress-bar" => show!(ProgressBarComponent::new(
                self.id.clone(),
                ProgressBarValue::determinate(7, 10)
            )),
            "badge" => show!(BadgeComponent::new(self.id.clone(), "Running")),
            "toast-stack" => {
                let items = [ToastItem::new("saved", "Saved").body("Your changes are ready.")];
                let state = Cell::new(ToastStackState::default());
                show!(ToastStackComponent::new(self.id.clone(), &items, &state))
            }
            "canvas" => {
                let points = [
                    CanvasPoint::new(0.1, 0.2, "•"),
                    CanvasPoint::new(0.3, 0.6, "•"),
                    CanvasPoint::new(0.5, 0.8, "•"),
                    CanvasPoint::new(0.7, 0.5, "•"),
                    CanvasPoint::new(0.9, 0.3, "•"),
                ];
                show!(
                    bmux_tui::composition::SizeBox::new(Canvas::new(
                        &points,
                        CanvasBounds::new(0.0, 1.0, 0.0, 1.0)
                    ))
                    .height(6)
                )
            }
            "chart" => {
                let points = [
                    ChartPoint::new(0.0, 1.0),
                    ChartPoint::new(1.0, 3.0),
                    ChartPoint::new(2.0, 2.0),
                    ChartPoint::new(3.0, 5.0),
                ];
                let datasets = [ChartDataset::line("builds", &points)];
                show!(
                    bmux_tui::composition::SizeBox::new(Chart::new(
                        &datasets,
                        ChartBounds::new(0.0, 3.0, 0.0, 6.0)
                    ))
                    .height(6)
                )
            }
            "source-viewer" => {
                let rows = source_viewer_rows(
                    SourceViewerInput {
                        label: "hello.rs",
                        styled_lines: None,
                        contents: "fn main() {\n    println!(\"Hello, BMUX!\");\n}",
                        start_line: 1,
                        max_lines: 5,
                        truncated_message: "...",
                        line_numbers: true,
                    },
                    width,
                );
                show!(TextBlock::new(Text::from_lines(rows)).id(self.id.clone()))
            }
            "diff-viewer" => {
                let rows = diff_viewer_rows(
                    DiffViewerInput {
                        label: "config.toml",
                        old_text: "theme = \"dark\"\n",
                        new_text: "theme = \"pong\"\n",
                        old_start_line: 1,
                        new_start_line: 1,
                        line_numbers_known: true,
                        title: "Theme change",
                        subtitle: None,
                        argument_bytes: None,
                        truncated: false,
                        layout: DiffViewerLayout::Unified,
                    },
                    width,
                );
                show!(TextBlock::new(Text::from_lines(rows)).id(self.id.clone()))
            }
            "terminal-viewer" => {
                let rows = terminal_viewer_rows(
                    TerminalViewerInput {
                        output: "$ cargo check\n\u{1b}[32mFinished\u{1b}[0m dev profile\n",
                        columns: width,
                        rows: 4,
                        exit_code: Some(0),
                        timed_out: Some(false),
                        elapsed: Some("0.4s"),
                        show_status: true,
                        output_truncated: false,
                        output_bytes: None,
                        retained_output_bytes: None,
                        sizing: TerminalViewerSizing::Compact,
                    },
                    width,
                );
                show!(TextBlock::new(Text::from_lines(rows)).id(self.id.clone()))
            }
            "scroll-view" => show!(ScrollViewComponent::new(
                self.id.clone(),
                bmux_tui::component::LogicalSize::new(u64::from(width), 4),
                ScrollViewState::new(),
                TextBlock::new("one\ntwo\nthree\nfour\nfive\nsix\nseven")
            )),
            "form" => {
                let fields = [
                    FormFieldItem::new("name").required(true),
                    FormFieldItem::new("email").required(true),
                ];
                let values = [Some("Braden"), None];
                let form = Form::new(&fields, &values);
                let invalid = form.validate_ids();
                let name = RefCell::new(TextInputState::new(
                    bmux_text_edit::TextEditBuffer::from_text("Braden"),
                ));
                let email = RefCell::new(TextInputState::new(
                    bmux_text_edit::TextEditBuffer::from_text(""),
                ));
                let name_control = TextInputBoxComponent::new(
                    "gallery.form.name.input",
                    TextInputPolicy::chat_composer(),
                    &name,
                )
                .policy(TextInputBoxPolicy::field());
                let email_control = TextInputBoxComponent::new(
                    "gallery.form.email.input",
                    TextInputPolicy::chat_composer(),
                    &email,
                )
                .policy(TextInputBoxPolicy::field());
                let email_field =
                    FormFieldComponent::new("gallery.form.email", "Email", email_control)
                        .required(true);
                let email_field = if invalid.contains(&"email") {
                    email_field.error("Email is required")
                } else {
                    email_field
                };
                show!(
                    bmux_tui::composition::Column::new()
                        .id(self.id.clone())
                        .child(
                            FormFieldComponent::new("gallery.form.name", "Name", name_control)
                                .required(true)
                        )
                        .child(email_field)
                )
            }
            _ => unreachable!("factory validated the gallery kind"),
        }
    }
}

impl Component for Gallery {
    fn revision(&self) -> ComponentRevision {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.kind.hash(&mut hash);
        self.state
            .lock()
            .expect("gallery state poisoned")
            .to_string()
            .hash(&mut hash);
        ComponentRevision::new(hash.finish(), hash.finish())
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.visit(
            u16::try_from(constraints.max_width()).unwrap_or(u16::MAX),
            false,
            |control| control.layout(constraints, cx),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.visit(
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            false,
            |control| control.paint(layout, cx),
        );
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        // These examples intentionally offer keyboard/paste interaction only. Native
        // mouse press/drag state is not serialized by this small fixture adapter.
        if !matches!(event, Event::Key(_) | Event::Paste(_))
            || !matches!(
                self.kind.as_str(),
                "button"
                    | "text-input-box"
                    | "checkbox"
                    | "breadcrumbs"
                    | "radio-group"
                    | "selectable-list"
                    | "tab-bar"
                    | "tree-view"
                    | "table"
            )
        {
            return EventOutcome::Ignored;
        }
        if matches!(event, Event::Paste(text) if text.len() > 4096) {
            return EventOutcome::Handled;
        }
        let outcome = self.visit(
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            true,
            |control| control.event(event, layout, cx),
        );
        if self.kind == "button"
            && outcome == EventOutcome::Redraw
            && matches!(event, Event::Key(key) if matches!(key.key, bmux_keyboard::KeyCode::Enter | bmux_keyboard::KeyCode::Char(' ')))
        {
            let mut saved = self.state.lock().expect("gallery state poisoned");
            saved["activations"] =
                serde_json::json!(saved["activations"].as_u64().unwrap_or(0).saturating_add(1));
        }
        outcome
    }
}

struct VirtualPreview<'a> {
    list: VirtualList<'a, u64>,
    state: VirtualListState<u64>,
}
impl Component for VirtualPreview<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            "gallery.virtual".into(),
            constraints.constrain(bmux_tui::component::LogicalSize::new(
                constraints.max_width(),
                5,
            )),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.list.paint(
            bmux_tui::geometry::Rect::new(
                0,
                0,
                u16::try_from(layout.size.width).unwrap_or(u16::MAX),
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            ),
            &self.state,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::prelude::{Buffer, Frame, Rect};
    use std::sync::{Arc, Mutex};

    fn block(kind: &str) -> Block {
        Block {
            id: format!("gallery-{kind}").into(),
            reveal_step: 0,
            source: None,
            kind: BlockKind::Custom {
                name: "tui-gallery".into(),
                config: serde_json::json!({"kind": kind}),
            },
        }
    }
    fn example(kind: &str) -> Gallery {
        let block = block(kind);
        Gallery {
            id: block.id.as_str().into(),
            kind: kind.into(),
            state: Arc::new(Mutex::new(GalleryFactory.initial_state(&block).unwrap())),
        }
    }
    fn paint(component: &dyn Component, width: u16) -> String {
        let layout = component.layout(
            Constraints::new(0, u64::from(width), 0, Some(10)),
            &mut LayoutCx::default(),
        );
        assert!(layout.size.height > 0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 10));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        (0..10)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn key(component: &dyn Component, code: KeyCode) {
        let layout = component.layout(
            Constraints::new(0, 60, 0, Some(10)),
            &mut LayoutCx::default(),
        );
        component.event(
            &Event::Key(KeyStroke::simple(code)),
            &layout,
            &mut EventCx::new(&layout),
        );
    }
    #[test]
    fn gallery_all_controls_measure_paint_and_keep_state_read_only() {
        for kind in KINDS {
            let example = example(kind);
            let before = example.state.lock().unwrap().clone();
            for width in [28, 60, 100] {
                assert!(
                    !paint(&example, width).trim().is_empty(),
                    "{kind} at {width}"
                );
            }
            assert_eq!(*example.state.lock().unwrap(), before, "{kind}");
        }
    }
    #[test]
    fn gallery_native_fixtures_are_visible() {
        for (kind, text) in [
            ("button", "Save"),
            ("breadcrumbs", "Projects"),
            ("checkbox", "Enable"),
            ("table", "passed"),
            ("source-viewer", "println!"),
            ("diff-viewer", "pong"),
            ("terminal-viewer", "Finished"),
            ("form", "Email is required"),
            ("virtual-list", "Job 00"),
        ] {
            assert!(paint(&example(kind), 80).contains(text), "{kind}: {text}");
        }
    }
    #[test]
    fn gallery_keyboard_changes_survive_component_rebuilds() {
        let checkbox = example("checkbox");
        key(&checkbox, KeyCode::Char(' '));
        assert_eq!(checkbox.state.lock().unwrap()["checked"], true);
        let breadcrumbs = example("breadcrumbs");
        key(&breadcrumbs, KeyCode::Right);
        assert_eq!(breadcrumbs.state.lock().unwrap()["selected"], 1);
        let radio = example("radio-group");
        key(&radio, KeyCode::Down);
        key(&radio, KeyCode::Char(' '));
        assert_eq!(radio.state.lock().unwrap()["selected"], 1);
        let list = example("selectable-list");
        key(&list, KeyCode::Down);
        key(&list, KeyCode::Enter);
        assert_eq!(list.state.lock().unwrap()["selected"], 1);
        let input = example("text-input-box");
        key(&input, KeyCode::Char('x'));
        let rebuilt = Gallery {
            id: input.id.clone(),
            kind: input.kind.clone(),
            state: Arc::clone(&input.state),
        };
        assert!(paint(&rebuilt, 60).contains('x'));
        let button = example("button");
        key(&button, KeyCode::Enter);
        assert!(paint(&button, 60).contains("Activations: 1"));
    }
    #[test]
    fn gallery_real_tour_slides_scroll_and_render_native_components() {
        let deck = bmux_presenter::parse_markdown(
            "tour.md",
            include_str!("../../../presentations/bmux-tour/deck.md"),
        )
        .unwrap();
        let theme = bmux_presenter::PresentationTheme::dark();
        let mut count = 0;
        for slide in &deck.slides {
            let Some(block) = slide.blocks.iter().find(|block| matches!(&block.kind, BlockKind::Custom { name, .. } if name == "tui-gallery")) else { continue; };
            count += 1;
            let mut registry = bmux_presenter::custom::CustomRegistry::new();
            registry
                .register(
                    "tui-gallery",
                    bmux_presenter::custom::OwnedFactory::new(|block: &Block| {
                        Ok(Gallery {
                            id: block.id.as_str().into(),
                            kind: kind(block)?,
                            state: Arc::new(Mutex::new(GalleryFactory.initial_state(block)?)),
                        })
                    }),
                )
                .unwrap();
            let component = bmux_presenter::SlideComponent::new(slide, 0, &theme, &registry);
            let layout = component.layout(Constraints::for_width(80), &mut LayoutCx::default());
            let area = layout
                .find_rect(
                    &format!("presentation/{}/{}", slide.id.as_str(), block.id.as_str()).into(),
                )
                .expect("gallery placement");
            // Closed overlays may have zero height; opening them is covered by
            // the keyboard/state tests. Only visible content needs scrolling.

            // Long walkthroughs intentionally overflow. Exercise the same viewer
            // and committed geometry used by the player instead of requiring fit.
            let navigation = bmux_presenter::PresentationState {
                slide_id: Some(slide.id.clone()),
                reveal_step: 0,
            };
            let state = bmux_presenter::ViewerState::new();
            let viewer =
                bmux_presenter::ViewerComponent::new(&deck, &navigation, &state, &theme, &registry);
            let _ = paint(&viewer, 80);
            let layout = viewer.layout(
                Constraints::new(0, 80, 0, Some(10)),
                &mut LayoutCx::default(),
            );
            let _ = viewer.event(
                &Event::Key(KeyStroke::simple(KeyCode::End)),
                &layout,
                &mut EventCx::new(&layout),
            );
            // End verifies the overflow route, but the gallery may precede a
            // long walkthrough. Reveal its actual committed position rather than
            // incorrectly requiring the example to be visible at the slide end.
            let mut scroll = state.scroll.get();
            scroll.set_vertical_offset(u64::from(area.y));
            state.scroll.set(scroll);
            let output = paint(&viewer, 80);
            if u32::from(area.y) + u32::from(area.height) > 9 {
                assert!(
                    state.scroll.get().vertical_offset() > 0,
                    "{}",
                    slide.id.as_str()
                );
            }
            let expected = match kind(block).unwrap().as_str() {
                "button" => Some("Save"),
                "checkbox" => Some("Enable"),
                "table" => Some("passed"),
                "form" => Some("Email is required"),
                "virtual-list" => Some("Job 00"),
                _ => None,
            };
            if let Some(expected) = expected {
                assert!(output.contains(expected), "{}: {output}", slide.id.as_str());
            }
            assert!(!output.contains("unknown component"));
        }
        assert_eq!(count, KINDS.len());
    }

    #[test]
    fn gallery_is_available_through_the_managed_owner_mirror() {
        let deck = bmux_presenter::DeckBuilder::new("gallery")
            .slide(
                bmux_presenter::SlideBuilder::new("checkbox")
                    .custom(
                        "gallery-checkbox",
                        "tui-gallery",
                        serde_json::json!({"kind": "checkbox"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let state = bmux_presenter::PresentationState::new(&deck);
        let mut owner =
            crate::owner::LinkedOwner::new(deck, &[], crate::demo::linked_registry()).unwrap();
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "gallery-test".into(),
            deck_revision: 1,
            revision: 1,
        };
        let frames = owner
            .scenes(bmux_presenter_runtime::OwnerContext {
                participant: "presenter",
                instance: "shared",
                state: &state,
                cursor: &cursor,
            })
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].surface_id, "gallery-checkbox");
        assert!(
            frames[0]
                .cells
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .contains("Enable")
        );
    }

    #[test]
    fn gallery_rejects_unknown_kinds_and_unbounded_paste() {
        assert!(
            GalleryFactory
                .initial_state(&block("not-a-control"))
                .is_err()
        );
        let input = example("text-input-box");
        let before = input.state.lock().unwrap().clone();
        let layout = input.layout(Constraints::for_width(40), &mut LayoutCx::default());
        input.event(
            &Event::Paste("x".repeat(4097)),
            &layout,
            &mut EventCx::new(&layout),
        );
        assert_eq!(*input.state.lock().unwrap(), before);
    }
}
