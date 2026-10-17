use bmux_live_relay::{MAX_BYTES, Phase, Relay};
use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};
use bmux_web_renderer::{BrowserFrame, decode_binary, encode_binary, encode_delta};
use std::{fs, path::PathBuf};
use uuid::Uuid;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("bmux-relay-test-{}", Uuid::new_v4())))
    }
    fn path(&self) -> PathBuf {
        self.0.join("authority.json")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn frame(revision: u64) -> BrowserFrame {
    let state = PresentationState::new(Rect::new(0, 0, 12, 2));
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    BrowserFrame::from_prepared_in_generation("relay-test", revision, &prepared).unwrap()
}
#[test]
fn durable_restart_requires_full_and_preserves_exclusive_owner() {
    let dir = Scratch::new();
    let mut relay = Relay::open(dir.path(), 1000).unwrap();
    assert!(relay.start(Uuid::new_v4(), 1000).unwrap());
    let epoch = relay.authority.epoch.unwrap();
    let old_fence = relay.authority.fence;
    let first = frame(1);
    let second = frame(2);
    relay
        .ingest(encode_binary(&first).unwrap().into(), true)
        .unwrap();
    let durable_before = fs::read(dir.path()).unwrap();
    relay
        .ingest(encode_delta(&second, &first).unwrap().into(), false)
        .unwrap();
    assert_eq!(
        durable_before,
        fs::read(dir.path()).unwrap(),
        "frame ingestion must never write metadata"
    );
    let mut reopened = Relay::open(dir.path(), 2000).unwrap();
    assert_eq!(reopened.authority.epoch, Some(epoch));
    assert!(reopened.authority.fence > old_fence);
    assert!(!reopened.authority.owns(epoch, old_fence, 2000));
    assert!(!reopened.start(Uuid::new_v4(), 2000).unwrap());
    assert!(reopened.delivery.is_none());
    assert!(
        reopened
            .ingest(encode_delta(&second, &first).unwrap().into(), true)
            .is_err()
    );
    reopened
        .ingest(encode_binary(&second).unwrap().into(), true)
        .unwrap();
}
#[test]
fn pause_stop_expiry_fence_and_clear_baselines() {
    let dir = Scratch::new();
    let mut relay = Relay::open(dir.path(), 1000).unwrap();
    relay.start(Uuid::new_v4(), 1000).unwrap();
    let epoch = relay.authority.epoch.unwrap();
    let fence = relay.authority.fence;
    relay
        .ingest(encode_binary(&frame(1)).unwrap().into(), true)
        .unwrap();
    relay.control("pause", 2000).unwrap();
    assert_eq!(relay.authority.state, Phase::Paused);
    assert!(relay.delivery.is_none());
    assert!(!relay.authority.owns(epoch, fence, 2000));
    relay.control("resume", 3000).unwrap();
    assert_eq!(relay.authority.state, Phase::Starting);
    relay.control("heartbeat", 4000).unwrap();
    assert!(!relay.expire(18_999).unwrap());
    assert!(relay.expire(19_000).unwrap());
    assert_eq!(relay.authority.state, Phase::Offline);
    relay.start(Uuid::new_v4(), 20_000).unwrap();
    assert_ne!(relay.authority.epoch, Some(epoch));
    relay.control("stop", 20_001).unwrap();
    assert!(relay.authority.epoch.is_none());
}
#[test]
fn malformed_gap_and_oversize_do_not_replace_baseline() {
    let dir = Scratch::new();
    let mut relay = Relay::open(dir.path(), 1000).unwrap();
    relay.start(Uuid::new_v4(), 1000).unwrap();
    let first = frame(1);
    let second = frame(2);
    let third = frame(3);
    relay
        .ingest(encode_binary(&first).unwrap().into(), true)
        .unwrap();
    assert!(
        relay
            .ingest(encode_delta(&third, &second).unwrap().into(), false)
            .is_err()
    );
    assert!(relay.ingest(vec![0; MAX_BYTES + 1].into(), false).is_err());
    assert!(
        relay
            .ingest(b"BMWP\x03garbage".to_vec().into(), false)
            .is_err()
    );
    assert_eq!(relay.delivery.as_ref().unwrap().frame.revision(), 1);
    relay
        .ingest(encode_delta(&second, &first).unwrap().into(), false)
        .unwrap();
    let delivery = relay.delivery.as_ref().unwrap();
    assert_eq!(delivery.base, Some(("relay-test".into(), 1)));
    let a = delivery.snapshot().unwrap();
    let b = delivery.clone().snapshot().unwrap();
    assert_eq!(
        a.as_ptr(),
        b.as_ptr(),
        "repair encoding is cached and shared"
    );
    assert_eq!(decode_binary(&a, None).unwrap().revision(), 2);
    assert!(
        relay
            .ingest(encode_binary(&second).unwrap().into(), false)
            .is_err()
    );
}
#[test]
fn corrupt_durable_authority_is_not_reset() {
    let dir = Scratch::new();
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.path(), b"{\"version\":99}").unwrap();
    assert!(Relay::open(dir.path(), 1000).is_err());
    assert_eq!(fs::read(dir.path()).unwrap(), b"{\"version\":99}");
}
#[test]
fn failed_persistence_poisoning_revokes_in_memory_authority() {
    let dir = Scratch::new();
    let mut relay = Relay::open(dir.path(), 1000).unwrap();
    relay.start(Uuid::new_v4(), 1000).unwrap();
    fs::create_dir(dir.path().with_extension("tmp")).unwrap();
    assert!(relay.control("pause", 1001).is_err());
    assert!(relay.authority.epoch.is_none());
    assert!(relay.expire(1002).is_err());
    assert!(relay.start(Uuid::new_v4(), 1002).is_err());
}

#[test]
fn inherited_image_resource_is_not_retransmitted_by_tiny_patch() {
    let dir = Scratch::new();
    let mut relay = Relay::open(dir.path(), 1000).unwrap();
    relay.start(Uuid::new_v4(), 1000).unwrap();
    let mut value = serde_json::to_value(frame(1)).unwrap();
    value["image_scene"] = serde_json::json!({"active": {
        "image": {"key":"image", "payload":{"Pixels":{"bytes":vec![123_u8; 4096],"width":32,"height":32,"format":"Rgba8"}},
        "destination":{"x":0,"y":0,"width":1,"height":1}, "clip":{"x":0,"y":0,"width":12,"height":2}, "lifecycle":"Persistent"}
    }});
    let first: BrowserFrame = serde_json::from_value(value.clone()).unwrap();
    value["revision"] = 2.into();
    value["buffer"]["cells"][0]["symbol"] = "x".into();
    let second: BrowserFrame = serde_json::from_value(value).unwrap();
    let full = encode_binary(&first).unwrap();
    let patch = encode_delta(&second, &first).unwrap();
    assert!(patch.len() < 1024);
    assert!(full.len() > 4096);
    assert!(!patch.windows(32).any(|window| window == [123_u8; 32]));
    relay.ingest(full.into(), true).unwrap();
    relay.ingest(patch.clone().into(), false).unwrap();
    let delivery = relay.delivery.as_ref().unwrap();
    assert_eq!(delivery.patch.as_ref(), patch.as_slice());
    assert_eq!(delivery.frame.as_ref(), &second);
    assert_eq!(
        decode_binary(&delivery.snapshot().unwrap(), None).unwrap(),
        second
    );
}
