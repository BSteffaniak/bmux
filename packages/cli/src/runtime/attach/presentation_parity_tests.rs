//! Golden full-frame parity harness for every presentation path.
//!
//! Each scenario renders fixed inputs through the real attach frame renderer,
//! replays the emitted bytes into a terminal grid, and compares a dump of every
//! visible cell's text and style with a checked-in golden under
//! `packages/cli/tests/fixtures/presentation-parity/`. The unified presentation
//! migration (docs/development/unified-presentation-plan.md) must keep these
//! byte-for-byte stable, except in commits that explain a deliberate change.
//!
//! Set `BMUX_UPDATE_PARITY_GOLDENS=1` to rewrite the goldens.

use super::*;
use crate::runtime::attach::state::{AttachViewState, PaneRenderBuffer};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind, PaneLayoutNode,
};
use bmux_attach_pipeline::render::append_pane_output;
use bmux_client::{AttachLayoutState, AttachOpenInfo};
use bmux_plugin::AttachRenderExtension;
use bmux_plugin::layout::{LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutSnapshot};
use bmux_plugin::surface::{PluginSurface, PluginSurfaceId, PluginSurfaceSnapshot};
use bmux_plugin::{BorderGlyphs, RenderCell, RenderNamedColor, RenderOp, RenderStyle};
use serial_test::serial;
use std::fmt::Write as _;

const COLS: u16 = 40;
const ROWS: u16 = 12;

/// One attach view with a single pane and its own presentation registries.
struct Harness {
    view: AttachViewState,
    layout: AttachLayoutState,
    pane_id: Uuid,
}

impl Harness {
    /// A pane covering `pane_rect` whose terminal shows `pane_output`.
    fn new(pane_rect: AttachRect, pane_output: &[u8]) -> Self {
        let session_id = Uuid::from_u128(0x5e55);
        let pane_id = Uuid::from_u128(0x9a1e);
        let layout = AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: Uuid::from_u128(0x5aff),
                    kind: AttachSurfaceKind::Pane,
                    layer: SurfaceLayer::Pane,
                    z: 0,
                    rect: pane_rect,
                    content_rect: pane_rect,
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: Some(pane_id),
                }],
            },
            zoomed: false,
        };
        let mut view = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view.presentation_layouts =
            std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(64));
        view.presentation_surfaces =
            std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(64));
        view.presentation_extensions =
            std::sync::Arc::new(bmux_plugin::RenderExtensionRegistry::new());
        let buffer = view
            .pane_buffers
            .entry(pane_id)
            .or_insert_with(|| PaneRenderBuffer {
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    pane_rect.w,
                    pane_rect.h,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("test grid dimensions are valid"),
                ..PaneRenderBuffer::default()
            });
        append_pane_output(buffer, pane_output);
        Self {
            view,
            layout,
            pane_id,
        }
    }

    fn layouts(&self, owner: &str, requests: Vec<PluginLayoutRequest>) {
        self.view
            .presentation_layouts
            .publish(
                owner,
                PluginLayoutSnapshot {
                    revision: 1,
                    requests,
                },
            )
            .expect("publish test layouts");
    }

    fn surfaces(&self, owner: &str, surfaces: Vec<PluginSurface>) {
        self.view
            .presentation_surfaces
            .publish(
                owner,
                PluginSurfaceSnapshot {
                    revision: 1,
                    surfaces,
                },
            )
            .expect("publish test surfaces");
    }

    fn extension(&self, extension: Arc<dyn AttachRenderExtension>) {
        self.view.presentation_extensions.register(extension);
    }

    /// Render one full frame and dump the resulting screen.
    fn render(&mut self) -> String {
        self.view
            .dirty
            .mark_retained_surfaces_dirty(AttachDirtySource::SceneChanged);
        self.view
            .dirty
            .mark_pane_dirty(self.pane_id, AttachDirtySource::PaneOutput);
        let mut output = Vec::new();
        render_attach_frame_to_writer(
            &mut output,
            &mut self.view,
            &self.layout,
            &RuntimeAppearance::default(),
            &[],
            0,
            &bmux_config::DamageBehaviorConfig::default(),
            u64::MAX,
            &mut DisplayCaptureFanout::default(),
            TerminalGeometry {
                cols: COLS,
                rows: ROWS,
            },
            None,
        )
        .expect("render parity frame");
        let mut screen = bmux_terminal_grid::TerminalGridStream::new(
            COLS,
            ROWS,
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("screen dimensions are valid");
        screen.process(&output);
        dump_screen(screen.grid())
    }
}

