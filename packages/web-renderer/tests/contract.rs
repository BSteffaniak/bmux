use bmux_tui::{
    damage::Damage,
    geometry::{Point, Rect},
    hit::HitRegion,
    image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    },
    paint::LocalRect,
    presentation::PresentationState,
    semantic::SemanticRegion,
    style::{Color, Modifier, Style},
    text::Line,
};
use bmux_web_renderer::{
    BackendCapabilities, BrowserBackend, BrowserFrame, DecodeError, Limits, PresentError, Renderer,
    SurfaceProvenance, decode_json, encode_json,
};

#[test]
fn empty_viewports_roundtrip_and_replace_visible_metadata() {
    let mut renderer = Renderer::new(Recording::default(), Limits::default());
    for (revision, area) in [
        (1, Rect::new(0, 0, 4, 2)),
        (2, Rect::new(0, 0, 0, 0)),
        (3, Rect::new(0, 0, 0, 2)),
        (4, Rect::new(0, 0, 4, 0)),
        (5, Rect::new(0, 0, 4, 2)),
    ] {
        let state = PresentationState::new(area);
        let prepared = state
            .prepare(Damage::Full, |cx| {
                cx.push_hit(HitRegion::new("visible", Rect::new(0, 0, 1, 1)));
            })
            .unwrap();
        let frame = BrowserFrame::from_prepared(revision, &prepared).unwrap();
        let decoded = decode_json(&encode_json(&frame).unwrap(), Limits::default()).unwrap();
        assert_eq!(decoded, frame);
        let binary = bmux_web_renderer::encode_binary(&frame).unwrap();
        assert_eq!(
            bmux_web_renderer::decode_binary(&binary, None).unwrap(),
            frame
        );
        if area.width == 0 || area.height == 0 {
            assert!(frame.hits().regions().is_empty());
        }
        renderer.present(frame).unwrap();
        assert_eq!(renderer.committed().unwrap().buffer().area(), area);
    }
}

#[test]
fn metadata_availability_is_required_bounded_and_strict_in_v3() {
    use bmux_web_renderer::MetadataKind;
    let state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let prepared = state.prepare(Damage::Full, |_| {}).expect("prepared");
    let complete = BrowserFrame::from_prepared(1, &prepared).expect("frame");
    assert!(complete.unsupported_metadata().is_empty());
    let unavailable = complete
        .clone()
        .with_unsupported_metadata(&[MetadataKind::Selection, MetadataKind::Semantics])
        .expect("availability");
    let json = encode_json(&unavailable).expect("encode");
    assert_eq!(
        decode_json(&json, Limits::default()).expect("roundtrip"),
        unavailable
    );
    let mut renderer = Renderer::new(Recording::default(), Limits::default());
    renderer
        .present(unavailable)
        .expect("unavailable metadata does not reject output");
    assert_eq!(
        renderer
            .committed()
            .expect("committed")
            .unsupported_metadata()
            .len(),
        2
    );
    assert!(
        complete
            .clone()
            .with_unsupported_metadata(&[MetadataKind::Selection, MetadataKind::Selection])
            .is_err()
    );
    assert!(
        complete
            .with_unsupported_metadata(&[MetadataKind::Selection; 3])
            .is_err()
    );
    let value: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    let mut missing = value.clone();
    missing
        .as_object_mut()
        .expect("object")
        .remove("unsupported_metadata");
    assert!(decode_json(&missing.to_string(), Limits::default()).is_err());
    let mut legacy = value.clone();
    legacy["version"] = 1.into();
    assert!(matches!(
        decode_json(&legacy.to_string(), Limits::default()),
        Err(DecodeError::Version)
    ));
    for kinds in [
        serde_json::json!(["future"]),
        serde_json::json!(["selection", "selection"]),
        serde_json::json!(["selection", "semantics", "selection"]),
    ] {
        let mut invalid = value.clone();
        invalid["unsupported_metadata"] = kinds;
        assert!(decode_json(&invalid.to_string(), Limits::default()).is_err());
    }
    let mut native = PresentationState::new(Rect::new(0, 0, 8, 3));
    assert!(
        first(&mut native)
            .with_unsupported_metadata(&[MetadataKind::Semantics])
            .is_err()
    );
}

