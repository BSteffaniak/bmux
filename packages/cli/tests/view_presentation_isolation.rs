//! Real bundled-plugin routing and recipient-retained presentation acceptance.
#![cfg(all(
    unix,
    feature = "bundled-plugin-theme",
    feature = "bundled-plugin-decoration"
))]

#[allow(dead_code)] // The shared fixture also supports gateway tests.
mod support;

use std::time::Duration;

use bmux_appearance::{RUNTIME_APPEARANCE_STATE_KIND, RuntimeAppearance};
use bmux_attach_layout_protocol::attach_layout_protocol::AttachSurfaceSummary;
use bmux_client::{BmuxClient, ServerEvent, StreamingBmuxClient};
use bmux_decoration_plugin_api::decoration_view_v1::client as view;
use bmux_scene_protocol::scene_protocol::{DecorationScene, Rect, STATE_KIND};
use bmux_theme_plugin::{theme_control_v1 as theme, theme_state};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_WINDOW: Duration = Duration::from_millis(250);

async fn connect(server: &support::ServerEnv) -> StreamingBmuxClient {
    let endpoint = bmux_ipc::IpcEndpoint::unix_socket(server.runtime_dir.join("server.sock"));
    let mut client = BmuxClient::connect(&endpoint, REQUEST_TIMEOUT, "view-isolation-test")
        .await
        .expect("connect to isolated bundled server");
    client
        .require_service_contracts()
        .expect("checked service contracts");
    StreamingBmuxClient::from_client(client).expect("streaming connection")
}

fn surfaces(pane: Uuid, width: u16) -> Vec<AttachSurfaceSummary> {
    let rect = Rect {
        x: 0,
        y: 0,
        w: width,
        h: 8,
    };
    vec![AttachSurfaceSummary {
        surface_id: pane,
        pane_id: Some(pane),
        rect: rect.clone(),
        content_rect: rect,
        visible: true,
    }]
}

async fn layout(
    client: &mut StreamingBmuxClient,
    life: Uuid,
    pane: Uuid,
    width: u16,
    revision: u64,
) {
    view::update_layout(client, life, revision, surfaces(pane, width), Some(pane))
        .await
        .expect("decoration view service must be available")
        .expect("accept owner layout");
}

async fn current(client: &mut StreamingBmuxClient) -> theme::Snapshot {
    theme::client::current(client)
        .await
        .expect("theme control service must be available")
}

async fn appearance(client: &mut StreamingBmuxClient) -> theme_state::Appearance {
    theme_state::client::active_appearance(client)
        .await
        .expect("theme appearance service must be available")
}

async fn select(client: &mut StreamingBmuxClient, revision: u64, name: &str) -> theme::Snapshot {
    theme::client::select(
        client,
        revision,
        theme::Selection::Preset { name: name.into() },
    )
    .await
    .expect("dispatch generated theme selection")
    .expect("select bundled preset")
}

#[derive(Default, Debug)]
struct PresentationEvents {
    scenes: Vec<DecorationScene>,
    appearances: Vec<theme_state::Appearance>,
}