/// Text rows, then one line per run of identically styled cells.
fn dump_screen(grid: &bmux_terminal_grid::TerminalGrid) -> String {
    let width = grid.width();
    let rows = grid.display_rows(0, grid.height());
    let mut dump = String::new();
    for (index, row) in rows.iter().enumerate() {
        let _ = writeln!(
            dump,
            "{index:02}|{}|",
            bmux_terminal_grid::row_text(row, width)
        );
    }
    let palette = grid.palette();
    for (index, row) in rows.iter().enumerate() {
        let mut run: Option<(usize, String)> = None;
        let flush = |dump: &mut String, run: &mut Option<(usize, String)>, end: usize| {
            if let Some((start, style)) = run.take()
                && style != "default"
            {
                let _ = writeln!(dump, "{index:02} {start:02}-{:02} {style}", end - 1);
            }
        };
        for col in 0..width {
            let style = row.cells().get(col).map_or_else(
                || "default".to_string(),
                |cell| describe_style(&palette.get(cell.style())),
            );
            if run.as_ref().is_some_and(|(_, current)| *current == style) {
                continue;
            }
            flush(&mut dump, &mut run, col);
            run = Some((col, style));
        }
        flush(&mut dump, &mut run, width);
    }
    dump
}

fn describe_style(style: &bmux_terminal_grid::Style) -> String {
    let color = |color: Option<bmux_terminal_grid::Color>| match color {
        None => "-".to_string(),
        Some(bmux_terminal_grid::Color::Indexed(index)) => format!("{index}"),
        Some(bmux_terminal_grid::Color::Rgb { r, g, b }) => format!("#{r:02x}{g:02x}{b:02x}"),
    };
    let mut flags = String::new();
    for (set, flag) in [
        (style.bold, 'b'),
        (style.italic, 'i'),
        (style.underline, 'u'),
        (style.inverse, 'r'),
        (style.dim, 'd'),
        (style.strike, 's'),
        (style.hidden, 'h'),
    ] {
        if set {
            flags.push(flag);
        }
    }
    if style.fg.is_none() && style.bg.is_none() && flags.is_empty() {
        return "default".to_string();
    }
    format!("fg={} bg={} {flags}", color(style.fg), color(style.bg))
        .trim_end()
        .to_string()
}

fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/presentation-parity")
        .join(format!("{name}.txt"));
    if std::env::var_os("BMUX_UPDATE_PARITY_GOLDENS").is_some() {
        std::fs::create_dir_all(path.parent().expect("golden directory"))
            .expect("create golden directory");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing golden {} ({error}); run with BMUX_UPDATE_PARITY_GOLDENS=1",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "presentation parity golden {name} changed\n--- expected\n{expected}\n--- actual\n{actual}"
    );
}

/// A surface at a fixed viewport rect.
fn explicit(
    id: PluginSurfaceId,
    revision: u64,
    rect: ExtensionRect,
    ops: Vec<RenderOp>,
) -> PluginSurface {
    PluginSurface {
        target: bmux_plugin::surface::PluginSurfaceTarget::Explicit(rect),
        ..PluginSurface::layout(id, revision, PluginLayoutId::new("unused", "unused"), ops)
    }
}

fn scene_op(key: &str, z: i16, op: RenderOp) -> bmux_plugin::RenderSceneItem {
    bmux_plugin::RenderSceneItem::op(bmux_plugin::RenderSceneItemKey::new(key), z, op)
}

fn full_pane() -> AttachRect {
    AttachRect {
        x: 0,
        y: 0,
        w: COLS,
        h: ROWS,
    }
}

fn styled(fg: RenderNamedColor, bg: RenderNamedColor) -> RenderStyle {
    RenderStyle::new().named_foreground(fg).named_background(bg)
}

