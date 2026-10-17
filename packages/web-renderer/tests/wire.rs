use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};
use bmux_web_renderer::{
    BrowserFrame, Limits, decode_binary, decode_binary_with_resources, encode_binary,
    encode_binary_with_resources, encode_delta, encode_delta_with_resources, extract_resources,
    resource_requirements,
};

fn frame(width: u16, height: u16, revision: u64) -> BrowserFrame {
    let state = PresentationState::new(Rect::new(0, 0, width, height));
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    BrowserFrame::from_prepared_in_generation("wire-test", revision, &prepared).unwrap()
}
fn mutate(frame: &BrowserFrame, mutate: impl FnOnce(&mut serde_json::Value)) -> BrowserFrame {
    let mut value = serde_json::to_value(frame).unwrap();
    mutate(&mut value);
    let frame: BrowserFrame = serde_json::from_value(value).unwrap();
    frame.validate(Limits::default()).unwrap();
    frame
}

#[test]
fn lossless_metadata_wide_cells_and_delta() {
    let base = mutate(&frame(12, 2, 1), |value| {
        value["buffer"]["cells"][0]["symbol"] = "界".into();
        value["buffer"]["cells"][0]["width"] = 2.into();
        value["buffer"]["cells"][1]["symbol"] = "".into();
        value["buffer"]["cells"][1]["width"] = 0.into();
        value["buffer"]["cells"][1]["wide_continuation"] = true.into();
        value["buffer"]["cells"][2]["symbol"] = "e\u{301}".into();
        value["selection_text_available"] = true.into();
        value["selection_text"] = serde_json::json!({"source": "unrendered source text"});
        value["unsupported_metadata"] = serde_json::json!(["semantics"]);
    });
    let encoded = encode_binary(&base).unwrap();
    assert_eq!(decode_binary(&encoded, None).unwrap(), base);
    let next = mutate(&base, |value| {
        value["revision"] = 2.into();
        value["buffer"]["cells"][17]["symbol"] = "!".into();
    });
    let delta = encode_delta(&next, &base).unwrap();
    assert_eq!(decode_binary(&delta, Some(&base)).unwrap(), next);
    assert!(delta.len() < encode_binary(&next).unwrap().len());
    assert!(decode_binary(&delta, None).is_err());
    let wrong = mutate(&base, |value| value["revision"] = 9.into());
    assert!(decode_binary(&delta, Some(&wrong)).is_err());
    let wrong = mutate(&base, |value| value["generation"] = "other".into());
    assert!(decode_binary(&delta, Some(&wrong)).is_err());
    assert!(encode_delta(&base, &base).is_err());
}

#[test]
fn resources_roundtrip_and_immutable_ids() {
    let base = mutate(&frame(8, 2, 1), |value| {
        value["image_scene"] = serde_json::json!({"active": {
            "first": {"key":"first", "payload":{"Pixels":{"bytes":[1,2,3,4],"width":1,"height":1,"format":"Rgba8"}},
                "destination":{"x":0,"y":0,"width":1,"height":1}, "clip":{"x":0,"y":0,"width":8,"height":2}, "lifecycle":"Frame"},
            "second": {"key":"second", "payload":{"Pixels":{"bytes":[1,2,3,4],"width":1,"height":1,"format":"Rgba8"}},
                "destination":{"x":2,"y":0,"width":1,"height":1}, "clip":{"x":0,"y":0,"width":8,"height":2}, "lifecycle":"Persistent"}
        }});
    });
    assert_eq!(
        decode_binary(&encode_binary(&base).unwrap(), None).unwrap(),
        base
    );
    assert!(
        resource_requirements(&encode_binary(&base).unwrap())
            .unwrap()
            .is_empty()
    );
    let bytes = encode_binary_with_resources(&base).unwrap();
    let resources = extract_resources(&base).unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].id.len(), 64);
    assert_eq!(resources[0].bytes(), [1, 2, 3, 4]);
    let requirements = resource_requirements(&bytes).unwrap();
    assert_eq!(requirements.len(), 1);
    assert_eq!(
        requirements[0].with_bytes(vec![1, 2, 3, 4]).unwrap(),
        resources[0]
    );
    assert!(requirements[0].with_bytes(vec![9]).is_err());
    assert!(decode_binary(&bytes, None).is_err());
    assert_eq!(
        decode_binary_with_resources(&bytes, None, &resources).unwrap(),
        base
    );
    let next = mutate(&base, |value| value["revision"] = 2.into());
    let delta = encode_delta_with_resources(&next, &base).unwrap();
    assert!(resource_requirements(&delta).unwrap().is_empty());
    assert_eq!(decode_binary(&delta, Some(&base)).unwrap(), next);
}

