//! Round-trip smoke tests for the scene-protocol BPDL types.

use bmux_scene_protocol::scene_protocol::{
    Color, DecorationScene, INTERFACE_ID, PaintCommand, Rect, Style, SurfaceDecoration,
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
fn decoration_scene_round_trips_through_json() {
    let pane = Uuid::from_u128(0xab_cd_ef);
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
        paint_commands: vec![PaintCommand {
            col: 0,
            row: 0,
            text: "+--".to_string(),
            style: Style {
                fg: Some(Color::White),
                bg: None,
                bold: false,
                underline: false,
                italic: false,
                reverse: false,
            },
        }],
    };

    let mut surfaces = BTreeMap::new();
    surfaces.insert(pane, surface.clone());
    let scene = DecorationScene {
        revision: 7,
        surfaces,
        fallback: None,
    };

    let json = serde_json::to_string(&scene).expect("scene encodes");
    let decoded: DecorationScene = serde_json::from_str(&json).expect("scene decodes");
    assert_eq!(decoded.revision, 7);
    assert_eq!(decoded.surfaces.get(&pane), Some(&surface));
}

#[test]
fn paint_command_style_default_flags_are_false() {
    let cmd = PaintCommand {
        col: 2,
        row: 3,
        text: "ok".to_string(),
        style: Style {
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            reverse: false,
        },
    };
    assert!(!cmd.style.bold);
    assert!(!cmd.style.underline);
    assert!(cmd.style.fg.is_none());
    assert!(cmd.style.bg.is_none());
}