#[test]
#[serial]
fn parity_sidebar_shaped_split_surface() {
    const OWNER: &str = "parity.sidebar";
    let mut harness = Harness::new(
        AttachRect {
            x: 14,
            y: 0,
            w: COLS - 14,
            h: ROWS,
        },
        b"pane output\r\nsecond line",
    );
    let id = PluginLayoutId::new(OWNER, "side");
    harness.layouts(
        OWNER,
        vec![PluginLayoutRequest::split(
            id.clone(),
            0,
            LayoutEdge::Left,
            LayoutExtent::Cells(14),
        )],
    );
    let surface = PluginSurface::layout(
        PluginSurfaceId::new(OWNER, "side", Uuid::from_u128(0xa1)),
        1,
        id,
        vec![
            RenderOp::fill_rect(
                ExtensionRect::new(0, 0, 14, ROWS),
                ' ',
                styled(RenderNamedColor::White, RenderNamedColor::Blue),
            ),
            RenderOp::border(
                ExtensionRect::new(0, 0, 14, 6),
                BorderGlyphs::square(),
                RenderStyle::new().named_foreground(RenderNamedColor::Cyan),
            ),
            RenderOp::text_run(1, 0, " Tabs ", RenderStyle::new().bold()),
            RenderOp::text_run(
                1,
                1,
                "> 1 main",
                styled(RenderNamedColor::Black, RenderNamedColor::Green),
            ),
            RenderOp::text_run(1, 2, "  2 logs", RenderStyle::new()),
        ],
    )
    .opaque(true);
    harness.surfaces(OWNER, vec![surface]);
    assert_golden("sidebar_split", &harness.render());
}

#[test]
#[serial]
fn parity_tab_bar_strip_menu_and_toast() {
    const OWNER: &str = "parity.tab-bar";
    let mut harness = Harness::new(
        AttachRect {
            x: 0,
            y: 0,
            w: COLS,
            h: ROWS - 1,
        },
        b"$ ls\r\nCargo.toml  src",
    );
    let id = PluginLayoutId::new(OWNER, "strip");
    harness.layouts(
        OWNER,
        vec![PluginLayoutRequest::split(
            id.clone(),
            0,
            LayoutEdge::Bottom,
            LayoutExtent::Cells(1),
        )],
    );
    let strip = PluginSurface::layout(
        PluginSurfaceId::new(OWNER, "strip", Uuid::from_u128(0xb1)),
        1,
        id,
        vec![
            RenderOp::fill_rect(
                ExtensionRect::new(0, 0, COLS, 1),
                ' ',
                styled(RenderNamedColor::Black, RenderNamedColor::White),
            ),
            RenderOp::text_run(
                0,
                0,
                " 1:main ",
                styled(RenderNamedColor::White, RenderNamedColor::Blue).bold(),
            ),
            RenderOp::text_run(9, 0, "2:logs", RenderStyle::new()),
        ],
    )
    .opaque(true);
    let backdrop = explicit(
        PluginSurfaceId::new(OWNER, "menu-dismiss", Uuid::from_u128(0xb2)),
        1,
        ExtensionRect::new(0, 0, COLS, ROWS),
        Vec::new(),
    )
    .modal(true)
    .order(100, 0);
    let menu = explicit(
        PluginSurfaceId::new(OWNER, "menu", Uuid::from_u128(0xb3)),
        1,
        ExtensionRect::new(1, 7, 12, 4),
        vec![
            RenderOp::border(
                ExtensionRect::new(0, 0, 12, 4),
                BorderGlyphs::rounded(),
                RenderStyle::new(),
            ),
            RenderOp::text_run(1, 1, "Rename", RenderStyle::new().reverse()),
            RenderOp::text_run(1, 2, "Close", RenderStyle::new()),
        ],
    )
    .opaque(true)
    .modal(true)
    .order(100, 1);
    let toast = explicit(
        PluginSurfaceId::new(OWNER, "toast", Uuid::from_u128(0xb4)),
        1,
        ExtensionRect::new(24, 0, 16, 1),
        vec![RenderOp::text_run(
            0,
            0,
            " build finished ",
            styled(RenderNamedColor::Black, RenderNamedColor::Yellow),
        )],
    )
    .opaque(true)
    .order(100, 2);
    harness.surfaces(OWNER, vec![strip, backdrop, menu, toast]);
    assert_golden("tab_bar_strip_menu_toast", &harness.render());
}

#[test]
#[serial]
fn parity_overlapping_surface_order() {
    // Two owners whose surfaces overlap. The surface with the higher z must be
    // the one visible where they overlap.
    let mut harness = Harness::new(full_pane(), b"");
    let low = explicit(
        PluginSurfaceId::new("parity.low", "low", Uuid::from_u128(0xc2)),
        1,
        ExtensionRect::new(2, 2, 10, 3),
        vec![RenderOp::fill_rect(
            ExtensionRect::new(0, 0, 10, 3),
            'L',
            styled(RenderNamedColor::White, RenderNamedColor::Red),
        )],
    )
    .opaque(true)
    .order(10, 1);
    let high = explicit(
        PluginSurfaceId::new("parity.high", "high", Uuid::from_u128(0xc1)),
        1,
        ExtensionRect::new(6, 3, 10, 3),
        vec![RenderOp::fill_rect(
            ExtensionRect::new(0, 0, 10, 3),
            'H',
            styled(RenderNamedColor::White, RenderNamedColor::Green),
        )],
    )
    .opaque(true)
    .order(10, 5);
    harness.surfaces("parity.low", vec![low]);
    harness.surfaces("parity.high", vec![high]);
    assert_golden("overlapping_order", &harness.render());
}