#[test]
fn one_digit_delta_retains_unchanged_cells_not_row_payload() {
    let base = mutate(&frame(600, 1, 1), |value| {
        for (index, cell) in value["buffer"]["cells"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            cell["symbol"] = char::from(b'!' + (index % 90) as u8).to_string().into();
        }
    });
    let next = mutate(&base, |value| {
        value["revision"] = 2.into();
        value["buffer"]["cells"][301]["symbol"] = "0".into();
    });
    let delta = encode_delta(&next, &base).unwrap();
    let message: TestMessage = bmux_codec::from_positional_bytes(&delta[5..]).unwrap();
    assert_eq!(message.rows.len(), 1);
    assert_eq!(message.rows[0].spans.len(), 3);
    assert!(matches!(
        message.rows[0].spans[0],
        TestSpan::Retain { count: 301 }
    ));
    assert!(matches!(
        message.rows[0].spans[2],
        TestSpan::Retain { count: 298 }
    ));
    assert!(delta.len() < 150, "{} bytes", delta.len());
    assert_eq!(decode_binary(&delta, Some(&base)).unwrap(), next);
    assert!(decode_binary(&delta, None).is_err());
    let wrong = mutate(&base, |value| value["revision"] = 3.into());
    assert!(decode_binary(&delta, Some(&wrong)).is_err());
}

#[test]
fn moved_immutable_image_reuses_base_resource() {
    let base = mutate(&frame(8, 2, 1), |value| {
        value["image_scene"] = serde_json::json!({"active": {
            "image": {"key":"image", "payload":{"Pixels":{"bytes":[1,2,3,255],"width":1,"height":1,"format":"Rgba8"}},
            "destination":{"x":0,"y":0,"width":1,"height":1},
            "clip":{"x":0,"y":0,"width":8,"height":2}, "lifecycle":"Persistent"}
        }});
    });
    let next = mutate(&base, |value| {
        value["revision"] = 2.into();
        value["image_scene"]["active"]["image"]["destination"]["x"] = 1.into();
    });
    let delta = encode_delta(&next, &base).unwrap();
    assert!(delta.len() < encode_binary(&next).unwrap().len());
    assert_eq!(decode_binary(&delta, Some(&base)).unwrap(), next);
}

#[test]
fn dense_600_by_300_is_compact() {
    let dense = mutate(&frame(600, 300, 1), |value| {
        for (index, cell) in value["buffer"]["cells"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            cell["symbol"] = char::from(b'!' + (index % 90) as u8).to_string().into();
        }
    });
    let snapshot = encode_binary(&dense).unwrap();
    assert!(snapshot.len() < 200_000, "{} bytes", snapshot.len());
    assert_eq!(decode_binary(&snapshot, None).unwrap(), dense);
    let next = mutate(&dense, |value| {
        value["revision"] = 2.into();
        value["buffer"]["cells"][601]["symbol"] = "x".into();
    });
    let delta = encode_delta(&next, &dense).unwrap();
    assert!(delta.len() < 1000, "{} bytes", delta.len());
    assert_eq!(decode_binary(&delta, Some(&dense)).unwrap(), next);
    eprintln!(
        "600x300 dense: snapshot={} delta={}",
        snapshot.len(),
        delta.len()
    );
}

#[test]
fn truncation_unknown_version_and_trailing_bytes_fail() {
    let bytes = encode_binary(&frame(4, 2, 1)).unwrap();
    for end in 0..bytes.len() {
        assert!(decode_binary(&bytes[..end], None).is_err());
    }
    for version in [bmux_web_renderer::BINARY_WIRE_VERSION + 1, 2, 3] {
        let mut corrupt = bytes.clone();
        corrupt[4] = version;
        assert!(decode_binary(&corrupt, None).is_err());
    }
    let mut corrupt = bytes;
    corrupt.push(0);
    assert!(decode_binary(&corrupt, None).is_err());
}

