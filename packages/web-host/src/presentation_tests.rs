use super::*;
use crate::viewport::Viewport;
use crate::{ViewportMode, snapshot_feed};
use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};

fn frame(generation: &str, revision: u64, cols: u16) -> BrowserFrame {
    let state = PresentationState::new(Rect::new(0, 0, cols, 2));
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    BrowserFrame::from_prepared_in_generation(generation, revision, &prepared).unwrap()
}

#[test]
fn rejection_preserves_feed_generation_and_recovery_revision() {
    let small = frame("initial", 1, 4);
    let size = bmux_web_renderer::wire::encode_binary_with_resources(&small)
        .unwrap()
        .len();
    let (feed_writer, feed) = snapshot_feed(size).unwrap();
    let (viewport, _requests) = Viewport::new(ViewportMode::Resizable);
    let viewport = Arc::new(viewport);
    let control = ViewControl::new(Arc::clone(&viewport));
    let mut publisher = BrowserPublisher::new(feed_writer, control);
    assert_eq!(publisher.publish_frame(small).unwrap(), 1);
    assert!(
        publisher
            .publish_frame(frame("replacement", 2, 80))
            .is_err()
    );
    assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 1);
    assert_eq!(publisher.previous.as_ref().unwrap().generation(), "initial");
    assert_eq!(publisher.revision, 1);
    let status: serde_json::Value = serde_json::from_slice(&viewport.claim(None).unwrap()).unwrap();
    assert_eq!(status["generation"], "initial");
    assert!(
        status["presentation_status"]
            .as_str()
            .unwrap()
            .contains("browser")
    );
    assert!(status["presentation_status"].as_str().unwrap().len() <= 1024);
    assert_eq!(publisher.publish_frame(frame("recover", 3, 4)).unwrap(), 2);
    let status: serde_json::Value = serde_json::from_slice(&viewport.claim(None).unwrap()).unwrap();
    assert!(status["presentation_status"].is_null());
    assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 2);
    assert_eq!(publisher.previous.as_ref().unwrap().generation(), "recover");
    viewport.close();
    assert!(publisher.publish_frame(frame("closed", 4, 4)).is_err());
    assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 2);
    assert_eq!(publisher.revision, 2);
}

#[test]
fn optional_delta_admission_failure_repairs_with_full_and_resources_are_atomic() {
    use sha2::{Digest, Sha256};

    let (feed_writer, feed) = snapshot_feed(128).unwrap();
    let (viewport, _requests) = Viewport::new(ViewportMode::Resizable);
    let viewport = Arc::new(viewport);
    let publisher = BrowserPublisher::new(feed_writer, ViewControl::new(Arc::clone(&viewport)));
    let full = BinaryPresentation {
        snapshot: Arc::from([1_u8]),
        delta: None,
    };
    publisher
        .publish_encoded("initial", 1, None, full.clone(), BTreeMap::new())
        .unwrap();
    let invalid_delta = BinaryPresentation {
        snapshot: Arc::from([2_u8]),
        delta: Some((99, Arc::from([3_u8]))),
    };
    publisher
        .publish_encoded("repaired", 2, None, invalid_delta, BTreeMap::new())
        .unwrap();
    assert!(
        feed.receiver
            .borrow()
            .as_ref()
            .unwrap()
            .binary
            .as_ref()
            .unwrap()
            .delta
            .is_none()
    );
    let bytes = Arc::<[u8]>::from([4_u8, 5]);
    let id = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let resources = BTreeMap::from([(
        id.clone(),
        Resource {
            bytes: Arc::clone(&bytes),
            mime_type: "application/octet-stream".into(),
        },
    )]);
    publisher
        .publish_encoded("resource", 3, None, full.clone(), resources)
        .unwrap();
    let invalid = BTreeMap::from([(
        id.clone(),
        Resource {
            bytes: Arc::from([6_u8]),
            mime_type: "application/octet-stream".into(),
        },
    )]);
    assert!(
        publisher
            .publish_encoded("rejected", 4, None, full, invalid)
            .is_err()
    );
    let current = feed.receiver.borrow();
    assert_eq!(current.as_ref().unwrap().revision, 3);
    assert_eq!(current.as_ref().unwrap().resources[&id].bytes, bytes);
    let info: serde_json::Value = serde_json::from_slice(&viewport.claim(None).unwrap()).unwrap();
    assert_eq!(info["generation"], "resource");
    drop(current);
    assert!(
        publisher
            .publish_encoded(
                "",
                4,
                None,
                BinaryPresentation {
                    snapshot: Arc::from([1_u8]),
                    delta: None,
                },
                BTreeMap::new()
            )
            .is_err()
    );
    assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 3);
    drop(feed);
    assert!(
        publisher
            .publish_encoded(
                "disconnected",
                4,
                None,
                BinaryPresentation {
                    snapshot: Arc::from([1_u8]),
                    delta: None,
                },
                BTreeMap::new()
            )
            .is_err()
    );
    let info: serde_json::Value = serde_json::from_slice(&viewport.claim(None).unwrap()).unwrap();
    assert_eq!(info["generation"], "resource");
}

#[test]
fn prepared_entry_point_keeps_explicit_caller_identity() {
    let (feed_writer, feed) = snapshot_feed(crate::Limits::default().max_snapshot_bytes).unwrap();
    let (viewport, _requests) = Viewport::new(ViewportMode::Observer);
    let mut publisher = BrowserPublisher::new(feed_writer, ViewControl::new(Arc::new(viewport)));
    let state = PresentationState::new(Rect::new(0, 0, 4, 2));
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    assert_eq!(
        publisher.publish_prepared("caller", 42, &prepared).unwrap(),
        1
    );
    let committed = publisher.previous.as_ref().unwrap();
    assert_eq!(committed.generation(), "caller");
    assert_eq!(committed.revision(), 42);
    assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 1);
    assert!(publisher.publish_frame(frame("caller", 42, 4)).is_err());
    assert_eq!(publisher.revision, 1);
    assert!(publisher.publish_prepared("", 43, &prepared).is_err());
    assert_eq!(publisher.revision, 1);
}
