//! Checked attached-runtime workflows against an explicitly isolated daemon.
#[allow(dead_code)] // Shared fixture also supports gateway and recording tests.
mod support;

use std::time::Duration;

use bmux_pane_runtime_plugin_api::{
    attach_runtime_commands::SessionSelector, attach_runtime_commands::client as attach,
    attach_runtime_state::client as state, pane_runtime_commands::client as panes,
    pane_runtime_state::client as pane_state,
};

async fn connect(endpoint: &bmux_ipc::IpcEndpoint) -> bmux_client::BmuxClient {
    let mut client = bmux_client::BmuxClient::connect(
        endpoint,
        Duration::from_secs(5),
        "bterm-interaction-test",
    )
    .await
    .unwrap();
    client.require_service_contracts().unwrap();
    client
}

async fn capture_through_background_route(
    owner: &mut bmux_client::StreamingBmuxClient,
    session: uuid::Uuid,
    pane: uuid::Uuid,
) -> bmux_pane_runtime_plugin_api::attach_runtime_state::HistoryCaptureV1 {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let mut route =
        bmux_plugin::AsyncServiceClient::bind(bmux_plugin::ASYNC_SERVICE_ROUTE_V1, sender)
            .unwrap()
            .with_service_contracts(
                bmux_plugin_sdk::TypedDispatchClient::requires_service_contracts(owner),
            );
    let (capture, ()) = tokio::join!(
        state::attach_history_capture_v1(&mut route, session, pane),
        async {
            let request = receiver.recv().await.unwrap();
            let response = owner
                .invoke_service_raw(
                    request.capability.clone(),
                    request.kind,
                    request.interface_id.clone(),
                    request.operation.clone(),
                    request.payload.clone(),
                )
                .await
                .map_err(|error| error.to_string());
            request.respond(response).unwrap();
        }
    );
    capture.unwrap().unwrap()
}

#[tokio::test]
async fn checked_capture_input_and_cross_client_denials_survive_streaming_upgrade() {
    let mut server = support::ServerEnv::new("bterm-interactions");
    server.write_config("[general]\ndefault_shell = '/bin/bash'\n");
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut owner = connect(&endpoint).await;
    let session = panes::new_session_with_runtime(&mut owner, Some("checked-interactions".into()))
        .await
        .unwrap()
        .unwrap()
        .session_id;
    let grant = attach::attach_session(
        &mut owner,
        SessionSelector {
            id: Some(session),
            name: None,
        },
        true,
    )
    .await
    .unwrap()
    .unwrap();
    attach::attach_open(&mut owner, session, grant.token)
        .await
        .unwrap()
        .unwrap();
    let pane = pane_state::list_panes(&mut owner, Some(session))
        .await
        .unwrap()
        .unwrap()
        .panes[0]
        .id;
    let owner_id = bmux_clients_plugin_api::clients_state::client::current_client(&mut owner)
        .await
        .unwrap()
        .unwrap()
        .id;
    let mut owner = bmux_client::StreamingBmuxClient::from_client(owner).unwrap();
    assert!(bmux_plugin_sdk::TypedDispatchClient::requires_service_contracts(&owner));
    panes::pane_direct_input(
        &mut owner,
        session,
        pane,
        b"printf 'selection test\\n'\n".to_vec(),
    )
    .await
    .unwrap()
    .unwrap();
    let capture = capture_through_background_route(&mut owner, session, pane).await;
    let mut stranger = connect(&endpoint).await;
    assert!(
        state::attach_history_capture_v1(&mut stranger, session, pane)
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        panes::pane_direct_input(&mut stranger, session, pane, b"x".to_vec())
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        state::attach_main_row_slice_v1(
            &mut stranger,
            session,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            0,
            80,
            4096,
        )
        .await
        .unwrap()
        .is_err()
    );
    assert!(
        state::attach_main_row_slice_v1(
            &mut owner,
            session,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            0,
            80,
            4096,
        )
        .await
        .unwrap()
        .is_ok()
    );
    // A grant change must update write admission on an already-open stream.
    let mut policy = connect(&endpoint).await;
    bmux_permissions_plugin_api::permissions_commands::client::grant(
        &mut policy,
        session.to_string(),
        owner_id.to_string(),
        "observer".to_string(),
    )
    .await
    .unwrap();
    assert!(
        panes::pane_direct_input(&mut owner, session, pane, b"x".to_vec())
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        state::attach_main_row_slice_v1(
            &mut owner,
            session,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            0,
            80,
            4096,
        )
        .await
        .unwrap()
        .is_ok()
    );
    bmux_permissions_plugin_api::permissions_commands::client::grant(
        &mut policy,
        session.to_string(),
        owner_id.to_string(),
        "writer".to_string(),
    )
    .await
    .unwrap();
    panes::pane_direct_input(&mut owner, session, pane, b"i".to_vec())
        .await
        .unwrap()
        .unwrap();
    let selector = bmux_tabs_plugin_api::tabs_commands::Selector {
        id: Some(session),
        name: None,
        index: None,
    };
    let target = bmux_tabs_plugin_api::tabs_commands::Selector {
        id: Some(pane),
        name: None,
        index: None,
    };
    bmux_tabs_plugin_api::tabs_commands::client::focus_pane_by_selector(
        &mut owner,
        Some(selector.clone()),
        target.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let split = bmux_tabs_plugin_api::tabs_commands::client::split_pane(
        &mut owner,
        Some(selector.clone()),
        Some(target.clone()),
        bmux_tabs_plugin_api::tabs_commands::PaneDirection::Horizontal,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(split.pane_id.is_some());
    for expected in [true, false] {
        let zoom = bmux_tabs_plugin_api::tabs_commands::client::zoom_pane(
            &mut owner,
            Some(selector.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(zoom.zoomed, expected);
    }
    bmux_tabs_plugin_api::tabs_commands::client::resize_pane(
        &mut owner,
        Some(selector),
        Some(target),
        bmux_tabs_plugin_api::tabs_commands::PaneResizeDirection::Right,
        1,
    )
    .await
    .unwrap()
    .unwrap();
    drop(policy);
    let released =
        state::attach_pane_scrollback_unpin(&mut owner, session, pane, capture.pin.pin_id)
            .await
            .unwrap()
            .unwrap();
    assert!(released.released);
    assert!(
        state::attach_main_row_slice_v1(
            &mut owner,
            session,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            0,
            80,
            4096,
        )
        .await
        .unwrap()
        .is_err()
    );
    drop((owner, stranger));
    server.stop();
}