#[test]
fn selection_text_is_source_owned_bounded_and_utf8_checked() {
    use bmux_tui::selection::{SelectionContentId, SelectionFragment, SelectionScope};
    use std::collections::BTreeMap;
    let state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let prepared = state
        .prepare(Damage::Full, |cx| {
            cx.push_selection_scope(SelectionScope::new("owner:scope", Rect::new(0, 0, 2, 1)));
            cx.push_selection_fragment(SelectionFragment::new(
                "owner:scope",
                "owner:text",
                Rect::new(0, 0, 2, 1),
                0,
                0..3,
            ));
        })
        .expect("prepared");
    let frame = BrowserFrame::from_prepared(1, &prepared).expect("frame");
    assert!(!frame.selection_text_available());
    assert!(frame.clone().with_selection_text(&BTreeMap::new()).is_err());
    let text = BTreeMap::from([(
        SelectionContentId::new("owner:text"),
        "界 source".to_owned(),
    )]);
    let frame = frame.with_selection_text(&text).expect("real source");
    assert!(frame.selection_text_available());
    assert_eq!(frame.selection_text(), &text);
    let json = encode_json(&frame).expect("encode");
    assert_eq!(
        decode_json(&json, Limits::default()).expect("decode"),
        frame
    );
    let value: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    for field in ["source_range", "word_range", "line_range"] {
        let mut invalid = value.clone();
        invalid["selection"]["fragments"][0][field] = serde_json::json!({"start":1,"end":3});
        assert!(
            decode_json(&invalid.to_string(), Limits::default()).is_err(),
            "{field}"
        );
    }
    for field in [
        "selection_text",
        "selection_text_available",
        "metadata_availability",
    ] {
        let mut missing = value.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(decode_json(&missing.to_string(), Limits::default()).is_err());
    }
    let duplicate = json.replace(
        "\"owner:text\":\"界 source\"",
        "\"owner:text\":\"界 source\",\"owner:text\":\"different\"",
    );
    assert_ne!(duplicate, json);
    assert!(decode_json(&duplicate, Limits::default()).is_err());
    let oversized = BTreeMap::from([(SelectionContentId::new("owner:text"), "a".repeat(65537))]);
    assert!(frame.clone().with_selection_text(&oversized).is_err());
    let many = (0..4097)
        .map(|i| (SelectionContentId::new(format!("id{i}")), String::new()))
        .collect();
    assert!(frame.clone().with_selection_text(&many).is_err());
    let aggregate = (0..17)
        .map(|i| (SelectionContentId::new(format!("id{i}")), "a".repeat(65536)))
        .collect();
    assert!(frame.with_selection_text(&aggregate).is_err());
}

#[test]
fn external_committed_focus_is_not_inferred_from_visible_targets() {
    let state = PresentationState::new(Rect::new(0, 0, 8, 2));
    let mut prepared = state
        .prepare(Damage::Full, |cx| {
            cx.push_hit(HitRegion::new("control", Rect::new(0, 0, 2, 1)).focusable(true));
            cx.push_hit(HitRegion::new("control", Rect::new(0, 1, 2, 1)).focusable(true));
        })
        .unwrap();
    assert_eq!(prepared.focus().targets().len(), 1);
    assert!(prepared.project_active_focus(None));
    let frame = BrowserFrame::from_prepared(1, &prepared).unwrap();
    let decoded = decode_json(&encode_json(&frame).unwrap(), Limits::default()).unwrap();
    assert!(decoded.focus().active().is_none());
    assert!(prepared.project_active_focus(Some(&bmux_tui::focus::FocusId::new("control"))));
    let frame = BrowserFrame::from_prepared(2, &prepared).unwrap();
    assert_eq!(frame.focus().active().unwrap().as_str(), "control");
}

