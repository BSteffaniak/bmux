//! Behavior of lazily materialized selection text runs.

use super::*;
use crate::buffer::Buffer;
use crate::frame::Frame;
use crate::paint::{LocalRect, PaintCx};

const SOURCES: [&str; 7] = [
    "plain ascii",
    "\u{FEFF}abc\u{200B}ghi 界x",
    "a\u{200B}bc",
    "e\u{301}t\u{e9} 👍🏽 ok",
    "界界界",
    "",
    "\u{200B}",
];

/// Paint `paint` into a `width`x`height` frame and return its selection scene.
fn scene_of(width: u16, height: u16, paint: impl FnOnce(&mut PaintCx<'_, '_>)) -> SelectionScene {
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    paint(&mut PaintCx::new(&mut frame));
    frame.selection().clone()
}

fn run_for(area: Rect, order: u64, source: &str, offset: usize) -> SelectionTextRun {
    SelectionTextRun::painted_text("scope", "content", area, order, source, offset).revision(7)
}

fn fragments_for(area: Rect, order: u64, source: &str, offset: usize) -> Vec<SelectionFragment> {
    painted_text_fragments("scope", "content", area, order, source, offset, 7)
}

#[test]
fn runs_materialize_the_fragments_of_per_grapheme_registration() {
    for source in SOURCES {
        for width in [0, 1, 2, 3, 5, 40] {
            let area = Rect::new(3, 2, width, 1);
            let run = run_for(area, 4, source, 10);
            assert_eq!(
                run.fragments().collect::<Vec<_>>(),
                fragments_for(area, 4, source, 10),
                "{source:?} at width {width}"
            );
        }
    }
}

#[test]
fn registered_runs_clip_like_registered_fragments() {
    // Translated through nested contexts and clipped on every side, including through wide
    // graphemes at the clip edges.
    let source = "界a界b界c";
    for (clip_x, clip_width) in [(0, 20), (2, 3), (3, 4), (5, 1), (0, 0)] {
        for (offset_x, offset_y) in [(0, 0), (-2, 1), (4, 2), (1, -1)] {
            let paint = |use_runs: bool| {
                scene_of(16, 4, |cx| {
                    cx.push_selection_scope(SelectionScope::new("scope", Rect::new(0, 0, 16, 4)));
                    cx.with_child(
                        offset_x,
                        offset_y,
                        LocalRect::new(clip_x - offset_x, -offset_y, clip_width, 4),
                        |cx| {
                            let area = Rect::new(1, 1, 12, 1);
                            if use_runs {
                                cx.push_selection_text_run(run_for(area, 0, source, 0));
                            } else {
                                for fragment in fragments_for(area, 0, source, 0) {
                                    cx.push_selection_fragment(fragment);
                                }
                            }
                        },
                    );
                })
            };
            let runs = paint(true);
            let fragments = paint(false);
            assert_eq!(
                runs.fragments(),
                fragments.fragments(),
                "clip {clip_x}+{clip_width} at offset ({offset_x}, {offset_y})"
            );
            assert_eq!(runs, fragments);
        }
    }
}

#[test]
fn scenes_present_runs_and_fragments_in_registration_order() {
    let mut mixed = SelectionScene::new();
    let mut eager = SelectionScene::new();
    mixed.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 20, 3)));
    eager.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 20, 3)));
    for (row, source) in ["one", "two", "three"].into_iter().enumerate() {
        let area = Rect::new(0, u16::try_from(row).expect("row"), 20, 1);
        let order = u64::try_from(row).expect("row");
        if row == 1 {
            for fragment in fragments_for(area, order, source, 0) {
                mixed.push_fragment(fragment);
            }
        } else {
            mixed.push_text_run(run_for(area, order, source, 0));
        }
        for fragment in fragments_for(area, order, source, 0) {
            eager.push_fragment(fragment);
        }
    }
    assert_eq!(mixed.fragments(), eager.fragments());
    // Registering after a query invalidates the materialized view.
    mixed.push_text_run(run_for(Rect::new(0, 2, 20, 1), 9, "four", 0));
    assert_eq!(mixed.fragments().len(), eager.fragments().len() + 4);
}

