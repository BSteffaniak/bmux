//! Product-neutral caller-owned custom component. Native main is a focused smoke
//! test; the same source exports a browser-local WASM application.
use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutNode, LogicalSize},
    damage::Damage,
    event::Event,
    geometry::{Point, Rect},
    hit::{HitId, HitRegion},
    paint::{LocalRect, PaintCx},
    presentation::PresentationLimits,
    selection::{SelectionController, SelectionScope, plain_text_fragments},
    semantic::{SemanticAccessibility, SemanticRegion},
    style::{Color, Style},
    text::Line,
};
use bmux_web_renderer::{LocalSurface, input::BrowserInput};

const CONTENT: &str = "Drag to select this caller-owned source text.";
const ID: &str = "counter:increment";

struct Counter(u32, String, u64);
impl Component for Counter {
    fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            "counter".into(),
            constraints.constrain(LogicalSize::new(52, 6)),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = layout.terminal_rect(0, 0);
        cx.fill(
            LocalRect::terminal(area),
            " ",
            Style::new().bg(Color::Black),
        );
        let button = Rect::new(0, 0, area.width.min(28), area.height.min(1));
        cx.write_line(
            LocalRect::terminal(button),
            &Line::raw(format!("[ Increment: {} ]", self.0)),
        );
        cx.push_hit(HitRegion::new(ID, button).focusable(true));
        let mut semantic = SemanticRegion::new(ID, button, "button");
        semantic.accessibility = Some(SemanticAccessibility {
            label: Some(format!("Increment: {}", self.0)),
            ..SemanticAccessibility::default()
        });
        cx.push_semantic(semantic);
        let text = Rect::new(0, 2, area.width, 1);
        cx.write_line(LocalRect::terminal(text), &Line::raw(self.1.as_str()));
        cx.push_hit(HitRegion::new("editor", text).focusable(true));
        let mut editor = SemanticRegion::new("editor", text, "textbox");
        editor.accessibility = Some(SemanticAccessibility {
            label: Some("Source editor".into()),
            text: Some(self.1.clone()),
            ..SemanticAccessibility::default()
        });
        cx.push_semantic(editor);
        cx.push_selection_scope(SelectionScope::new("source", text));
        for fragment in plain_text_fragments("source", "source", text, 0, &self.1, 0, self.2) {
            cx.push_selection_fragment(fragment);
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct CustomComponent {
    surface: LocalSurface,
    counter: Counter,
    selection: SelectionController,
    #[cfg(target_arch = "wasm32")]
    canvas: bmux_web_renderer::CanvasRenderer,
}

fn surface() -> LocalSurface {
    LocalSurface::new(
        Rect::new(0, 0, 52, 6),
        PresentationLimits {
            max_cells: 8192,
            max_entries: 4096,
            max_bytes: 1_048_576,
        },
    )
}

impl CustomComponent {
    fn dispatch(&mut self, json: &str) -> Result<(), Box<dyn std::error::Error>> {
        let input = BrowserInput::decode(json, 8192)?.normalize(4096, |x, y| {
            // JS supplies cells from committed output, not a newly requested layout.
            if x < 0.0 || y < 0.0 || x > 255.0 || y > 63.0 {
                return None;
            }
            Some(Point::new(x as u16, y as u16))
        })?;
        if input.cancel_pointer {
            self.selection.clear();
        }
        for route in self.surface.route_input(input) {
            if let Event::Mouse(mouse) = &route.event {
                self.selection
                    .handle_mouse(self.surface.presentation().selection(), *mouse);
            }
            if route.targets(&HitId::new("editor")) {
                let changed = match &route.event {
                    Event::Paste(text) if self.counter.1.len() + text.len() <= 4096 => {
                        self.counter.1.push_str(text);
                        true
                    }
                    Event::Key(key) if key.key == bmux_keyboard::KeyCode::Backspace => {
                        self.counter.1.pop().is_some()
                    }
                    _ => false,
                };
                if changed {
                    self.counter.2 = self.counter.2.saturating_add(1);
                    self.selection.clear();
                }
            }
            if route.targets(&HitId::new(ID)) && route.is_activation() {
                self.counter.0 = self.counter.0.saturating_add(1);
            }
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
impl CustomComponent {
    #[wasm_bindgen::prelude::wasm_bindgen(constructor)]
    pub fn new(canvas: web_sys::HtmlCanvasElement) -> Result<Self, wasm_bindgen::JsValue> {
        Ok(Self {
            surface: surface(),
            counter: Counter(0, CONTENT.into(), 1),
            selection: SelectionController::new(),
            canvas: bmux_web_renderer::CanvasRenderer::from_canvas(canvas, 10.0, 20.0)?,
        })
    }
    pub fn input(&mut self, json: &str) -> Result<(), wasm_bindgen::JsValue> {
        self.dispatch(json)
            .map_err(|e| wasm_bindgen::JsValue::from_str(&e.to_string()))
    }
    pub async fn present(
        &mut self,
        columns: u16,
        rows: u16,
        dpr: f64,
    ) -> Result<String, wasm_bindgen::JsValue> {
        if columns > 128 || rows > 64 || !dpr.is_finite() || !(0.5..=4.0).contains(&dpr) {
            return Err(wasm_bindgen::JsValue::from_str(
                "example viewport budget exceeded",
            ));
        }
        self.canvas.set_metrics(
            10.0 * dpr,
            20.0 * dpr,
            format!("{}px monospace", 16.0 * dpr),
            16.0 * dpr,
        )?;
        self.surface.resize(Rect::new(0, 0, columns, rows));
        let layout = self.counter.layout(
            Constraints::new(0, columns.into(), 0, Some(rows.into())),
            &mut LayoutCx::new(),
        );
        let counter = &self.counter;
        let selection = &self.selection;
        self.surface
            .present_canvas(&mut self.canvas, Damage::Full, |cx| {
                counter.paint(&layout, cx);
                if let Some(snapshot) = selection.snapshot(cx.selection()) {
                    cx.paint_selection(&snapshot, Style::new().bg(Color::Blue));
                }
            })
            .await?;
        self.selection
            .reconcile(self.surface.presentation().selection());
        serde_json::to_string(self.surface.presentation().semantics())
            .map_err(|e| wasm_bindgen::JsValue::from_str(&e.to_string()))
    }
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = selectedText)]
    pub fn selected_text(&self) -> String {
        let Some(snapshot) = self
            .selection
            .snapshot(self.surface.presentation().selection())
        else {
            return String::new();
        };
        snapshot
            .slices
            .iter()
            .filter_map(|slice| self.counter.1.get(slice.source_range.clone()))
            .collect()
    }
}

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut app = CustomComponent {
            surface: surface(),
            counter: Counter(0, CONTENT.into(), 1),
            selection: SelectionController::new(),
        };
        let layout = app
            .counter
            .layout(Constraints::new(0, 52, 0, Some(6)), &mut LayoutCx::new());
        app.surface
            .present(
                Damage::Full,
                |cx| app.counter.paint(&layout, cx),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        app.dispatch(r#"{"type":"key","key":"Enter","code":"Enter"}"#)
            .unwrap();
        assert_eq!(app.counter.0, 1);
        println!("Custom component: committed-scene keyboard activation passed");
    }
}
