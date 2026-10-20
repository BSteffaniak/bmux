//! Native, bounded examples for the tour. No source evaluation or terminal subprocesses.
use std::cell::{Cell, RefCell};

use bmux_presenter::custom::{LinkedComponentFactory, LinkedState, PresentationBuildContext};
use bmux_presenter::{Block, BlockKind};
use bmux_tui::prelude::{
    Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome, LayoutCx,
    LayoutNode, Line, PaintCx, Text, TextBlock,
};
use bmux_tui_components::{
    action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowPolicy, ActionRowState},
    badge::BadgeComponent,
    bar_chart::{BarChartComponent, BarChartItem},
    breadcrumbs::{BreadcrumbItem, BreadcrumbsComponent, BreadcrumbsOutcome, BreadcrumbsState},
    button::{ButtonComponent, ButtonState},
    canvas::{Canvas, CanvasBounds, CanvasPoint},
    chart::{Chart, ChartBounds, ChartDataset, ChartPoint},
    checkbox::{CheckboxComponent, CheckboxState},
    dialog::{Dialog, DialogComponent},
    diff_viewer::{DiffViewerInput, DiffViewerLayout, diff_viewer_rows},
    form_field::FormFieldComponent,
    modal_frame::{ModalSizing, ModalTheme},
    progress_bar::{ProgressBarComponent, ProgressBarValue},
    radio_group::{RadioGroupComponent, RadioGroupState, RadioOption},
    scroll_view::{ScrollView, ScrollViewComponent, ScrollViewOutcome, ScrollViewState},
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

fn interactive(kind: &str) -> bool {
    matches!(
        kind,
        "button"
            | "checkbox"
            | "breadcrumbs"
            | "radio-group"
            | "select-dropdown"
            | "selectable-list"
            | "tab-bar"
            | "tree-view"
            | "table"
            | "text-input-box"
            | "form"
            | "dialog"
            | "scroll-view"
            | "virtual-list"
    )
}

pub(crate) struct GalleryFactory;
impl LinkedComponentFactory for GalleryFactory {
    fn state_version(&self) -> u64 {
        3
    }
    fn initial_state(&self, block: &Block) -> Result<serde_json::Value, String> {
        kind(block)?;
        Ok(serde_json::json!({"selected": 0, "checked": false}))
    }
    fn build<'a>(&'a self, block: &'a Block, state: LinkedState) -> Result<Element<'a>, String> {
        self.build_with_context(block, state, PresentationBuildContext::default())
    }
    fn focusable(&self, block: &Block) -> bool {
        kind(block).is_ok_and(|kind| interactive(&kind))
    }
    fn build_with_context<'a>(
        &'a self,
        block: &'a Block,
        state: LinkedState,
        context: PresentationBuildContext,
    ) -> Result<Element<'a>, String> {
        Ok(Element::new(Gallery {
            id: block.id.as_str().into(),
            kind: kind(block)?,
            state,
            focused: context.focused,
            transient: GalleryTransient::new(context.interaction),
        }))
    }
}