// An absolute deadline prevents unrelated public events from extending a negative assertion.
async fn events(client: &mut StreamingBmuxClient) -> PresentationEvents {
    let deadline = tokio::time::Instant::now() + EVENT_WINDOW;
    let mut result = PresentationEvents::default();
    loop {
        match tokio::time::timeout_at(deadline, client.event_receiver().recv()).await {
            Ok(Some(ServerEvent::PluginBusEvent { kind, payload }))
                if kind == STATE_KIND.as_str() =>
            {
                result
                    .scenes
                    .push(serde_json::from_slice(&payload).expect("complete scene payload"));
            }
            Ok(Some(ServerEvent::PluginBusEvent { kind, payload }))
                if kind == RUNTIME_APPEARANCE_STATE_KIND.as_str() =>
            {
                let runtime: RuntimeAppearance =
                    serde_json::from_slice(&payload).expect("complete appearance payload");
                result.appearances.push(runtime.into());
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!(
                "presentation connection closed: {:?}",
                client.disconnect_reason()
            ),
            Err(_) => return result,
        }
    }
}

fn quiet(events: &PresentationEvents) {
    assert!(
        events.scenes.is_empty(),
        "unexpected scene delivery: {events:?}"
    );
    assert!(
        events.appearances.is_empty(),
        "unexpected appearance delivery: {events:?}"
    );
}

fn hydrated(
    events: &PresentationEvents,
    scene: &DecorationScene,
    appearance: &theme_state::Appearance,
) {
    assert_eq!(
        events.scenes.last(),
        Some(scene),
        "retained scene must match owner snapshot"
    );
    assert_eq!(
        events.appearances.last(),
        Some(appearance),
        "retained appearance must match owner snapshot"
    );
    assert!(
        events.scenes.iter().all(|value| value == scene),
        "foreign or obsolete scene: {events:?}"
    );
    assert!(
        events.appearances.iter().all(|value| value == appearance),
        "foreign or obsolete appearance: {events:?}"
    );
}

#[tokio::test]
async fn real_connections_keep_layout_theme_and_retained_lifetimes_private() {
    let mut server = support::ServerEnv::new("view-presentation-isolation");
    server.start();
    let mut a = connect(&server).await;
    let mut b = connect(&server).await;
    let pane = Uuid::new_v4();
    let life_a = Uuid::new_v4();
    let life_b = Uuid::new_v4();

    // Initializing through the real theme service must also reach the real decoration plugin.
    let baseline = current(&mut a).await;
    let baseline_appearance = appearance(&mut a).await;
    let b_initial = current(&mut b).await;
    assert_eq!(b_initial.selection, baseline.selection);
    let a_selected = select(&mut a, baseline.revision, "minimal").await;
    let b_selected = select(&mut b, b_initial.revision, "hacker").await;
    layout(&mut a, life_a, pane, 20, 1).await;
    layout(&mut b, life_b, pane, 40, 1).await;
    let a_scene = view::scene(&mut a).await.expect("owner A scene");
    let b_scene = view::scene(&mut b).await.expect("owner B scene");
    assert_eq!(a_scene.surfaces[&pane].rect.w, 20);
    assert_eq!(b_scene.surfaces[&pane].rect.w, 40);
    let a_appearance = appearance(&mut a).await;
    let b_appearance = appearance(&mut b).await;
    assert_ne!(
        a_appearance, b_appearance,
        "fixtures must have distinguishable themes"
    );

    // Publish BEFORE event push: hydration must come from the real recipient retention table.
    a.enable_event_push().await.expect("enable A push");
    b.enable_event_push().await.expect("enable B push");
    hydrated(&events(&mut a).await, &a_scene, &a_appearance);
    hydrated(&events(&mut b).await, &b_scene, &b_appearance);

    // Read/no-op operations must not repaint either connection.
    assert_eq!(current(&mut a).await, a_selected);
    assert_eq!(appearance(&mut a).await, a_appearance);
    assert_eq!(view::scene(&mut a).await.expect("query scene"), a_scene);
    layout(&mut a, life_a, pane, 20, 2).await;
    quiet(&events(&mut a).await);
    quiet(&events(&mut b).await);

    // Another owner cannot cancel A's preview token, even though it knows its value.
    let token = theme::client::begin_preview(&mut a, a_selected.revision)
        .await
        .expect("begin preview dispatch")
        .expect("begin owner preview");
    assert!(
        theme::client::cancel_preview(&mut b, token)
            .await
            .expect("foreign cancel dispatch")
            .is_err()
    );
    theme::client::preview(
        &mut a,
        token,
        theme::Selection::Preset {
            name: "hacker".into(),
        },
    )
    .await
    .expect("preview dispatch")
    .expect("preview preset");
    let preview_events = events(&mut a).await;
    assert_eq!(preview_events.appearances.last(), Some(&b_appearance));
    let preview_scene = view::scene(&mut a).await.expect("preview owner scene");
    assert_eq!(preview_events.scenes.last(), Some(&preview_scene));
    assert_eq!(preview_scene.surfaces[&pane].rect.w, 20);
    assert_ne!(
        preview_scene.surfaces[&pane].paint_commands, a_scene.surfaces[&pane].paint_commands,
        "theme preview must reach real decoration output, not appearance alone"
    );
    quiet(&events(&mut b).await);
    theme::client::cancel_preview(&mut a, token)
        .await
        .expect("cancel dispatch")
        .expect("cancel owner preview");
    let restored = events(&mut a).await;
    assert_eq!(restored.appearances.last(), Some(&a_appearance));
    let restored_scene = view::scene(&mut a).await.expect("restored owner scene");
    assert_eq!(restored.scenes.last(), Some(&restored_scene));
    assert_eq!(
        restored_scene.surfaces[&pane].paint_commands, a_scene.surfaces[&pane].paint_commands,
        "cancel must restore real decoration output"
    );
    quiet(&events(&mut b).await);
    assert_eq!(
        view::scene(&mut b).await.expect("unchanged B scene"),
        b_scene
    );
    assert_eq!(current(&mut b).await, b_selected);

    // Mutating geometry changes only A's complete snapshot and advances its output revision.
    layout(&mut a, life_a, pane, 24, 3).await;
    let changed = view::scene(&mut a).await.expect("changed A scene");
    assert!(changed.revision > a_scene.revision);
    assert_eq!(changed.surfaces[&pane].rect.w, 24);
    assert_eq!(events(&mut a).await.scenes.last(), Some(&changed));
    quiet(&events(&mut b).await);
    assert!(
        view::update_layout(&mut a, life_a, 2, surfaces(pane, 99), None)
            .await
            .expect("stale revision dispatch")
            .is_err()
    );
    assert!(
        view::close(&mut b, life_a)
            .await
            .expect("foreign lifetime close dispatch")
            .is_err()
    );
    assert_eq!(
        view::scene(&mut b).await.expect("B survives foreign close"),
        b_scene
    );

    view::close(&mut a, life_a)
        .await
        .expect("close dispatch")
        .expect("close A lifetime");
    let _ = events(&mut a).await; // A may receive the retirement tombstone before clear.
    quiet(&events(&mut b).await);
    assert!(
        view::update_layout(&mut a, life_a, 4, surfaces(pane, 99), None)
            .await
            .expect("retired lifetime dispatch")
            .is_err()
    );
    let replacement = Uuid::new_v4();
    layout(&mut a, replacement, pane, 28, 1).await;
    assert!(
        view::scene(&mut a)
            .await
            .expect("replacement scene")
            .revision
            > changed.revision
    );
    let _ = events(&mut a).await;
    quiet(&events(&mut b).await);
    drop(a);

    // A fresh transport identity must never inherit the previous owner's live preset/preview.
    let mut c = connect(&server).await;
    assert_eq!(current(&mut c).await.selection, baseline.selection);
    assert_eq!(appearance(&mut c).await, baseline_appearance);
    layout(&mut c, Uuid::new_v4(), pane, 32, 1).await;
    let c_scene = view::scene(&mut c).await.expect("fresh owner scene");
    c.enable_event_push()
        .await
        .expect("enable fresh owner push");
    hydrated(&events(&mut c).await, &c_scene, &baseline_appearance);
    quiet(&events(&mut b).await);
    assert_eq!(
        view::scene(&mut b).await.expect("B survives A disconnect"),
        b_scene
    );
    assert_eq!(appearance(&mut b).await, b_appearance);
    drop((b, c));
    server.stop();
}