// Test-only frozen v1 shapes let malformed indices/topology be constructed
// without exposing transport internals as runtime API.
#[derive(serde::Serialize, serde::Deserialize)]
struct TestMessage {
    generation: String,
    revision: u64,
    base: Option<(String, u64)>,
    area: Rect,
    styles: Vec<bmux_tui::style::Style>,
    glyphs: Vec<String>,
    rows: Vec<TestRow>,
    metadata: Option<TestBlob>,
    images: Option<Vec<()>>,
    resources: Vec<()>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct TestBlob(#[serde(with = "bmux_codec::serde_bytes_vec")] Vec<u8>);
#[derive(serde::Serialize, serde::Deserialize)]
struct TestRow {
    index: u16,
    spans: Vec<TestSpan>,
}
#[derive(serde::Serialize, serde::Deserialize)]
enum TestSpan {
    Fill {
        style: u32,
        glyph: u32,
        width: u8,
        continuation: bool,
        count: u16,
    },
    Ascii {
        style: u32,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        bytes: Vec<u8>,
    },
    Glyphs {
        style: u32,
        cells: Vec<(u32, u8, bool)>,
    },
    Retain {
        count: u16,
    },
}
fn corrupt(change: impl FnOnce(&mut TestMessage)) {
    let bytes = encode_binary(&frame(4, 2, 1)).unwrap();
    let mut message: TestMessage = bmux_codec::from_positional_bytes(&bytes[5..]).unwrap();
    change(&mut message);
    let mut bytes = b"BMWP".to_vec();
    bytes.push(bmux_web_renderer::BINARY_WIRE_VERSION);
    bytes.extend(bmux_codec::to_positional_vec(&message).unwrap());
    assert!(decode_binary(&bytes, None).is_err());
}

#[test]
fn malformed_tables_widths_and_topology_fail_closed() {
    corrupt(|m| m.rows[0].spans = vec![TestSpan::Retain { count: 4 }]);
    corrupt(|m| m.styles.clear());
    corrupt(|m| m.glyphs.clear());
    corrupt(|m| m.glyphs[0] = "x".repeat(129));
    corrupt(|m| m.rows[0].index = 2);
    corrupt(|m| m.rows[1].index = 0);
    corrupt(|m| m.rows.remove(0).index = 0);
    corrupt(|m| {
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 0,
            glyph: 0,
            width: 3,
            continuation: false,
            count: 4,
        }]
    });
    corrupt(|m| {
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 999,
            glyph: 0,
            width: 1,
            continuation: false,
            count: 4,
        }]
    });
    corrupt(|m| {
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 0,
            glyph: 999,
            width: 1,
            continuation: false,
            count: 4,
        }]
    });
    corrupt(|m| {
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 0,
            glyph: 0,
            width: 1,
            continuation: false,
            count: 5,
        }]
    });
    corrupt(|m| {
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 0,
            glyph: 0,
            width: 1,
            continuation: false,
            count: 0,
        }]
    });
    corrupt(|m| {
        m.glyphs[0].clear();
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 0,
            glyph: 0,
            width: 0,
            continuation: true,
            count: 4,
        }];
    });
    corrupt(|m| {
        m.glyphs[0] = "界".into();
        m.rows[0].spans = vec![TestSpan::Fill {
            style: 0,
            glyph: 0,
            width: 2,
            continuation: false,
            count: 4,
        }];
    });
    corrupt(|m| m.metadata = None);
    corrupt(|m| m.images = None);
}

