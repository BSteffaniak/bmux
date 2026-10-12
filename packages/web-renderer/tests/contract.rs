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
    BrowserBackend, BrowserFrame, DecodeError, Limits, PresentError, Renderer, SurfaceProvenance,
    decode_json, encode_json,
};

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
fn image() -> ImagePlacement {
    ImagePlacement {
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
    };
    let surfaces = vec![root.clone(), child.clone()];
    let frame = first(&mut state)
        .with_surfaces(surfaces.clone())
        .expect("provenance");
    let decoded =
        decode_json(&encode_json(&frame).expect("encode"), Limits::default()).expect("decode");
    assert_eq!(decoded.surfaces(), surfaces);
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
    let duplicate = json.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
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
