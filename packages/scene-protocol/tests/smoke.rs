//! Round-trip smoke tests for the scene-protocol BPDL types.
//!
//! These also act as the primary coverage for the additively-widened
//! color / glyph / paint-command vocabulary introduced in the scene
//! protocol v1.1 refresh.

use bmux_scene_protocol::scene_protocol::{
    AnimationHint, BorderGlyphs, Cell, Color, DecorationScene, GradientAxis, INTERFACE_ID,
    NamedColor, PaintCommand, Rect, Style, SurfaceDecoration,
};
use std::collections::BTreeMap;
use uuid::Uuid;

#[test]
fn interface_id_matches_bpdl_source() {
    assert_eq!(INTERFACE_ID, "scene-protocol");
}

#[test]
fn color_default_is_terminal_default() {
    // BPDL marks `default` as `@default`; this must survive codegen.
    assert_eq!(Color::default(), Color::Default);
}

#[test]
fn named_color_default_is_white() {
    // Explicit default on the named palette enum so round-tripped
    // records with a missing field don't accidentally resolve to
    // whatever `enum::default()` normally picks.
    assert_eq!(NamedColor::default(), NamedColor::White);
}

#[test]
fn border_glyphs_default_is_ascii() {
    assert_eq!(BorderGlyphs::default(), BorderGlyphs::Ascii);
}

#[test]
fn gradient_axis_default_is_horizontal() {
    assert_eq!(GradientAxis::default(), GradientAxis::Horizontal);
}

fn default_style() -> Style {
    Style {
        fg: None,
        bg: None,
        bold: false,
        underline: false,
        italic: false,
        reverse: false,
        dim: false,
        blink: false,
        strikethrough: false,
    }
}

#[test]
fn decoration_scene_round_trips_through_json() {
    let pane = Uuid::from_u128(0xab_cd_ef);
    let mut style = default_style();
    style.fg = Some(Color::Named {
        name: NamedColor::White,
    });
    let surface = SurfaceDecoration {
        surface_id: pane,
        rect: Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        },
        content_rect: Rect {
            x: 1,
            y: 1,
            w: 18,
            h: 8,
        },
        paint_commands: vec![PaintCommand::Text {
            col: 0,
            row: 0,
            z: 0,
            text: "+--".to_string(),
            style,
        }],
        interactive_regions: Vec::new(),
    };

    let mut surfaces = BTreeMap::new();
    surfaces.insert(pane, surface.clone());
    let scene = DecorationScene {
        revision: 7,
        surfaces,
        animation: None,
    };

    let json = serde_json::to_string(&scene).expect("scene encodes");
    let decoded: DecorationScene = serde_json::from_str(&json).expect("scene decodes");
    assert_eq!(decoded.revision, 7);
    assert_eq!(decoded.surfaces.get(&pane), Some(&surface));
}

#[test]
fn paint_command_style_default_flags_are_false() {
    let style = default_style();
    assert!(!style.bold);
    assert!(!style.underline);
    assert!(!style.dim);
    assert!(!style.blink);
    assert!(!style.strikethrough);
    assert!(style.fg.is_none());
    assert!(style.bg.is_none());
}

#[test]
fn color_variants_round_trip_through_json() {
    let cases = [
        Color::Default,
        Color::Reset,
        Color::Named {
            name: NamedColor::BrightMagenta,
        },
        Color::Indexed { index: 214 },
        Color::Rgb {
            r: 57,
            g: 255,
            b: 20,
        },
    ];
    for c in cases {
        let json = serde_json::to_string(&c).expect("encode");
        let round: Color = serde_json::from_str(&json).expect("decode");
        assert_eq!(round, c);
    }
}

#[test]
fn paint_command_variants_round_trip_through_json() {
    let style = default_style();
    let variants = [
        PaintCommand::Text {
            col: 0,
            row: 0,
            z: 0,
            text: "hi".to_string(),
            style: style.clone(),
        },
        PaintCommand::FilledRect {
            rect: Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 2,
            },
            z: 1,
            glyph: "#".to_string(),
            style: style.clone(),
        },
        PaintCommand::GradientRun {
            col: 2,
            row: 3,
            z: 0,
            text: "fade".to_string(),
            axis: GradientAxis::Horizontal,
            from_style: style.clone(),
            to_style: style.clone(),
        },
        PaintCommand::CellGrid {
            origin_col: 0,
            origin_row: 0,
            z: 0,
            cols: 2,
            cells: vec![
                Cell {
                    glyph: "A".to_string(),
                    style: style.clone(),
                },
                Cell {
                    glyph: "B".to_string(),
                    style: style.clone(),
                },
            ],
        },
        PaintCommand::BoxBorder {
            rect: Rect {
                x: 0,
                y: 0,
                w: 20,
                h: 5,
            },
            z: 10,
            glyphs: BorderGlyphs::Rounded,
            style: style.clone(),
        },
    ];

    for v in variants {
        let json = serde_json::to_string(&v).expect("encode");
        let round: PaintCommand = serde_json::from_str(&json).expect("decode");
        assert_eq!(round, v);
    }
}

#[test]
fn border_glyphs_custom_carries_six_runes() {
    let custom = BorderGlyphs::Custom {
        top_left: "╭".to_string(),
        top_right: "╮".to_string(),
        bottom_left: "╰".to_string(),
        bottom_right: "╯".to_string(),
        horizontal: "─".to_string(),
        vertical: "│".to_string(),
    };
    let json = serde_json::to_string(&custom).expect("encode");
    let round: BorderGlyphs = serde_json::from_str(&json).expect("decode");
    assert_eq!(round, custom);
}

#[test]
fn animation_hint_round_trips() {
    let hint = AnimationHint { target_hz: 30 };
    let json = serde_json::to_string(&hint).expect("encode");
    let round: AnimationHint = serde_json::from_str(&json).expect("decode");
    assert_eq!(round, hint);
}