#[derive(Clone)]
struct GalleryTransient(LinkedState);
impl GalleryTransient {
    fn new(handle: Option<LinkedState>) -> Self {
        Self(
            handle.unwrap_or_else(|| {
                std::sync::Arc::new(std::sync::Mutex::new(serde_json::json!({})))
            }),
        )
    }
    fn snapshot(&self, id: &str, kind: &str) -> serde_json::Value {
        self.0
            .lock()
            .expect("gallery interaction state poisoned")
            .get(id)
            .filter(|entry| entry["kind"] == kind)
            .and_then(|entry| entry.get("native"))
            .filter(|native| native.to_string().len() <= 16384)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
    fn save(&self, id: &str, kind: &str, native: serde_json::Value) {
        // Caller owns eviction; even malformed caller maps remain bounded.
        // Rebuilding, measuring, and painting never insert an entry.
        if id.len() > 1024 || native.to_string().len() > 16384 {
            return;
        }
        let mut root = self.0.lock().expect("gallery interaction state poisoned");
        if !root.is_object() {
            *root = serde_json::json!({});
        }
        let map = root.as_object_mut().expect("initialized object");
        if map.len() < 256 || map.contains_key(id) {
            map.insert(
                id.into(),
                serde_json::json!({"kind": kind, "native": native}),
            );
        }
    }
}

struct Gallery {
    id: String,
    kind: String,
    state: LinkedState,
    // Viewer-local context, never serialized into the linked owner state.
    focused: bool,
    transient: GalleryTransient,
}

impl Gallery {
    // Every pass rebuilds a borrowed control from bounded canonical state. Only an
    // event pass commits changes; layout and paint cannot mutate owner state.
    fn visit<R>(&self, width: u16, commit: bool, f: impl FnOnce(&dyn Component) -> R) -> R {
        let focused = self.focused && interactive(&self.kind);
        let f = |control: &dyn Component| {
            let hint = if interactive(&self.kind) {
                if focused { "[focus]" } else { "[control]" }
            } else {
                "[static preview]"
            };
            f(&bmux_tui::composition::Column::new()
                .id(format!("{}.frame", self.id))
                .child(TextBlock::new(hint))
                .child(BorrowedControl(control)))
        };
        let transient = self.transient.snapshot(&self.id, &self.kind);
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
        macro_rules! interaction {
            ($value:ident) => {
                let _ = $value.restore_snapshot(&transient);
                $value.interaction.focused = focused;
            };
        }
        macro_rules! save_interaction {
            ($value:expr) => {
                if commit {
                    let value = &$value;
                    self.transient.save(&self.id, &self.kind, value.snapshot());
                }
            };
        }
        macro_rules! selection {
            ($cell:ident, $control:expr, $getter:ident) => {{
                let result = f(&$control);
                save_interaction!($cell.get());
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
                interaction!(value);
                let state = Cell::new(value);
                let result = show!(
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
                );
                save_interaction!(state.get());
                result
            }
            "text-input-box" => {
                let buffer = saved
                    .get("buffer")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<bmux_text_edit::TextEditBuffer>(v).ok())
                    .unwrap_or_else(|| bmux_text_edit::TextEditBuffer::from_text("Ada"));
                let before_text = buffer.text().to_owned();
                let mut value = TextInputState::new(buffer);
                restore_editor(&mut value, &transient);
                let state = RefCell::new(value);
                let control = TextInputBoxComponent::new(
                    self.id.clone(),
                    TextInputPolicy::chat_composer(),
                    &state,
                )
                .policy(TextInputBoxPolicy::field().focused(focused));
                let result = f(&control);
                if commit && state.borrow().buffer().text().len() <= 4096 {
                    save_interaction!(state.borrow());
                    // Shared state owns chosen text, not this view's caret or selection.
                    // Native snapshot restores those over this normalized buffer.
                    if state.borrow().buffer().text() != before_text {
                        self.state.lock().expect("gallery state poisoned")["buffer"] =
                            serde_json::to_value(bmux_text_edit::TextEditBuffer::from_text(
                                state.borrow().buffer().text(),
                            ))
                            .expect("text buffer serializes");
                    }
                }
                result
            }
            "virtual-list" => {
                let mut list = VirtualList::new(self.id.clone());
                for index in 0..40_u64 {
                    list = list.component(index, TextBlock::new(format!("Job {index:02} — ready")));
                }
                let mut state = VirtualListState::new(0);
                if !commit {
                    list.sync(u64::from(width), &mut state, &mut LayoutCx::default());
                }
                state.scroll.interaction.focused = focused;
                state
                    .scroll
                    .set_vertical_offset(transient["offset"].as_u64().unwrap_or(0).min(35));
                let state = RefCell::new(state);
                let result = f(&VirtualPreview {
                    list,
                    state: &state,
                    focused,
                });
                if commit {
                    self.transient.save(
                        &self.id,
                        &self.kind,
                        serde_json::json!({"offset": state.borrow().scroll.vertical_offset()}),
                    );
                }
                result
            }
            "checkbox" => {
                let mut value = CheckboxState::new(saved["checked"].as_bool().unwrap_or(false));
                let _ = value.restore_snapshot(&transient);
                value.set_checked(saved["checked"].as_bool().unwrap_or(false));
                value.set_focused(focused);
                let state = Cell::new(value);
                let result = f(&CheckboxComponent::new(self.id.clone(), "Enable", &state));
                save_interaction!(state.get());
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
                let _ = value.restore_snapshot(&transient);
                value.set_current(Some(selected));
                value.set_focused(focused);
                let state = Cell::new(value);
                selection!(
                    state,
                    BreadcrumbExample {
                        native: BreadcrumbsComponent::new(self.id.clone(), &items, &state),
                        state: &state
                    },
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
                interaction!(value);
                value.set_selected(Some(selected));
                let state = Cell::new(value);
                let result = f(&RadioGroupComponent::new(self.id.clone(), &options, &state));
                save_interaction!(state.get());
                if commit {
                    let value = state.get();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected());
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
                interaction!(value);
                value.set_selected(Some(selected));
                let state = Cell::new(value);
                let result = f(
                    &bmux_tui::composition::SizeBox::new(SelectDropdownComponent::new(
                        self.id.clone(),
                        &options,
                        &state,
                    ))
                    .height(4),
                );
                save_interaction!(state.get());
                if commit {
                    let value = state.get();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected());
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
                interaction!(value);
                value.set_selected(Some(selected));
                let state = Cell::new(value);
                let result = f(&SelectableListComponent::new(
                    self.id.clone(),
                    &items,
                    &state,
                ));
                save_interaction!(state.get());
                if commit {
                    let value = state.get();
                    let mut saved = self.state.lock().expect("gallery state poisoned");
                    saved["selected"] = serde_json::json!(value.selected());
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
                interaction!(value);
                value.set_selected(Some(selected));
                let state = RefCell::new(value);
                let result = f(&TabBarComponent::new(self.id.clone(), &items, &state));
                save_interaction!(state.borrow());
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
                interaction!(value);
                value.set_selected_visible(Some(selected));
                value.set_expanded("src", saved["expanded"].as_bool().unwrap_or(true));
                let state = RefCell::new(value);
                let result = f(&TreeViewComponent::new(self.id.clone(), &items, &state));
                save_interaction!(state.borrow());
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
                interaction!(value);
                value.set_selected(Some(selected));
                let state = RefCell::new(value);
                let result = f(&TableComponent::new(
                    self.id.clone(),
                    &columns,
                    &rows,
                    &state,
                ));
                save_interaction!(state.borrow());
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
                let mut native = ActionRowState::new();
                let _ = native.restore_snapshot(&transient["actions"]);
                native.set_focused(Some(native.focused().unwrap_or(0).min(1)));
                native.interaction.focused = focused;
                let state = Cell::new(native);
                let mut reopen = ButtonState::new();
                let _ = reopen.restore_snapshot(&transient["reopen"]);
                reopen.set_focused(focused);
                let reopen = Cell::new(reopen);
                let closed = Cell::new(transient["closed"].as_bool().unwrap_or(false));
                let activated = RefCell::new(None);
                let control = DialogExample {
                    id: &self.id,
                    body: &body,
                    actions: &actions,
                    state: &state,
                    reopen: &reopen,
                    closed: &closed,
                    activated: &activated,
                    status: saved["dialog_status"]
                        .as_str()
                        .unwrap_or("Choose Save or Cancel"),
                };
                let result = f(&control);
                if commit {
                    self.transient.save(&self.id,&self.kind,serde_json::json!({
                        "actions":state.get().snapshot(),"reopen":reopen.get().snapshot(),"closed":closed.get()
                    }));
                    if let Some(action) = activated.into_inner() {
                        self.state.lock().expect("gallery state poisoned")["dialog_status"] =
                            serde_json::json!(action);
                    }
                }
                result
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
            "scroll-view" => {
                let mut state = ScrollViewState::new();
                state.interaction.focused = focused;
                state.set_vertical_offset(transient["offset"].as_u64().unwrap_or(0).min(36));
                let state = Cell::new(state);
                let child = TextBlock::new(
                    (0..40)
                        .map(|i| format!("Row {i:02} — scroll me"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                let control = ScrollExample {
                    id: self.id.clone(),
                    state: &state,
                    child,
                    focused,
                };
                let result = f(&control);
                if commit {
                    self.transient.save(
                        &self.id,
                        &self.kind,
                        serde_json::json!({"offset": state.get().vertical_offset()}),
                    );
                }
                result
            }
            "form" => self.visit_form(&saved, &transient, focused, commit, f),
            _ => unreachable!("factory validated the gallery kind"),
        }
    }
    fn visit_form<R>(
        &self,
        saved: &serde_json::Value,
        transient: &serde_json::Value,
        focused: bool,
        commit: bool,
        f: impl FnOnce(&dyn Component) -> R,
    ) -> R {
        let mut name = TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(
            saved["name"].as_str().unwrap_or("Braden"),
        ));
        let mut email = TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(
            saved["email"].as_str().unwrap_or(""),
        ));
        restore_editor(&mut name, &transient["name"]);
        restore_editor(&mut email, &transient["email"]);
        let name = RefCell::new(name);
        let email = RefCell::new(email);
        let mut button = ButtonState::new();
        let _ = button.restore_snapshot(&transient["button"]);
        let button = Cell::new(button);
        let active = Cell::new(
            usize::try_from(transient["active"].as_u64().unwrap_or(0).min(2)).unwrap_or(0),
        );
        let submitted = Cell::new(false);
        let status = saved["status"].as_str().unwrap_or("Email is required");
        let control = FormExample {
            id: &self.id,
            name: &name,
            email: &email,
            button: &button,
            active: &active,
            focused,
            submitted: &submitted,
            status,
        };
        let result = f(&control);
        if commit
            && name.borrow().buffer().text().len() <= 4096
            && email.borrow().buffer().text().len() <= 4096
        {
            self.transient.save(
                &self.id,
                &self.kind,
                serde_json::json!({
                    "active": active.get(), "name": name.borrow().snapshot(),
                    "email": email.borrow().snapshot(), "button": button.get().snapshot()
                }),
            );
            let mut saved = self.state.lock().expect("gallery state poisoned");
            saved["name"] = serde_json::json!(name.borrow().buffer().text());
            saved["email"] = serde_json::json!(email.borrow().buffer().text());
            if submitted.get() {
                saved["status"] = serde_json::json!(if name.borrow().buffer().text().is_empty() {
                    "Name is required"
                } else if email.borrow().buffer().text().is_empty() {
                    "Email is required"
                } else {
                    "Saved"
                });
            }
        }
        result
    }
}

// Rebase native caret/selection onto current canonical text without restoring stale text.
fn restore_editor(state: &mut TextInputState, snapshot: &serde_json::Value) {
    if snapshot.is_null() {
        return;
    }
    let text = state.buffer().text().to_owned();
    let mut snapshot = snapshot.clone();
    if let Some(buffer) = snapshot
        .get_mut("buffer")
        .and_then(serde_json::Value::as_object_mut)
    {
        buffer.insert("text".into(), serde_json::json!(text));
        for field in ["cursor", "selection_anchor"] {
            if let Some(index) = buffer.get(field).and_then(serde_json::Value::as_u64) {
                let mut rebased = bmux_text_edit::TextEditBuffer::from_text(&text);
                rebased.move_cursor(bmux_text_edit::TextMotion::Absolute(
                    usize::try_from(index).unwrap_or(usize::MAX),
                ));
                buffer.insert(field.into(), serde_json::json!(rebased.cursor_byte_index()));
            }
        }
    }
    let _ = state.restore_snapshot(&snapshot);
}

impl Component for Gallery {
    fn revision(&self) -> ComponentRevision {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.kind.hash(&mut hash);
        self.focused.hash(&mut hash);
        self.transient
            .snapshot(&self.id, &self.kind)
            .to_string()
            .hash(&mut hash);
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
        // Native controls resolve pointer coordinates from this committed layout.
        // Never measure a replacement tree in an input pass.
        if !interactive(&self.kind)
            || (!self.focused && matches!(event, Event::Key(_) | Event::Paste(_)))
        {
            return EventOutcome::Ignored;
        }
        if matches!(event, Event::Paste(text) if text.len() > 4096) {
            return EventOutcome::Handled;
        }
        let mut button_state = ButtonState::new();
        if self.kind == "button" {
            let _ = button_state.restore_snapshot(&self.transient.snapshot(&self.id, &self.kind));
        }
        let button_pointer_activation = self.kind == "button"
            && matches!(event, Event::Mouse(mouse) if mouse.kind == bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left)
                && cx.find_visible_rect(&format!("{}.save", self.id).into()).is_some_and(|area| area.contains(mouse.position)))
            && button_state.interaction.pressed;
        let outcome = self.visit(
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            true,
            |control| control.event(event, layout, cx),
        );
        if self.kind == "button"
            && outcome == EventOutcome::Redraw
            && (button_pointer_activation
                || matches!(event, Event::Key(key) if matches!(key.key, bmux_keyboard::KeyCode::Enter | bmux_keyboard::KeyCode::Char(' '))))
        {
            let mut saved = self.state.lock().expect("gallery state poisoned");
            saved["activations"] =
                serde_json::json!(saved["activations"].as_u64().unwrap_or(0).saturating_add(1));
        }
        outcome
    }
}

// Borrow only for a single visit; native controls retain no hidden gallery state.
struct BorrowedControl<'a>(&'a dyn Component);
impl Component for BorrowedControl<'_> {
    fn revision(&self) -> ComponentRevision {
        self.0.revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }
}

struct DialogExample<'a> {
    id: &'a str,
    body: &'a [Line],
    actions: &'a [ActionButton],
    state: &'a Cell<ActionRowState>,
    reopen: &'a Cell<ButtonState>,
    closed: &'a Cell<bool>,
    activated: &'a RefCell<Option<&'static str>>,
    status: &'a str,
}
impl DialogExample<'_> {
    fn tree(&self) -> Element<'_> {
        let content = if self.closed.get() {
            Element::new(
                bmux_tui::composition::Column::new()
                    .id(format!("{}.closed", self.id))
                    .child(TextBlock::new(self.status))
                    .child(ButtonComponent::new(
                        format!("{}.reopen", self.id),
                        "Reopen / reset",
                        self.reopen,
                    )),
            )
        } else {
            Element::new(DialogComponent::new(
                self.id.to_owned(),
                Dialog::new(
                    self.body,
                    self.actions,
                    ModalTheme::dark(bmux_tui::prelude::Color::Cyan),
                )
                .sizing(ModalSizing::new(
                    bmux_tui::geometry::Size::new(20, 5),
                    bmux_tui::geometry::Size::new(80, 6),
                    bmux_tui::geometry::Insets::all(0),
                ))
                .padding(bmux_tui::geometry::Insets::new(0, 1, 0, 1))
                .title("Unsaved changes"),
                self.state,
            ))
        };
        Element::new(
            bmux_tui::composition::SizeBox::new(content)
                .id(format!("{}.stable", self.id))
                .height(6),
        )
    }
    fn action(&self, action: &'static str) -> EventOutcome {
        *self.activated.borrow_mut() = Some(action);
        self.closed.set(true);
        EventOutcome::Redraw
    }
}
impl Component for DialogExample<'_> {
    fn layout(&self, c: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(c, cx)
    }
    fn paint(&self, l: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(l, cx);
    }
    fn event(&self, e: &Event, l: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if self.closed.get() {
            let was_pressed = self.reopen.get().interaction.pressed;
            let outcome = self.tree().event(e, l, cx);
            let clicked = was_pressed
                && matches!(e,Event::Mouse(mouse) if mouse.kind == bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left)
                && cx.find_visible_rect(&format!("{}.reopen",self.id).into()).is_some_and(|area|area.contains(mouse.position)));
            if outcome.is_handled()
                && (clicked
                    || matches!(e,Event::Key(key) if matches!(key.key,bmux_keyboard::KeyCode::Enter|bmux_keyboard::KeyCode::Char(' '))))
            {
                self.closed.set(false);
                let mut state = ActionRowState::new();
                state.set_focused(Some(0));
                self.state.set(state);
                *self.activated.borrow_mut() = Some("Choose Save or Cancel");
                return EventOutcome::Redraw;
            }
            return outcome;
        }
        if let Event::Key(key) = e {
            if key.key == bmux_keyboard::KeyCode::Escape {
                return self.action("Cancelled");
            }
            if key.key == bmux_keyboard::KeyCode::Tab {
                let mut state = self.state.get();
                let index = state.focused().unwrap_or(0);
                if (key.modifiers.shift && index == 0) || (!key.modifiers.shift && index == 1) {
                    return EventOutcome::Ignored;
                }
                state.set_focused(Some(usize::from(!key.modifiers.shift)));
                self.state.set(state);
                return EventOutcome::Redraw;
            }
        }
        if matches!(
            e,
            Event::Focus(bmux_tui::event::FocusEvent::Lost) | Event::Resize(_)
        ) {
            let mut state = ActionRowState::new();
            state.set_focused(self.state.get().focused());
            self.state.set(state);
            return EventOutcome::Redraw;
        }
        let Some(area) = cx.find_rect(&format!("{}.actions", self.id).into()) else {
            return EventOutcome::Ignored;
        };
        if matches!(e, Event::Mouse(mouse) if matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_) | bmux_tui::event::MouseEventKind::Move)
            && !cx.find_visible_rect(&format!("{}.actions", self.id).into()).is_some_and(|visible| visible.contains(mouse.position)))
        {
            return EventOutcome::Ignored;
        }
        let mut state = self.state.get();
        // Native controller uses the same committed action-row rectangle painted by Dialog.
        let result = ActionRow::new(self.actions)
            .policy(ActionRowPolicy::roving())
            .handle_event(area, &mut state, e);
        self.state.set(state);
        match result {
            ActionRowOutcome::Activated { index, .. } => {
                self.action(if index == 0 { "Saved" } else { "Cancelled" })
            }
            ActionRowOutcome::Ignored => EventOutcome::Ignored,
            _ => EventOutcome::Redraw,
        }
    }
}