#[test]
fn selection_gestures_resolve_against_runs() {
    let mut scene = SelectionScene::new();
    scene.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 20, 2)));
    scene.push_text_run(run_for(Rect::new(0, 0, 20, 1), 0, "hello 界", 0));
    scene.push_text_run(run_for(Rect::new(0, 1, 20, 1), 1, "world", 10));
    let mut controller = SelectionController::new();
    let mouse = |kind, x, y| MouseEvent::new(kind, Point::new(x, y));
    assert_eq!(
        controller.handle_mouse(&scene, mouse(MouseEventKind::Down(MouseButton::Left), 6, 0)),
        SelectionOutcome::Armed
    );
    let _ = controller.handle_mouse(&scene, mouse(MouseEventKind::Drag(MouseButton::Left), 2, 1));
    let _ = controller.handle_mouse(&scene, mouse(MouseEventKind::Up(MouseButton::Left), 2, 1));
    let snapshot = controller.snapshot(&scene).expect("selection");
    let slices = snapshot
        .slices
        .iter()
        .map(|slice| slice.source_range.clone())
        .collect::<Vec<_>>();
    // The wide grapheme (bytes 6..9) and the first three graphemes of the second row.
    assert_eq!(slices, vec![6..9, 10..13]);
    assert_eq!(
        snapshot.visible_highlights,
        vec![Rect::new(6, 0, 2, 1), Rect::new(0, 1, 3, 1)]
    );
}

#[test]
fn occlusion_splits_only_the_runs_it_covers() {
    let scene = scene_of(20, 3, |cx| {
        cx.push_selection_scope(SelectionScope::new("scope", Rect::new(0, 0, 20, 3)));
        cx.push_selection_text_run(run_for(Rect::new(0, 0, 20, 1), 0, "abcdefgh", 0));
        cx.push_selection_text_run(run_for(Rect::new(0, 1, 20, 1), 1, "a界cdefgh", 0));
        cx.push_selection_text_run(run_for(Rect::new(0, 2, 20, 1), 2, "abcdefgh", 0));
        cx.occlude(LocalRect::new(2, 1, 3, 1));
    });
    let cells = |row: u16| {
        scene
            .fragments()
            .iter()
            .filter(|fragment| fragment.area.y == row)
            .map(|fragment| fragment.area.x)
            .collect::<Vec<_>>()
    };
    assert_eq!(cells(0), (0..8).collect::<Vec<_>>());
    // Row 1 loses the wide grapheme (cells 1..3) and the cells under the overlay.
    assert_eq!(cells(1), vec![0, 5, 6, 7, 8]);
    assert_eq!(cells(2), (0..8).collect::<Vec<_>>());
}

#[test]
fn regional_merge_replaces_runs_in_damaged_regions() {
    let mut committed = SelectionScene::new();
    committed.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 10, 2)));
    committed.push_text_run(run_for(Rect::new(0, 0, 10, 1), 0, "old top", 0));
    committed.push_text_run(run_for(Rect::new(0, 1, 10, 1), 1, "old bottom", 0));
    let mut emitted = SelectionScene::new();
    emitted.push_scope(SelectionScope::new("scope", Rect::new(0, 0, 10, 2)));
    emitted.push_text_run(
        SelectionTextRun::painted_text("scope", "new", Rect::new(0, 0, 10, 1), 0, "new top", 0)
            .revision(7),
    );
    emitted.push_text_run(
        SelectionTextRun::painted_text("scope", "new", Rect::new(0, 1, 10, 1), 1, "new bot", 0)
            .revision(7),
    );
    // Damage covering the whole top row and the start of the bottom row.
    let merged =
        committed.merge_regions(&emitted, &[Rect::new(0, 0, 10, 1), Rect::new(0, 1, 2, 1)]);
    let row = |y: u16| {
        merged
            .fragments()
            .iter()
            .filter(|fragment| fragment.area.y == y)
            .map(|fragment| (fragment.area.x, fragment.content_id.as_str().to_owned()))
            .collect::<Vec<_>>()
    };
    assert!(row(0).iter().all(|(_, content)| content == "new"));
    assert_eq!(row(0).len(), 7);
    let bottom = row(1);
    assert_eq!(bottom.len(), 2 + 8);
    assert!(
        bottom
            .iter()
            .all(|(x, content)| (*x < 2) == (content == "new"))
    );
}

#[test]
fn foreground_focus_scope_keeps_background_runs_selectable_content() {
    let scene = scene_of(10, 2, |cx| {
        cx.push_selection_scope(SelectionScope::new("background", Rect::new(0, 0, 10, 2)));
        cx.push_selection_text_run(SelectionTextRun::painted_text(
            "background",
            "c",
            Rect::new(0, 0, 10, 1),
            0,
            "abc",
            0,
        ));
        cx.set_focus_scope(Some(crate::focus::FocusScopeId::new("modal")));
    });
    assert_eq!(scene.fragments().len(), 3);
    assert_eq!(scene.scopes()[0].capture, SelectionCapture::Disabled);
    assert!(scene.initiation_scope(Point::new(0, 0)).is_none());
}
