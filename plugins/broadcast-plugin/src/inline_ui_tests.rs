use super::*;
use bmux_keyboard::{KeyStroke, Modifiers};
fn picker() -> Picker {
    let picker = Picker::new(
        (0..100)
            .map(|index| Choice {
                id: format!("source-{index}"),
                title: format!("terminal-{index}"),
                details: format!("pane {index} · nu"),
            })
            .collect(),
        Size::new(80, 24),
        "https://example.com/live".into(),
    );
    let _ = picker.render();
    picker
}
fn key(picker: &mut Picker, code: KeyCode, ctrl: bool) {
    picker
        .update(RuntimeEvent::Terminal(Event::Key(KeyStroke {
            key: code,
            modifiers: Modifiers {
                ctrl,
                ..Modifiers::default()
            },
        })))
        .unwrap();
}
#[test]
fn filtering_preserves_identity_and_paste_is_bounded() {
    let mut picker = picker();
    key(&mut picker, KeyCode::Down, false);
    let id = picker.selected().unwrap().id.clone();
    picker
        .update(RuntimeEvent::Terminal(Event::Paste("terminal-1".into())))
        .unwrap();
    assert_eq!(picker.selected().unwrap().id, id);
    assert_eq!(picker.visible.len(), 11);
    picker
        .update(RuntimeEvent::Terminal(Event::Paste("x".repeat(4097))))
        .unwrap();
    assert_eq!(picker.search.borrow().buffer().text(), "terminal-1");
}
#[test]
fn review_never_accepts_enter_and_requires_distinct_confirmation() {
    let mut picker = picker();
    key(&mut picker, KeyCode::Enter, false);
    assert!(picker.review);
    key(&mut picker, KeyCode::Enter, false);
    assert!(picker.accepted.is_none());
    key(&mut picker, KeyCode::Char('s'), true);
    assert_eq!(picker.accepted.as_deref(), Some("source-0"));
}
#[test]
fn large_list_renders_bounded_and_focus_is_revealed() {
    let mut picker = picker();
    let first = picker.render();
    assert_eq!(first.area().height, 15);
    key(&mut picker, KeyCode::End, false);
    let _ = picker.render();
    assert_eq!(picker.selected().unwrap().id, "source-99");
    assert!(picker.list.get().vertical_scroll() > 0);
}
#[test]
fn tiny_terminal_cannot_confirm_unseen_choice() {
    let mut picker = picker();
    picker.size = Size::new(10, 4);
    key(&mut picker, KeyCode::Enter, false);
    assert!(!picker.review);
    assert_eq!(picker.render().area().height, 3);
}
#[test]
fn no_matches_never_selects_a_different_source() {
    let mut picker = picker();
    picker
        .update(RuntimeEvent::Terminal(Event::Paste(
            "no-such-source".into(),
        )))
        .unwrap();
    assert!(picker.selected().is_none());
    key(&mut picker, KeyCode::Enter, false);
    assert!(!picker.review);
}