#[test]
fn unicode_style_boundaries_and_all_metadata_are_lossless() {
    use bmux_tui::{
        geometry::Point,
        hit::HitRegion,
        paint::LocalRect,
        semantic::SemanticRegion,
        style::{Color, Modifier, Style},
        text::Line,
    };
    let state = PresentationState::new(Rect::new(0, 0, 12, 3));
    let prepared = state
        .prepare(Damage::Regions(vec![Rect::new(0, 0, 12, 3)]), |cx| {
            cx.write_line(
                LocalRect::new(0, 0, 12, 1),
                &Line::raw("界e\u{301}🦀abcdef"),
            );
            cx.set_cell(
                0,
                1,
                "Ω",
                Style::new()
                    .fg(Color::BrightRed)
                    .bg(Color::Rgb(1, 2, 3))
                    .add_modifier(Modifier::UNDERLINE),
            );
            cx.set_cursor(Point::new(2, 2), true);
            cx.push_hit(HitRegion::new("target", Rect::new(0, 0, 2, 1)).focusable(true));
            cx.push_selection_scope(bmux_tui::selection::SelectionScope::new(
                "scope",
                Rect::new(0, 0, 2, 1),
            ));
            cx.push_selection_fragment(bmux_tui::selection::SelectionFragment::new(
                "scope",
                "source",
                Rect::new(0, 0, 2, 1),
                0,
                0..3,
            ));
            cx.push_semantic(SemanticRegion::new(
                "target",
                Rect::new(0, 0, 2, 1),
                "heading",
            ));
        })
        .unwrap();
    let frame = BrowserFrame::from_prepared_in_generation("rich", 1, &prepared)
        .unwrap()
        .with_selection_text(&std::collections::BTreeMap::from([(
            bmux_tui::selection::SelectionContentId::new("source"),
            "界 source".to_owned(),
        )]))
        .unwrap();
    let frame = frame
        .with_surfaces(vec![bmux_web_renderer::SurfaceProvenance {
            retained_id: "surface-1".into(),
            owner_plugin_id: Some("producer".into()),
            owner_local_id: Some("local".into()),
            revision: Some(12),
            stream_end: Some(123),
            rect: Rect::new(0, 0, 12, 3),
            clip: Rect::new(0, 0, 12, 3),
            layer: 2,
            z: -1,
            content_id: Some("source".into()),
            selection_available: false,
            semantics_available: true,
        }])
        .unwrap();
    assert_eq!(
        decode_binary(&encode_binary(&frame).unwrap(), None).unwrap(),
        frame
    );
    let next = mutate(&frame, |v| {
        v["revision"] = 2.into();
        v["buffer"]["cells"][12]["style"]["bg"] = serde_json::json!({"Rgb":[9,8,7]});
        v["selection_text_available"] = true.into();
        v["selection_text"] = serde_json::json!({"source":"full source text"});
        v["surfaces"][0]["stream_end"] = 124.into();
    });
    assert_eq!(
        decode_binary(&encode_delta(&next, &frame).unwrap(), Some(&frame)).unwrap(),
        next
    );
}

#[test]
fn missing_evicted_resources_and_repair_fail_closed() {
    let base = mutate(&frame(4, 2, 1), |v| {
        v["image_scene"] = serde_json::json!({"active":{"image": {
            "key":"image","payload":{"Pixels":{"bytes":[1,2,3,4],"width":1,"height":1,"format":"Rgba8"}},
            "destination":{"x":0,"y":0,"width":1,"height":1},"clip":{"x":0,"y":0,"width":4,"height":2},"lifecycle":"Persistent"
        }}});
    });
    let next = mutate(&base, |v| {
        v["revision"] = 2.into();
        v["image_scene"]["active"]["image"]["payload"]["Pixels"]["bytes"] =
            serde_json::json!([5, 6, 7, 8]);
    });
    let delta = encode_delta_with_resources(&next, &base).unwrap();
    assert!(
        decode_binary_with_resources(&delta, Some(&base), &extract_resources(&base).unwrap())
            .is_err()
    );
    assert!(decode_binary_with_resources(&delta, Some(&base), &[]).is_err());
    assert_eq!(
        decode_binary_with_resources(&delta, Some(&base), &extract_resources(&next).unwrap())
            .unwrap(),
        next
    );
    assert_eq!(
        decode_binary(&encode_binary(&next).unwrap(), None).unwrap(),
        next
    );
    let removed = mutate(&next, |v| {
        v["revision"] = 3.into();
        v["image_scene"] = serde_json::json!({"active":{}});
    });
    assert_eq!(
        decode_binary(&encode_delta(&removed, &next).unwrap(), Some(&next)).unwrap(),
        removed
    );
}