struct BreadcrumbExample<'a, 'state> {
    native: BreadcrumbsComponent<'a, 'state>,
    state: &'state Cell<BreadcrumbsState>,
}
impl Component for BreadcrumbExample<'_, '_> {
    fn layout(&self, c: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.native.layout(c, cx)
    }
    fn paint(&self, l: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.native.paint(l, cx);
    }
    fn event(&self, e: &Event, l: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        match self.native.handle_event(e, l, cx) {
            BreadcrumbsOutcome::Activated { index, .. } => {
                let mut state = self.state.get();
                state.set_current(Some(index));
                self.state.set(state);
                EventOutcome::Redraw
            }
            BreadcrumbsOutcome::Redraw => EventOutcome::Redraw,
            BreadcrumbsOutcome::Ignored => EventOutcome::Ignored,
        }
    }
}

struct FormExample<'a> {
    id: &'a str,
    name: &'a RefCell<TextInputState>,
    email: &'a RefCell<TextInputState>,
    button: &'a Cell<ButtonState>,
    active: &'a Cell<usize>,
    focused: bool,
    submitted: &'a Cell<bool>,
    status: &'a str,
}
impl FormExample<'_> {
    fn child(&self, index: usize) -> Element<'_> {
        if index == 2 {
            return Element::new(FormSave {
                id: format!("{}.save", self.id),
                state: self.button,
                focused: self.focused && self.active.get() == 2,
            });
        }
        let (key, label, state) = if index == 0 {
            ("name", "Name", self.name)
        } else {
            ("email", "Email", self.email)
        };
        Element::new(FormFieldComponent::new(
            format!("{}.{key}", self.id),
            label,
            TextInputBoxComponent::new(
                format!("{}.{key}.input", self.id),
                TextInputPolicy::chat_composer(),
                state,
            )
            .policy(
                TextInputBoxPolicy::field()
                    .focused(self.focused && self.active.get() == index)
                    .rows(1, Some(1)),
            ),
        ))
    }
    fn tree(&self) -> impl Component + '_ {
        bmux_tui::composition::Column::new()
            .id(self.id.to_owned())
            .child(self.child(0))
            .child(self.child(1))
            .child(self.child(2))
            .child(TextBlock::new(self.status))
    }
}
impl Component for FormExample<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if let Event::Key(key) = event {
            if key.key == bmux_keyboard::KeyCode::Tab {
                let active = self.active.get();
                if key.modifiers.shift {
                    if active == 0 {
                        return EventOutcome::Ignored;
                    }
                    self.active.set(active - 1);
                } else {
                    if active == 2 {
                        return EventOutcome::Ignored;
                    }
                    self.active.set(active + 1);
                }
                return EventOutcome::Redraw;
            }
            if key.key == bmux_keyboard::KeyCode::Enter
                || (self.active.get() == 2 && key.key == bmux_keyboard::KeyCode::Char(' '))
            {
                self.submitted.set(true);
                return EventOutcome::Redraw;
            }
            if self.active.get() == 2 {
                return EventOutcome::Ignored;
            }
        }
        if let Event::Mouse(mouse) = event
            && matches!(
                mouse.kind,
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
            )
        {
            for (index, child) in layout.children.iter().take(3).enumerate() {
                let hit = cx.with_child(child, |cx| {
                    cx.find_visible_rect(&child.node.id)
                        .is_some_and(|area| area.contains(mouse.position))
                });
                if hit {
                    self.active.set(index);
                    break;
                }
            }
        }
        // Dispatch only to the active descendant; a Column would broadcast keys.
        let active = self.active.get();
        let Some(child) = layout.children.get(active) else {
            return EventOutcome::Ignored;
        };
        let pressed = self.button.get().interaction.pressed;
        let result = cx.with_child(child, |cx| self.child(active).event(event, &child.node, cx));
        if active == 2
            && pressed
            && result.is_handled()
            && matches!(event, Event::Mouse(mouse) if mouse.kind == bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left)
                && cx.find_visible_rect(&child.node.id).is_some_and(|area| area.contains(mouse.position)))
        {
            self.submitted.set(true);
        }
        result
    }
}
struct FormSave<'a> {
    id: String,
    state: &'a Cell<ButtonState>,
    focused: bool,
}
impl FormSave<'_> {
    fn visit<R>(&self, commit: bool, f: impl FnOnce(&dyn Component) -> R) -> R {
        let mut value = self.state.get();
        value.set_focused(self.focused);
        let state = Cell::new(value);
        let result = f(&ButtonComponent::new(self.id.clone(), "Save", &state));
        if commit {
            self.state.set(state.get());
        }
        result
    }
}
impl Component for FormSave<'_> {
    fn layout(&self, c: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.visit(false, |control| control.layout(c, cx))
    }
    fn paint(&self, l: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.visit(false, |control| control.paint(l, cx));
    }
    fn event(&self, e: &Event, l: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.visit(true, |control| control.event(e, l, cx))
    }
}