#[derive(Default)]
struct Recording {
    frames: Vec<BrowserFrame>,
    fail: bool,
}
impl BrowserBackend for Recording {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::COMPLETE
    }
    type Error = &'static str;
    fn render(
        &mut self,
        frame: &BrowserFrame,
        _previous: Option<&BrowserFrame>,
    ) -> Result<(), Self::Error> {
        if self.fail {
            return Err("output rejected");
        }
        self.frames.push(frame.clone());
        Ok(())
    }
    fn reset(&mut self) -> Result<(), Self::Error> {
        if self.fail {
            return Err("reset rejected");
        }
        self.frames.clear();
        Ok(())
    }
}
#[test]
fn image_fit_deserialization_defaults_to_contain_but_honors_stretch() {
    let original = image();
    let mut json = serde_json::to_value(&original).unwrap();
    json.as_object_mut().unwrap().remove("fit");
    let decoded: ImagePlacement = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded.fit, bmux_tui::image::ImageFit::Contain);
    json["fit"] = serde_json::json!("Stretch");
    let decoded: ImagePlacement = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.fit, bmux_tui::image::ImageFit::Stretch);
}

fn image() -> ImagePlacement {
    ImagePlacement {
        fit: bmux_tui::image::ImageFit::Contain,
        allocation: Some(bmux_tui::image::ImageBounds {
            x: 1,
            y: -1,
            width: 2,
            height: 3,
        }),
        key: ImageKey::new("pixels"),
        destination: Rect::new(1, 1, 2, 1),
        clip: Rect::new(0, 0, 8, 3),
        lifecycle: ImageLifecycle::Persistent,
        payload: ImagePayload::Pixels {
            bytes: vec![1, 2, 3, 255],
            width: 1,
            height: 1,
            format: ImagePixelFormat::Rgba8,
        },
    }
}
fn first(state: &mut PresentationState) -> BrowserFrame {
    state
        .present(
            Damage::Full,
            |cx| {
                cx.write_line(LocalRect::new(0, 0, 8, 1), &Line::raw("界e\u{301}🦀"));
                cx.set_cursor(Point::new(2, 2), true);
                cx.push_hit(HitRegion::new("retained", Rect::new(0, 0, 2, 1)));
                cx.push_semantic(SemanticRegion::new(
                    "retained",
                    Rect::new(0, 0, 2, 1),
                    "heading",
                ));
                cx.push_image(ImageContribution::Present(image()));
            },
            |prepared, _| BrowserFrame::from_prepared_in_generation("source-a", 1, prepared),
        )
        .expect("output")
        .expect("frame")
}
#[test]
fn shared_wide_cells_partial_damage_images_and_metadata_are_not_recomposed() {
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let first = first(&mut state);
    assert_eq!(first.buffer().cells()[0].symbol, "界");
    assert!(first.buffer().cells()[1].is_wide_continuation());
    assert_eq!(first.buffer().cells()[2].symbol, "e\u{301}");
    let second = state
        .present(
            Damage::Regions(vec![Rect::new(6, 2, 1, 1)]),
            |cx| {
                cx.set_cell(
                    6,
                    2,
                    "x",
                    Style::new()
                        .fg(Color::BrightRed)
                        .add_modifier(Modifier::UNDERLINE),
                );
            },
            |prepared, _| BrowserFrame::from_prepared_in_generation("source-a", 2, prepared),
        )
        .expect("output")
        .expect("frame");
    let second =
        decode_json(&encode_json(&second).expect("encode"), Limits::default()).expect("decode");
    assert_eq!(second.hits(), first.hits());
    assert_eq!(second.semantics(), first.semantics());
    assert_eq!(second.image_scene(), first.image_scene());
    assert_eq!(second.buffer().cells()[0], first.buffer().cells()[0]);
    let mut renderer = Renderer::new(Recording::default(), Limits::default());
    renderer.present(first).expect("render");
    renderer.backend_mut().fail = true;
    assert!(renderer.present(second.clone()).is_err());
    assert_eq!(renderer.committed().expect("retained").revision(), 1);
    renderer.backend_mut().fail = false;
    renderer.present(second).expect("retry");
    let removed = state
        .present(
            Damage::Full,
            |cx| {
                cx.push_image(ImageContribution::Remove(ImageKey::new("pixels")));
            },
            |prepared, _| BrowserFrame::from_prepared_in_generation("source-a", 3, prepared),
        )
        .expect("output")
        .expect("frame");
    renderer.present(removed).expect("remove image");
    assert_eq!(
        renderer
            .committed()
            .expect("frame")
            .image_scene()
            .placements()
            .len(),
        0
    );
}
#[test]
fn explicit_generation_reset_prevents_reattach_revision_confusion() {
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let frame = first(&mut state);
    let mut renderer = Renderer::new(Recording::default(), Limits::default());
    renderer.present(frame.clone()).expect("render");
    let mut json: serde_json::Value =
        serde_json::from_str(&encode_json(&frame).expect("encode")).expect("JSON");
    json["generation"] = "source-b".into();
    let next = decode_json(&json.to_string(), Limits::default()).expect("new source");
    assert!(matches!(
        renderer.present(next.clone()),
        Err(PresentError::GenerationChanged)
    ));
    assert_eq!(
        renderer.committed().expect("retained").generation(),
        "source-a"
    );
    renderer.reset().expect("explicit reset");
    renderer.present(next).expect("reattach");
}
#[test]
fn invalid_resources_and_limits_do_not_reach_backend() {
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let frame = first(&mut state);
    let mut renderer = Renderer::new(
        Recording::default(),
        Limits {
            image_bytes: 3,
            ..Limits::default()
        },
    );
    assert!(matches!(
        renderer.present(frame.clone()),
        Err(PresentError::Invalid(DecodeError::Limit(_)))
    ));
    assert!(renderer.backend().frames.is_empty());
    let mut json: serde_json::Value =
        serde_json::from_str(&encode_json(&frame).expect("encode")).expect("JSON");
    json["image_scene"]["active"]["pixels"]["payload"]["Pixels"]["bytes"] =
        serde_json::json!([1, 2]);
    assert!(decode_json(&json.to_string(), Limits::default()).is_err());
    json["image_scene"]["active"]["pixels"]["payload"] =
        serde_json::json!({"Jpeg": {"bytes": [1], "width": 1, "height": 1}});
    assert!(decode_json(&json.to_string(), Limits::default()).is_err());
    json["image_scene"]["active"]["pixels"]["payload"] =
        serde_json::json!({"Png": {"bytes": [1], "width": 1, "height": 1}});
    assert!(matches!(
        decode_json(&json.to_string(), Limits::default()),
        Err(DecodeError::Unsupported(_))
    ));
}
#[test]
fn malformed_geometry_focus_and_unknown_modifiers_are_rejected() {
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let frame = first(&mut state);
    let json = encode_json(&frame).expect("encode");
    for mutation in ["geometry", "focus", "modifier", "unknown", "wide"] {
        let mut value: serde_json::Value = serde_json::from_str(&json).expect("JSON");
        match mutation {
            "geometry" => value["buffer"]["area"]["x"] = 65535.into(),
            "focus" => value["focus"]["active"] = 99.into(),
            "modifier" => value["buffer"]["cells"][0]["style"]["modifiers"] = 32768.into(),
            "unknown" => value["unsupported_resource"] = true.into(),
            "wide" => value["buffer"]["cells"][1]["wide_continuation"] = false.into(),
            _ => unreachable!(),
        }
        assert!(
            decode_json(&value.to_string(), Limits::default()).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn provenance_is_preserved_without_browser_composition_and_bounded() {
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let root = SurfaceProvenance {
        retained_id: "00000000-0000-0000-0000-000000000001".to_owned(),
        owner_plugin_id: None,
        owner_local_id: None,
        revision: None,
        stream_end: None,
        rect: Rect::new(0, 0, 8, 3),
        clip: Rect::new(0, 0, 8, 3),
        layer: -2,
        z: -7,
        content_id: None,
        selection_available: false,
        semantics_available: false,
    };
    let child = SurfaceProvenance {
        retained_id: "00000000-0000-0000-0000-000000000002".to_owned(),
        owner_plugin_id: Some("producer".to_owned()),
        owner_local_id: Some("overlay".to_owned()),
        revision: Some(19),
        stream_end: Some(u64::MAX),
        rect: Rect::new(6, 1, 8, 4),
        clip: Rect::new(6, 1, 2, 2),
        layer: 5,
        z: 17,
        content_id: Some("00000000-0000-0000-0000-000000000003".to_owned()),
        selection_available: true,
        semantics_available: true,
    };
    let surfaces = vec![root.clone(), child.clone()];
    let frame = first(&mut state)
        .with_surfaces(surfaces.clone())
        .expect("provenance");
    let decoded =
        decode_json(&encode_json(&frame).expect("encode"), Limits::default()).expect("decode");
    assert_eq!(decoded.surfaces(), surfaces);
    assert!(!decoded.surfaces()[0].selection_available);
    assert_eq!(
        decoded.partial_metadata(),
        vec![
            bmux_web_renderer::MetadataKind::Selection,
            bmux_web_renderer::MetadataKind::Semantics
        ]
    );
    assert!(decoded.surfaces()[1].selection_available);
    assert!(
        decoded
            .clone()
            .with_unsupported_metadata(&[bmux_web_renderer::MetadataKind::Selection])
            .is_err()
    );
    let encoded = encode_json(&decoded).expect("encode");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("JSON");
    for field in ["selection_available", "semantics_available"] {
        let mut missing = value.clone();
        missing["surfaces"][0]
            .as_object_mut()
            .expect("surface")
            .remove(field);
        assert!(decode_json(&missing.to_string(), Limits::default()).is_err());
        let mut invalid = value.clone();
        invalid["surfaces"][0][field] = "true".into();
        assert!(decode_json(&invalid.to_string(), Limits::default()).is_err());
    }
    let mut renderer = Renderer::new(Recording::default(), Limits::default());
    renderer.present(decoded).expect("render");
    assert_eq!(
        renderer.committed().expect("committed").surfaces(),
        surfaces
    );
    let mut offscreen = child.clone();
    offscreen.rect = Rect::new(40, 20, 8, 4);
    offscreen.clip = Rect::new(40, 20, 8, 4);
    let offscreen_frame = first(&mut state)
        .with_surfaces(vec![offscreen.clone()])
        .expect("offscreen retained surface");
    let offscreen_decoded = decode_json(
        &encode_json(&offscreen_frame).expect("encode"),
        Limits::default(),
    )
    .expect("decode");
    assert_eq!(offscreen_decoded.surfaces(), &[offscreen]);
    let mut overflowing = child.clone();
    overflowing.rect = Rect::new(65535, 0, 2, 1);
    assert!(first(&mut state).with_surfaces(vec![overflowing]).is_err());
    let mut bad = child;
    bad.retained_id.clone_from(&root.retained_id);
    assert!(first(&mut state).with_surfaces(vec![root, bad]).is_err());
}

#[test]
fn duplicate_fields_and_aggregate_native_byte_limits_are_rejected() {
    let mut state = PresentationState::new(Rect::new(0, 0, 8, 3));
    let frame = first(&mut state);
    let json = encode_json(&frame).expect("encode");
    let version_field = format!("\"version\":{}", bmux_web_renderer::WIRE_VERSION);
    assert!(json.contains(&version_field));
    let duplicate = json.replacen(
        &version_field,
        &format!("{version_field},{version_field}"),
        1,
    );
    assert_ne!(duplicate, json);
    assert!(decode_json(&duplicate, Limits::default()).is_err());
    let mut renderer = Renderer::new(
        Recording::default(),
        Limits {
            json_bytes: 10,
            ..Limits::default()
        },
    );
    assert!(matches!(
        renderer.present(frame),
        Err(PresentError::Invalid(DecodeError::Limit(_)))
    ));
    assert!(renderer.backend().frames.is_empty());
}