/// Paints fixed under-content and after-content scenes on every pane surface.
struct ParityPaneExtension;

impl AttachRenderExtension for ParityPaneExtension {
    fn name(&self) -> &'static str {
        "parity.pane-layers"
    }

    fn render_layer_scene_with_context(
        &self,
        _surface_id: Uuid,
        surface_rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<bmux_plugin::RenderLayerScene> {
        if context.surface_role != bmux_plugin::RenderSurfaceRole::Content {
            return None;
        }
        let x = surface_rect.x;
        let y = surface_rect.y;
        let scene = match layer {
            RenderExtensionLayer::BeforePaneContent => bmux_plugin::RenderLayerScene::builder()
                .item(scene_op(
                    "under",
                    0,
                    RenderOp::text_run(
                        x,
                        y.saturating_add(1),
                        "UNDERNEATH-UNDERNEATH",
                        RenderStyle::new().named_foreground(RenderNamedColor::Magenta),
                    ),
                ))
                .build(),
            RenderExtensionLayer::AfterPaneContent => bmux_plugin::RenderLayerScene::builder()
                .item(scene_op(
                    "sparse",
                    0,
                    RenderOp::cell_grid(
                        x.saturating_add(2),
                        y.saturating_add(4),
                        vec![vec![
                            RenderCell::new('#', RenderStyle::new().bold()),
                            RenderCell::sparse(RenderStyle::new()),
                            RenderCell::new('#', RenderStyle::new().bold()),
                        ]],
                    ),
                ))
                .item(scene_op(
                    "label",
                    1,
                    RenderOp::text_run(
                        x.saturating_add(20),
                        y.saturating_add(6),
                        "over",
                        styled(RenderNamedColor::Black, RenderNamedColor::Cyan),
                    ),
                ))
                .build(),
        };
        Some(scene)
    }
}

#[test]
#[serial]
fn parity_pane_extension_under_and_after_content() {
    // Row 1: text, then blanks with a background colour (background-only cells),
    // then default blanks (transparent cells). Row 4 carries the sparse grid.
    let mut harness = Harness::new(
        full_pane(),
        b"top row\r\nab\x1b[44m    \x1b[0m\r\n\r\n\r\nXYZXYZ",
    );
    harness.extension(Arc::new(ParityPaneExtension));
    assert_golden("pane_extension_layers", &harness.render());
}

#[test]
#[serial]
fn parity_host_notification_row() {
    let mut harness = Harness::new(full_pane(), b"pane");
    super::super::local_presentation::publish_notification(
        &harness.view.presentation_layouts,
        &harness.view.presentation_surfaces,
        Some("saved layout"),
        COLS,
        ROWS,
    );
    assert_golden("host_notification", &harness.render());
}

#[test]
#[serial]
fn parity_cell_block_graphemes_transparency_and_clip() {
    use bmux_presentation_scene::scene::{CellBlock, SceneCell};
    const OWNER: &str = "parity.cells";
    let mut harness = Harness::new(full_pane(), b"0123456789abcdefghij\r\nKLMNOPQRSTUVWXYZ");
    // Row 0: a wide emoji, a combining cluster, then transparent cells that
    // keep the pane's text. Row 1: opaque cells.
    let mut block = CellBlock::transparent(0, 0, 8, 2);
    let style = styled(RenderNamedColor::Black, RenderNamedColor::Yellow);
    block.set(0, 0, SceneCell::cluster("👍🏽", style));
    block.set(2, 0, SceneCell::cluster("e\u{301}", style));
    for column in 0..8 {
        block.set(column, 1, SceneCell::new('=', RenderStyle::new().bold()));
    }
    let mut surface = explicit(
        PluginSurfaceId::new(OWNER, "cells", Uuid::from_u128(0xd1)),
        1,
        ExtensionRect::new(4, 0, 8, 2),
        vec![RenderOp::cells(block)],
    )
    .order(10, 0);
    // Clip away the last two columns of the surface.
    surface.clip_rect = Some(ExtensionRect::new(0, 0, 6, 2));
    harness.surfaces(OWNER, vec![surface]);
    assert_golden("cell_block_graphemes", &harness.render());
}
