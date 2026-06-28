//! Protocol construction and serialization tests.

use bmux_tui_component_protocol::{
    ActionId, ButtonRole, ComponentKind, ComponentNode, ComponentTree, OptionItem, StackDirection,
};

fn sample_tree() -> ComponentTree {
    ComponentTree::new(ComponentNode::container(
        ComponentKind::Stack {
            direction: StackDirection::Vertical,
            gap: 1,
        },
        vec![
            ComponentNode::leaf(ComponentKind::Text {
                text: "Choose an option".to_owned(),
                align: None,
            }),
            ComponentNode::leaf(ComponentKind::RadioGroup {
                options: vec![OptionItem::new("yes", "Yes"), OptionItem::new("no", "No")],
                selected: Some("yes".to_owned()),
                required: true,
                disabled: false,
            })
            .with_id("choice"),
            ComponentNode::leaf(ComponentKind::Button {
                label: "Submit".to_owned(),
                action: ActionId::new("submit"),
                role: ButtonRole::Primary,
                disabled: false,
            }),
        ],
    ))
}

#[cfg(feature = "serde-json")]
#[test]
fn json_roundtrip_preserves_component_tree() {
    let tree = sample_tree();

    let value = bmux_tui_component_protocol::serialization::json::to_json_value(&tree).unwrap();
    let decoded: ComponentTree =
        bmux_tui_component_protocol::serialization::json::from_json_value(value).unwrap();

    assert_eq!(decoded, tree);
}

#[cfg(feature = "bmux-codec")]
#[test]
fn bmux_codec_roundtrip_preserves_component_tree() {
    let tree = sample_tree();

    let bytes = bmux_tui_component_protocol::serialization::codec::to_typed_bytes(&tree).unwrap();
    let decoded: ComponentTree =
        bmux_tui_component_protocol::serialization::codec::from_typed_bytes(&bytes).unwrap();

    assert_eq!(decoded, tree);
}

#[test]
fn component_tree_records_protocol_version() {
    let tree = sample_tree();

    assert_eq!(tree.protocol_version, 1);
}