struct ScrollExample<'a> {
    id: String,
    state: &'a Cell<ScrollViewState>,
    child: TextBlock,
    focused: bool,
}
impl ScrollExample<'_> {
    fn viewport(&self, width: u64) -> ScrollViewComponent<'_> {
        ScrollViewComponent::new(
            self.id.clone(),
            bmux_tui::component::LogicalSize::new(width, 4),
            self.state.get(),
            BorrowedControl(&self.child),
        )
    }
}
impl Component for ScrollExample<'_> {
    fn layout(&self, c: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.viewport(c.max_width()).layout(c, cx)
    }
    fn paint(&self, l: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let style = if self.focused {
            bmux_tui::prelude::Style::new().add_modifier(bmux_tui::style::Modifier::UNDERLINE)
        } else {
            bmux_tui::prelude::Style::new()
        };
        cx.with_style(style, |cx| self.viewport(l.size.width).paint(l, cx));
        cx.push_hit(
            bmux_tui::hit::HitRegion::new(
                self.id.clone(),
                bmux_tui::prelude::Rect::new(
                    0,
                    0,
                    u16::try_from(l.size.width).unwrap_or(u16::MAX),
                    4,
                ),
            )
            .role(bmux_tui::hit::HitRole::Scroll),
        );
    }
    fn event(&self, e: &Event, l: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_visible_rect(&l.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let result = ScrollView::new().handle_event(area, l, &mut state, e);
        self.state.set(state);
        if result == ScrollViewOutcome::Ignored {
            EventOutcome::Ignored
        } else {
            EventOutcome::Redraw
        }
    }
}

struct VirtualPreview<'a> {
    list: VirtualList<'a, u64>,
    state: &'a RefCell<VirtualListState<u64>>,
    focused: bool,
}
impl Component for VirtualPreview<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        ScrollViewComponent::viewport_layout(
            "gallery.virtual".into(),
            constraints.constrain(bmux_tui::component::LogicalSize::new(
                constraints.max_width(),
                5,
            )),
            LayoutNode::leaf(
                "gallery.virtual.content".into(),
                bmux_tui::component::LogicalSize::new(constraints.max_width(), 40),
            ),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let style = if self.focused {
            bmux_tui::prelude::Style::new().add_modifier(bmux_tui::style::Modifier::UNDERLINE)
        } else {
            bmux_tui::prelude::Style::new()
        };
        cx.with_style(style, |cx| {
            self.list.paint(
                bmux_tui::geometry::Rect::new(
                    0,
                    0,
                    u16::try_from(layout.size.width).unwrap_or(u16::MAX),
                    u16::try_from(layout.size.height).unwrap_or(u16::MAX),
                ),
                &self.state.borrow(),
                cx,
            );
        });
        cx.push_hit(
            bmux_tui::hit::HitRegion::new(
                "gallery.virtual",
                bmux_tui::prelude::Rect::new(
                    0,
                    0,
                    u16::try_from(layout.size.width).unwrap_or(u16::MAX),
                    u16::try_from(layout.size.height).unwrap_or(u16::MAX),
                ),
            )
            .role(bmux_tui::hit::HitRole::Scroll),
        );
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_visible_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.borrow_mut();
        let result = ScrollView::new().handle_event(area, layout, &mut state.scroll, event);
        if result == ScrollViewOutcome::Ignored {
            EventOutcome::Ignored
        } else {
            EventOutcome::Redraw
        }
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
            focused: true,
            transient: GalleryTransient::new(None),
        }
    }
    fn paint(component: &dyn Component, width: u16) -> String {
        let layout = component.layout(
            Constraints::new(0, u64::from(width), 0, Some(16)),
            &mut LayoutCx::default(),
        );
        assert!(layout.size.height > 0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 16));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        (0..16)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn key(component: &dyn Component, code: KeyCode) {
        let layout = component.layout(
            Constraints::new(0, 60, 0, Some(16)),
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
            let transient_before = example.transient.0.lock().unwrap().clone();
            for width in [28, 60, 100] {
                assert!(
                    !paint(&example, width).trim().is_empty(),
                    "{kind} at {width}"
                );
            }
            assert_eq!(*example.state.lock().unwrap(), before, "{kind}");
            assert_eq!(
                *example.transient.0.lock().unwrap(),
                transient_before,
                "{kind}"
            );
        }
    }
    #[test]
    fn gallery_classifies_every_kind_and_static_previews_ignore_input() {
        let controls = [
            "button",
            "checkbox",
            "breadcrumbs",
            "radio-group",
            "select-dropdown",
            "selectable-list",
            "tab-bar",
            "tree-view",
            "table",
            "text-input-box",
            "form",
            "dialog",
            "scroll-view",
            "virtual-list",
        ];
        for kind in KINDS {
            let control = example(kind);
            assert_eq!(
                GalleryFactory.focusable(&block(kind)),
                controls.contains(kind),
                "{kind}"
            );
            if !controls.contains(kind) {
                let before = control.state.lock().unwrap().clone();
                key(&control, KeyCode::Enter);
                key(&control, KeyCode::Down);
                assert_eq!(*control.state.lock().unwrap(), before, "{kind}");
                assert!(paint(&control, 60).contains("[static preview]"), "{kind}");
            }
        }
    }

    #[test]
    fn gallery_focus_is_view_local_and_never_changes_selection_or_shared_state() {
        for kind in KINDS.iter().filter(|kind| interactive(kind)) {
            let focused = example(kind);
            let blurred = Gallery {
                id: focused.id.clone(),
                kind: focused.kind.clone(),
                state: Arc::clone(&focused.state),
                focused: false,
                transient: GalleryTransient::new(None),
            };
            let before = focused.state.lock().unwrap().clone();
            assert!(paint(&focused, 60).contains("[focus]"), "{kind}");
            assert!(paint(&blurred, 60).contains("[control]"), "{kind}");
            assert_ne!(focused.revision(), blurred.revision(), "{kind}");
            key(&blurred, KeyCode::Enter);
            key(&blurred, KeyCode::Char('x'));
            assert_eq!(*focused.state.lock().unwrap(), before, "{kind}");
            assert!(focused.state.lock().unwrap().get("focused").is_none());
        }
    }

    #[test]
    fn gallery_each_interactive_kind_changes_and_survives_rebuild() {
        for (kind, keys, field, expected) in [
            (
                "button",
                vec![KeyCode::Enter],
                "activations",
                serde_json::json!(1),
            ),
            (
                "checkbox",
                vec![KeyCode::Char(' ')],
                "checked",
                serde_json::json!(true),
            ),
            (
                "breadcrumbs",
                vec![KeyCode::Right],
                "selected",
                serde_json::json!(1),
            ),
            (
                "radio-group",
                vec![KeyCode::Down, KeyCode::Char(' ')],
                "selected",
                serde_json::json!(1),
            ),
            (
                "selectable-list",
                vec![KeyCode::Down, KeyCode::Enter],
                "selected",
                serde_json::json!(1),
            ),
            (
                "select-dropdown",
                vec![KeyCode::Enter, KeyCode::Down, KeyCode::Enter],
                "selected",
                serde_json::json!(1),
            ),
            (
                "tab-bar",
                vec![KeyCode::Right],
                "selected",
                serde_json::json!(1),
            ),
            (
                "tree-view",
                vec![KeyCode::Down],
                "selected",
                serde_json::json!(1),
            ),
            (
                "table",
                vec![KeyCode::Down],
                "selected",
                serde_json::json!(1),
            ),
        ] {
            let control = example(kind);
            for code in keys {
                key(&control, code);
            }
            assert_eq!(control.state.lock().unwrap()[field], expected, "{kind}");
            let rebuilt = Gallery {
                id: control.id.clone(),
                kind: control.kind.clone(),
                state: Arc::clone(&control.state),
                focused: true,
                transient: control.transient.clone(),
            };
            let before = control.state.lock().unwrap().clone();
            assert_eq!(paint(&control, 60), paint(&rebuilt, 60), "{kind}");
            assert_eq!(*control.state.lock().unwrap(), before, "{kind}");
        }
        let input = example("text-input-box");
        key(&input, KeyCode::Char('x'));
        let rebuilt = Gallery {
            id: input.id.clone(),
            kind: input.kind.clone(),
            state: Arc::clone(&input.state),
            focused: true,
            transient: input.transient.clone(),
        };
        assert!(paint(&rebuilt, 60).contains('x'));
    }

    #[test]
    fn gallery_pointer_uses_committed_control_geometry_and_retains_trigger_press() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        for kind in ["button", "checkbox", "select-dropdown"] {
            let control = example(kind);
            let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
            let id = if kind == "button" {
                format!("{}.save", control.id)
            } else {
                control.id.clone()
            };
            let area = layout.find_rect(&id.into()).unwrap();
            // The status cue occupies row zero and is deliberately not a control.
            assert!(area.y > 0);
            let before = control.state.lock().unwrap().clone();
            let send = |kind, position| {
                control.event(
                    &Event::Mouse(MouseEvent::new(kind, position)),
                    &layout,
                    &mut EventCx::new(&layout),
                )
            };
            send(MouseEventKind::Down(MouseButton::Left), Point::new(0, 0));
            send(MouseEventKind::Up(MouseButton::Left), Point::new(0, 0));
            assert_eq!(
                *control.state.lock().unwrap(),
                before,
                "{kind}: cue is not a target"
            );
            let point = Point::new(area.x, area.y);
            send(MouseEventKind::Down(MouseButton::Left), point);
            // Read-only rebuilds between the press and release must not lose the press.
            let transient = control.transient.snapshot(&control.id, &control.kind);
            let mut pressed = ButtonState::new();
            if kind == "button" {
                let _ = pressed.restore_snapshot(&transient);
                assert!(pressed.interaction.pressed);
            }
            paint(&control, 60);
            send(MouseEventKind::Up(MouseButton::Left), point);
            let saved = control.state.lock().unwrap();
            match kind {
                "button" => assert_eq!(saved["activations"], 1),
                "checkbox" => assert_eq!(saved["checked"], true),
                "select-dropdown" => {
                    let mut native = SelectDropdownState::new(Some(0));
                    let _ = native
                        .restore_snapshot(&control.transient.snapshot(&control.id, &control.kind));
                    assert!(native.is_open());
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn gallery_all_native_pointer_choices_use_painted_regions() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        for (kind, target) in [
            ("breadcrumbs", "Projects"),
            ("radio-group", "Frozen"),
            ("selectable-list", "Test"),
            ("tab-bar", "Activity"),
            ("tree-view", "main.rs"),
            ("table", "test"),
            ("select-dropdown", "Running"),
        ] {
            let control = example(kind);
            if kind == "select-dropdown" {
                key(&control, KeyCode::Enter);
            }
            let layout = control.layout(
                Constraints::new(0, 60, 0, Some(12)),
                &mut LayoutCx::default(),
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 12));
            let mut frame = Frame::new(&mut buffer);
            control.paint(&layout, &mut PaintCx::new(&mut frame));
            let y = (1..12)
                .find(|&y| {
                    frame
                        .buffer()
                        .row_symbols(y)
                        .is_some_and(|line| line.contains(target))
                })
                .unwrap_or_else(|| panic!("{kind}: missing {target}"));
            let row = frame.buffer().row_symbols(y).unwrap();
            let start = row.find(target).unwrap();
            let point = Point::new(
                u16::try_from(bmux_tui::text_width::display_width(&row[..start])).unwrap(),
                y,
            );
            assert!(
                frame.hits().regions().iter().any(|region| region.enabled
                    && region.pointer_events
                    && region.area.contains(point)),
                "{kind}: label must belong to a painted pointer region"
            );
            control.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    point,
                )),
                &layout,
                &mut EventCx::new(&layout),
            );
            // A real rebuild uses the same per-view handle, not a replayed key.
            let rebuilt = Gallery {
                id: control.id.clone(),
                kind: control.kind.clone(),
                state: Arc::clone(&control.state),
                focused: true,
                transient: control.transient.clone(),
            };
            rebuilt.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    point,
                )),
                &layout,
                &mut EventCx::new(&layout),
            );
            assert_eq!(control.state.lock().unwrap()["selected"], 1, "{kind}");
        }
    }

    #[test]
    fn gallery_text_pointer_cursor_and_focus_are_visible_and_rebuild_safe() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        let control = example("text-input-box");
        let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 10));
        let mut frame = Frame::new(&mut buffer);
        control.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(frame.cursor().is_some_and(|cursor| cursor.visible));
        let region = frame
            .hits()
            .regions()
            .iter()
            .find(|region| region.role == bmux_tui::hit::HitRole::TextInput)
            .unwrap();
        let point = Point::new(region.area.x, region.area.y);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            control.event(
                &Event::Mouse(MouseEvent::new(kind, point)),
                &layout,
                &mut EventCx::new(&layout),
            );
        }
        key(&control, KeyCode::Char('x'));
        assert!(paint(&control, 60).contains("xAda"));
        let blurred = Gallery {
            id: control.id.clone(),
            kind: control.kind.clone(),
            state: Arc::clone(&control.state),
            focused: false,
            transient: control.transient.clone(),
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 10));
        let mut frame = Frame::new(&mut buffer);
        blurred.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(frame.cursor().is_none_or(|cursor| !cursor.visible));
    }

    #[test]
    fn gallery_text_caret_is_view_local_while_chosen_text_is_shared() {
        let a = example("text-input-box");
        let b = Gallery {
            id: a.id.clone(),
            kind: a.kind.clone(),
            state: Arc::clone(&a.state),
            focused: true,
            transient: GalleryTransient::new(None),
        };
        key(&b, KeyCode::End);
        let text_before = a.state.lock().unwrap().clone();
        key(&a, KeyCode::Home);
        assert_eq!(
            *a.state.lock().unwrap(),
            text_before,
            "caret navigation must not change shared text state"
        );
        key(&b, KeyCode::Char('b'));
        assert!(paint(&a, 60).contains("Adab"));
        key(&a, KeyCode::Char('a'));
        assert!(paint(&b, 60).contains("aAdab"));
    }

    #[test]
    fn gallery_native_focus_styles_change_beyond_the_status_cue() {
        use bmux_tui::geometry::Point;
        for kind in KINDS.iter().filter(|kind| interactive(kind)) {
            let focused = example(kind);
            let blurred = Gallery {
                id: focused.id.clone(),
                kind: focused.kind.clone(),
                state: Arc::clone(&focused.state),
                focused: false,
                transient: GalleryTransient::new(None),
            };
            let render = |control: &Gallery| {
                let layout = control.layout(
                    Constraints::new(0, 60, 0, Some(16)),
                    &mut LayoutCx::default(),
                );
                let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 10));
                control.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
                buffer
            };
            let on = render(&focused);
            let off = render(&blurred);
            assert!(
                (1..10).any(|y| (0..60).any(|x| {
                    on.get(Point::new(x, y)).unwrap().style
                        != off.get(Point::new(x, y)).unwrap().style
                })),
                "{kind}: native styling must change, not only the wrapper cue"
            );
        }
    }

    #[test]
    fn gallery_dropdown_starts_closed_and_does_not_reopen_during_paint() {
        let control = example("select-dropdown");
        assert!(!paint(&control, 60).contains("Running"));
        key(&control, KeyCode::Enter);
        assert!(
            !control
                .transient
                .snapshot(&control.id, &control.kind)
                .is_null()
        );
        assert!(paint(&control, 60).contains("Running"));
        let before = control.state.lock().unwrap().clone();
        paint(&control, 28);
        paint(&control, 100);
        assert_eq!(*control.state.lock().unwrap(), before);
        key(&control, KeyCode::Escape);
        let mut native = SelectDropdownState::new(Some(0));
        let _ = native.restore_snapshot(&control.transient.snapshot(&control.id, &control.kind));
        assert!(!native.is_open());
        assert!(!paint(&control, 60).contains("Running"));
    }

    #[test]
    fn gallery_views_share_values_not_pending_navigation_or_pointer_gestures() {
        for (kind, start, finish) in [
            ("radio-group", vec![KeyCode::Down], KeyCode::Char(' ')),
            ("selectable-list", vec![KeyCode::Down], KeyCode::Enter),
            (
                "select-dropdown",
                vec![KeyCode::Enter, KeyCode::Down],
                KeyCode::Enter,
            ),
        ] {
            let a = example(kind);
            let b = Gallery {
                id: a.id.clone(),
                kind: a.kind.clone(),
                state: Arc::clone(&a.state),
                focused: true,
                transient: GalleryTransient::new(None),
            };
            let canonical = a.state.lock().unwrap().clone();
            let b_before = paint(&b, 60);
            for code in start {
                key(&a, code);
            }
            assert_eq!(
                *a.state.lock().unwrap(),
                canonical,
                "{kind}: pending navigation is not a value"
            );
            assert_eq!(
                paint(&b, 60),
                b_before,
                "{kind}: another view did not navigate"
            );
            assert!(b.transient.snapshot(&b.id, kind).is_null());
            let rebuilt = Gallery {
                id: a.id.clone(),
                kind: a.kind.clone(),
                state: Arc::clone(&a.state),
                focused: true,
                transient: a.transient.clone(),
            };
            assert_eq!(paint(&a, 60), paint(&rebuilt, 60));
            key(&rebuilt, finish);
            assert_eq!(a.state.lock().unwrap()["selected"], 1, "{kind}");
            for field in ["focused", "active", "open", "pressed", "hovered"] {
                assert!(
                    a.state.lock().unwrap().get(field).is_none(),
                    "{kind}: {field}"
                );
            }
        }
    }

    #[test]
    fn gallery_paint_after_interaction_preserves_both_authoritative_stores() {
        for kind in KINDS.iter().filter(|kind| interactive(kind)) {
            let control = example(kind);
            key(&control, KeyCode::Enter);
            key(&control, KeyCode::Down);
            let canonical = control.state.lock().unwrap().clone();
            let transient = control.transient.0.lock().unwrap().clone();
            for width in [1, 28, 60, 100] {
                paint(&control, width);
                assert_eq!(
                    *control.state.lock().unwrap(),
                    canonical,
                    "{kind}: canonical paint write"
                );
                assert_eq!(
                    *control.transient.0.lock().unwrap(),
                    transient,
                    "{kind}: transient paint write"
                );
            }
        }
    }

    #[test]
    fn gallery_pending_dropdown_cancel_and_tree_expansion_survive_rebuild() {
        let dropdown = example("select-dropdown");
        key(&dropdown, KeyCode::Enter);
        key(&dropdown, KeyCode::Down);
        key(&dropdown, KeyCode::Escape);
        assert_eq!(dropdown.state.lock().unwrap()["selected"], 0);
        key(&dropdown, KeyCode::Enter);
        key(&dropdown, KeyCode::Down);
        key(&dropdown, KeyCode::Down);
        let before = dropdown.transient.0.lock().unwrap().clone();
        for width in [28, 60, 100] {
            paint(&dropdown, width);
        }
        assert_eq!(*dropdown.transient.0.lock().unwrap(), before);
        let rebuilt = Gallery {
            id: dropdown.id.clone(),
            kind: dropdown.kind.clone(),
            state: Arc::clone(&dropdown.state),
            focused: true,
            transient: dropdown.transient.clone(),
        };
        key(&rebuilt, KeyCode::Enter);
        assert_eq!(dropdown.state.lock().unwrap()["selected"], 2);
        assert!(!paint(&rebuilt, 60).contains("Running"));
        let tree = example("tree-view");
        key(&tree, KeyCode::Left);
        assert_eq!(tree.state.lock().unwrap()["expanded"], false);
        assert!(!paint(&tree, 60).contains("main.rs"));
        key(&tree, KeyCode::Right);
        assert_eq!(tree.state.lock().unwrap()["expanded"], true);
        assert!(paint(&tree, 60).contains("main.rs"));
    }

    #[test]
    fn gallery_transient_map_is_bounded_namespaced_and_read_only_on_miss() {
        let local = GalleryTransient::new(None);
        let before = local.0.lock().unwrap().clone();
        assert!(local.snapshot("missing", "button").is_null());
        assert_eq!(*local.0.lock().unwrap(), before);
        for index in 0..300 {
            local.save(
                &format!("block-{index}"),
                "button",
                serde_json::json!({"pressed": true}),
            );
        }
        assert_eq!(local.0.lock().unwrap().as_object().unwrap().len(), 256);
        assert!(local.snapshot("block-0", "checkbox").is_null());
        local.save("block-0", "checkbox", serde_json::json!({"pressed": false}));
        assert!(local.snapshot("block-0", "button").is_null());
        assert_eq!(local.snapshot("block-0", "checkbox")["pressed"], false);
        let before = local.0.lock().unwrap().clone();
        local.save(
            "block-0",
            "checkbox",
            serde_json::json!({"unbounded": "x".repeat(16385)}),
        );
        assert_eq!(*local.0.lock().unwrap(), before);
    }

    #[test]
    fn gallery_press_capture_is_view_local_and_cancelled_by_focus_loss() {
        use bmux_tui::event::{FocusEvent, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        for kind in ["button", "checkbox", "select-dropdown"] {
            let a = example(kind);
            let b = Gallery {
                id: a.id.clone(),
                kind: a.kind.clone(),
                state: Arc::clone(&a.state),
                focused: true,
                transient: GalleryTransient::new(None),
            };
            let layout = a.layout(Constraints::for_width(60), &mut LayoutCx::default());
            let id = if kind == "button" {
                format!("{}.save", a.id)
            } else {
                a.id.clone()
            };
            let area = layout.find_rect(&id.into()).unwrap();
            let point = Point::new(area.x, area.y);
            let before = a.state.lock().unwrap().clone();
            let send = |control: &Gallery, event| {
                control.event(&event, &layout, &mut EventCx::new(&layout))
            };
            send(
                &a,
                Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    point,
                )),
            );
            send(
                &b,
                Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    point,
                )),
            );
            assert_eq!(
                *a.state.lock().unwrap(),
                before,
                "{kind}: another view cannot release a press"
            );
            send(&a, Event::Focus(FocusEvent::Lost));
            send(
                &a,
                Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    point,
                )),
            );
            assert_eq!(
                *a.state.lock().unwrap(),
                before,
                "{kind}: focus loss cancels press"
            );
            if kind == "select-dropdown" {
                assert!(!paint(&a, 60).contains("Running"));
            }
        }
    }

    #[test]
    fn gallery_dialog_native_actions_close_reopen_and_traverse_per_view() {
        let control = example("dialog");
        let tab = |shift| {
            let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
            let mut mods = bmux_keyboard::Modifiers::NONE;
            mods.shift = shift;
            control.event(
                &Event::Key(KeyStroke::with_modifiers(KeyCode::Tab, mods)),
                &layout,
                &mut EventCx::new(&layout),
            )
        };
        assert!(paint(&control, 60).contains("Save changes"));
        assert_eq!(tab(true), EventOutcome::Ignored);
        assert_eq!(tab(false), EventOutcome::Redraw);
        assert_eq!(tab(false), EventOutcome::Ignored);
        key(&control, KeyCode::Enter);
        assert_eq!(control.state.lock().unwrap()["dialog_status"], "Cancelled");
        assert!(paint(&control, 60).contains("Reopen / reset"));
        let other = Gallery {
            id: control.id.clone(),
            kind: control.kind.clone(),
            state: Arc::clone(&control.state),
            focused: true,
            transient: GalleryTransient::new(None),
        };
        assert!(
            paint(&other, 60).contains("Save changes"),
            "another view did not close"
        );
        let rebuilt = Gallery {
            id: control.id.clone(),
            kind: control.kind.clone(),
            state: Arc::clone(&control.state),
            focused: true,
            transient: control.transient.clone(),
        };
        key(&rebuilt, KeyCode::Enter);
        assert!(paint(&control, 60).contains("Save changes"));
        key(&control, KeyCode::Enter);
        assert_eq!(control.state.lock().unwrap()["dialog_status"], "Saved");
        key(&control, KeyCode::Enter);
        key(&control, KeyCode::Escape);
        assert_eq!(control.state.lock().unwrap()["dialog_status"], "Cancelled");
    }

    #[test]
    fn gallery_dialog_pointer_uses_committed_action_row_across_rebuild() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        let control = example("dialog");
        let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
        let area = layout
            .find_rect(&format!("{}.actions", control.id).into())
            .expect("native action row");
        let actions = [
            ActionButton::new("save", "Save"),
            ActionButton::new("cancel", "Cancel"),
        ];
        let cancel = ActionRow::new(&actions).action_areas(area)[1];
        let point = Point::new(cancel.x, cancel.y);
        let before = control.state.lock().unwrap().clone();
        control.event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                point,
            )),
            &layout,
            &mut EventCx::new(&layout),
        );
        let canonical = control.state.lock().unwrap().clone();
        let transient = control.transient.0.lock().unwrap().clone();
        paint(&control, 60);
        assert_eq!(*control.state.lock().unwrap(), canonical);
        assert_eq!(*control.transient.0.lock().unwrap(), transient);
        let rebuilt = Gallery {
            id: control.id.clone(),
            kind: control.kind.clone(),
            state: Arc::clone(&control.state),
            focused: true,
            transient: control.transient.clone(),
        };
        rebuilt.event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                point,
            )),
            &layout,
            &mut EventCx::new(&layout),
        );
        assert_eq!(control.state.lock().unwrap()["dialog_status"], "Cancelled");
        assert_ne!(*control.state.lock().unwrap(), before);
        assert!(paint(&rebuilt, 60).contains("Reopen / reset"));
    }

    #[test]
    fn gallery_dialog_focus_loss_cancels_press_without_shared_transients() {
        use bmux_tui::event::{FocusEvent, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        let control = example("dialog");
        let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
        let area = layout
            .find_rect(&format!("{}.actions", control.id).into())
            .unwrap();
        let point = Point::new(area.x, area.y);
        let before = control.state.lock().unwrap().clone();
        for event in [
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                point,
            )),
            Event::Focus(FocusEvent::Lost),
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                point,
            )),
        ] {
            control.event(&event, &layout, &mut EventCx::new(&layout));
        }
        assert_eq!(*control.state.lock().unwrap(), before);
        assert!(paint(&control, 60).contains("Save changes"));
    }

    #[test]
    fn gallery_dropdown_geometry_is_identical_open_closed_and_after_selection() {
        let control = example("select-dropdown");
        let measure = || control.layout(Constraints::for_width(60), &mut LayoutCx::default());
        let closed = measure();
        key(&control, KeyCode::Enter);
        assert_eq!(closed, measure());
        key(&control, KeyCode::Down);
        key(&control, KeyCode::Enter);
        assert_eq!(closed, measure());
        assert_eq!(control.state.lock().unwrap()["selected"], 1);
    }

    #[test]
    fn gallery_form_routes_one_field_traverses_and_yields_at_boundaries() {
        let control = example("form");
        let send = |shift| {
            let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
            let mut modifiers = bmux_keyboard::Modifiers::NONE;
            modifiers.shift = shift;
            control.event(
                &Event::Key(KeyStroke::with_modifiers(KeyCode::Tab, modifiers)),
                &layout,
                &mut EventCx::new(&layout),
            )
        };
        assert_eq!(send(true), EventOutcome::Ignored);
        key(&control, KeyCode::Char('x'));
        assert_eq!(control.state.lock().unwrap()["name"], "Bradenx");
        assert_eq!(control.state.lock().unwrap()["email"], "");
        assert_eq!(send(false), EventOutcome::Redraw);
        key(&control, KeyCode::Char('a'));
        assert_eq!(control.state.lock().unwrap()["email"], "a");
        assert_eq!(send(false), EventOutcome::Redraw);
        key(&control, KeyCode::Enter);
        assert_eq!(control.state.lock().unwrap()["status"], "Saved");
        assert_eq!(send(false), EventOutcome::Ignored);
        assert_eq!(send(true), EventOutcome::Redraw);
        let other = Gallery {
            id: control.id.clone(),
            kind: control.kind.clone(),
            state: Arc::clone(&control.state),
            focused: true,
            transient: GalleryTransient::new(None),
        };
        assert_eq!(
            other.transient.snapshot(&other.id, "form"),
            serde_json::Value::Null
        );
        key(&other, KeyCode::Char('z'));
        assert_eq!(control.state.lock().unwrap()["name"], "Bradenxz");
        assert_eq!(control.state.lock().unwrap()["email"], "a");
    }

    #[test]
    fn gallery_form_pointer_targets_email_and_save_without_broadcast() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        let control = example("form");
        let click = |suffix: &str| {
            let layout = control.layout(Constraints::for_width(60), &mut LayoutCx::default());
            let area = layout
                .find_rect(&format!("{}.{}", control.id, suffix).into())
                .unwrap();
            let point = Point::new(area.x, area.y);
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                control.event(
                    &Event::Mouse(MouseEvent::new(kind, point)),
                    &layout,
                    &mut EventCx::new(&layout),
                );
            }
        };
        click("email.input.input");
        key(&control, KeyCode::Char('a'));
        assert_eq!(control.state.lock().unwrap()["email"], "a");
        assert_eq!(control.state.lock().unwrap()["name"], "Braden");
        click("save");
        assert_eq!(control.state.lock().unwrap()["status"], "Saved");
        let before = control.state.lock().unwrap().clone();
        key(&control, KeyCode::Char('x'));
        assert_eq!(
            *control.state.lock().unwrap(),
            before,
            "Save focus must not broadcast typing"
        );
    }

    #[test]
    fn gallery_scroll_examples_use_native_keyboard_wheel_and_rebuild_offsets() {
        use bmux_tui::event::{MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        for kind in ["scroll-view", "virtual-list"] {
            let control = example(kind);
            let before = paint(&control, 60);
            key(&control, KeyCode::PageDown);
            assert_ne!(paint(&control, 60), before, "{kind}");
            let rebuilt = Gallery {
                id: control.id.clone(),
                kind: control.kind.clone(),
                state: Arc::clone(&control.state),
                focused: true,
                transient: control.transient.clone(),
            };
            assert_eq!(paint(&control, 60), paint(&rebuilt, 60));
            let layout = rebuilt.layout(Constraints::for_width(60), &mut LayoutCx::default());
            let old = rebuilt.transient.snapshot(&rebuilt.id, kind)["offset"]
                .as_u64()
                .unwrap();
            let result = rebuilt.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::ScrollDown,
                    Point::new(1, 2),
                )),
                &layout,
                &mut EventCx::new(&layout),
            );
            assert_eq!(result, EventOutcome::Redraw);
            assert!(
                rebuilt.transient.snapshot(&rebuilt.id, kind)["offset"]
                    .as_u64()
                    .unwrap()
                    > old
            );
            let other = Gallery {
                id: control.id.clone(),
                kind: control.kind.clone(),
                state: Arc::clone(&control.state),
                focused: true,
                transient: GalleryTransient::new(None),
            };
            assert_eq!(paint(&other, 60), before);
        }
    }

    #[test]
    fn gallery_context_build_reuses_only_the_supplied_view_handle() {
        let block = block("select-dropdown");
        let canonical = Arc::new(Mutex::new(GalleryFactory.initial_state(&block).unwrap()));
        let local = Arc::new(Mutex::new(serde_json::json!({})));
        let build = || {
            GalleryFactory
                .build_with_context(
                    &block,
                    Arc::clone(&canonical),
                    PresentationBuildContext {
                        focused: true,
                        interaction: Some(Arc::clone(&local)),
                    },
                )
                .unwrap()
        };
        key(&build(), KeyCode::Enter);
        assert!(paint(&build(), 60).contains("Running"));
        let other = GalleryFactory
            .build_with_context(
                &block,
                Arc::clone(&canonical),
                PresentationBuildContext {
                    focused: true,
                    interaction: None,
                },
            )
            .unwrap();
        assert!(!paint(&other, 60).contains("Running"));
        assert!(
            !paint(
                &GalleryFactory
                    .build(&block, Arc::clone(&canonical))
                    .unwrap(),
                60
            )
            .contains("Running")
        );
        assert_eq!(
            *canonical.lock().unwrap(),
            serde_json::json!({"selected": 0, "checked": false})
        );
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
            focused: true,
            transient: input.transient.clone(),
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
                            focused: false,
                            transient: GalleryTransient::new(None),
                        })
                    }),
                )
                .unwrap();
            // Long walkthroughs intentionally overflow. Exercise the same viewer
            // and committed geometry used by the player instead of requiring fit.
            let navigation = bmux_presenter::PresentationState {
                slide_id: Some(slide.id.clone()),
                reveal_step: 0,
            };
            let state = bmux_presenter::ViewerState::new();
            let viewer =
                bmux_presenter::ViewerComponent::new(&deck, &navigation, &state, &theme, &registry);
            let layout = viewer.layout(
                Constraints::new(0, 80, 0, Some(10)),
                &mut LayoutCx::default(),
            );
            let viewport = layout
                .find(&format!("presentation/{}/viewer/viewport/content", deck.id.as_str()).into())
                .expect("viewer content viewport");
            let actual_area = viewport
                .find_logical_rect(
                    &format!("presentation/{}/{}", slide.id.as_str(), block.id.as_str()).into(),
                )
                .expect("gallery in committed viewer");
            let maximum = ScrollView::max_vertical_offset(viewport);
            let _ = viewer.event(
                &Event::Key(KeyStroke::simple(KeyCode::End)),
                &layout,
                &mut EventCx::new(&layout),
            );
            // End verifies the overflow route, but the gallery may precede a
            // long walkthrough. Reveal its actual committed position rather than
            // incorrectly requiring the example to be visible at the slide end.
            let mut scroll = state.scroll.get();
            let desired = if kind(block).unwrap() == "form" {
                // Two bordered fields plus Save/status exceed this tiny viewport.
                // Reveal the trailing validation status; other controls reveal the top.
                actual_area
                    .y
                    .saturating_add(actual_area.height)
                    .saturating_sub(viewport.size.height)
            } else {
                actual_area.y
            };
            scroll.set_vertical_offset(desired.min(maximum));
            state.scroll.set(scroll);
            // Paint exactly the geometry used by End/reveal. The generic helper
            // uses 16 rows and would reconcile this 10-row viewport's offset away.
            let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
            viewer.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
            let output = (0..10)
                .filter_map(|row| buffer.row_symbols(row))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                state.scroll.get().vertical_offset(),
                desired.min(maximum),
                "{}: committed offset",
                slide.id.as_str()
            );
            if actual_area.y + actual_area.height > viewport.size.height && maximum > 0 {
                assert!(
                    state.scroll.get().vertical_offset() > 0,
                    "{}: overflow must scroll",
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
