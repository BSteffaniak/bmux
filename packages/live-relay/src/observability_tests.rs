#[tokio::test]
async fn publisher_repair_reason_requires_selected_subprotocol() {
    for offered in [Some("bmux-observe-v1"), None] {
        let fixture = Fixture::new().await;
        let authority = fixture.app.relay.lock().await.authority.clone();
        let mut request = format!("{}/v1/publisher/ws", fixture.url)
            .into_client_request()
            .unwrap();
        for (name, value) in [
            ("authorization", "Bearer publisher-test-key".to_owned()),
            ("x-bmux-epoch", authority.epoch.unwrap().to_string()),
            ("x-bmux-fence", authority.fence.to_string()),
        ] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
        if let Some(protocol) = offered {
            request
                .headers_mut()
                .insert("sec-websocket-protocol", protocol.parse().unwrap());
        }
        let (mut client, response) = connect_async(request).await.unwrap();
        let negotiated = offered == Some("bmux-observe-v1");
        assert_eq!(
            response.headers().get("sec-websocket-protocol").is_some(),
            negotiated
        );
        client
            .send(ClientMessage::Binary(
                b"BMWP\x05malformed-private-content".to_vec().into(),
            ))
            .await
            .unwrap();
        let reply = message(&mut client).await.into_text().unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        if negotiated {
            assert_eq!(
                reply,
                json!({"type":"repair","reason":"malformed","observability_version":1})
            );
        } else {
            assert_eq!(reply, json!({"type":"repair"}));
        }
    }
}

#[tokio::test]
async fn blocked_socket_write_is_not_reported_as_transport_error() {
    assert_eq!(
        bounded_send(
            Duration::from_millis(1),
            std::future::pending::<std::result::Result<(), ()>>()
        )
        .await,
        Err(Disconnect::SendTimeout)
    );
    assert_eq!(
        bounded_send(Duration::from_secs(1), std::future::ready(Err(()))).await,
        Err(Disconnect::SendFailed)
    );
    assert_eq!(
        bounded_send(Duration::from_secs(1), std::future::ready(Ok::<(), ()>(()))).await,
        Ok(())
    );
}

// Included in the existing WebSocket fixture module: no external runtime needed.
#[tokio::test]
async fn diagnostics_requires_independent_auth_and_explicit_version_and_redacts() {
    let fixture = Fixture::new().await;
    let mut headers = HeaderMap::new();
    assert_eq!(
        diagnostics(State(fixture.app.clone()), headers.clone())
            .await
            .unwrap_err(),
        StatusCode::UNAUTHORIZED
    );
    headers.insert(
        "authorization",
        "Bearer publisher-test-key".parse().unwrap(),
    );
    headers.insert("x-bmux-diagnostics-version", "1".parse().unwrap());
    assert_eq!(
        diagnostics(State(fixture.app.clone()), headers.clone())
            .await
            .unwrap_err(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        metrics(State(fixture.app.clone()), headers.clone())
            .await
            .unwrap_err(),
        StatusCode::UNAUTHORIZED
    );
    headers.insert("authorization", "Bearer metrics-test-key".parse().unwrap());
    headers.remove("x-bmux-diagnostics-version");
    assert_eq!(
        diagnostics(State(fixture.app.clone()), headers.clone())
            .await
            .unwrap_err(),
        StatusCode::NOT_ACCEPTABLE
    );
    headers.insert("x-bmux-diagnostics-version", "2".parse().unwrap());
    assert_eq!(
        diagnostics(State(fixture.app.clone()), headers.clone())
            .await
            .unwrap_err(),
        StatusCode::NOT_ACCEPTABLE
    );
    headers.insert("x-bmux-diagnostics-version", "1".parse().unwrap());
    let connection = fixture.app.telemetry.connection(Role::Viewer);
    connection.sent(7);
    let authority = fixture.app.relay.lock().await.authority.clone();
    let response = diagnostics(State(fixture.app.clone()), headers.clone())
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(
        value["build"]["binary_wire_version"],
        bmux_web_renderer::BINARY_WIRE_VERSION
    );
    assert_eq!(value["progress"]["connections"][0]["pending_revision"], "7");
    let rendered = String::from_utf8(bytes.to_vec()).unwrap();
    for forbidden in [
        "publisher-test-key".to_owned(),
        "metrics-test-key".to_owned(),
        authority.epoch.unwrap().to_string(),
        authority.publisher_id.unwrap().to_string(),
        "fence".to_owned(),
        "ws-test".to_owned(),
        "generation".to_owned(),
    ] {
        assert!(!rendered.contains(&forbidden));
    }
    let response = metrics(State(fixture.app.clone()), headers.clone())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        !String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("connection_id")
    );
    assert_eq!(
        publisher_auth(&fixture.app, &headers),
        Err(StatusCode::UNAUTHORIZED)
    );
}

#[tokio::test]
async fn stale_revision_malformed_delta_and_full_repair_keep_legacy_envelopes() {
    let fixture = Fixture::new().await;
    let mut publisher = fixture.publisher().await;
    let first = frame(1);
    let second = frame(2);
    publisher
        .send(ClientMessage::Binary(encode_binary(&first).unwrap().into()))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let cases = [
        (encode_binary(&first).unwrap(), "stale_revision"),
        (encode_delta(&second, &first).unwrap(), "missing_delta_base"),
        (b"BMWP\x05malformed-private-content".to_vec(), "malformed"),
    ];
    for (bytes, reason) in cases {
        publisher
            .send(ClientMessage::Binary(bytes.into()))
            .await
            .unwrap();
        let response = message(&mut publisher).await.into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            json!({"type":"repair"})
        );
        assert!(fixture.app.telemetry.metrics().contains(&format!(
            "event=\"ingest_rejected\",reason=\"{reason}\"}} 1"
        )));
        assert_eq!(
            fixture
                .app
                .relay
                .lock()
                .await
                .delivery
                .as_ref()
                .unwrap()
                .frame
                .revision(),
            1
        );
    }
    publisher
        .send(ClientMessage::Binary(
            encode_binary(&second).unwrap().into(),
        ))
        .await
        .unwrap();
    let response = message(&mut publisher).await.into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap(),
        json!({"type":"ack","generation":"ws-test","revision":"2"})
    );
    assert_eq!(
        fixture.app.telemetry.diagnostics()["admitted_revision"],
        "2"
    );
    assert!(
        !fixture
            .app
            .telemetry
            .metrics()
            .contains("malformed-private-content")
    );
}

#[tokio::test]
async fn stale_ack_has_specific_disconnect_reason() {
    let fixture = Fixture::new().await;
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(
            encode_binary(&frame(1)).unwrap().into(),
        ))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let mut viewer = fixture.viewer().await;
    let _ = binary(&mut viewer).await;
    ack(&mut viewer, 2).await;
    expect_throttled_disconnect(&mut viewer).await;
    assert!(
        fixture
            .app
            .telemetry
            .metrics()
            .contains("event=\"viewer\",reason=\"invalid_ack\"} 1")
    );
}

#[tokio::test]
async fn lost_viewer_ack_disconnects_instead_of_queuing_and_has_reason() {
    let fixture = Fixture::new().await;
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(
            encode_binary(&frame(1)).unwrap().into(),
        ))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let mut viewer = fixture.viewer().await;
    let _ = binary(&mut viewer).await;
    // Continue transport liveness but deliberately lose the application ACK.
    timeout(IO_TIMEOUT + Duration::from_secs(2), async {
        loop {
            match viewer.next().await {
                Some(Ok(ClientMessage::Ping(bytes))) => {
                    viewer.send(ClientMessage::Pong(bytes)).await.unwrap();
                }
                Some(Ok(ClientMessage::Close(_))) | Some(Err(_)) | None => break,
                _ => panic!("pending frame must not be resent without ACK or repair"),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        fixture
            .app
            .telemetry
            .metrics()
            .contains("event=\"viewer\",reason=\"ack_timeout\"} 1")
    );
}

#[tokio::test]
async fn ingress_throttle_has_bounded_disconnect_reason() {
    let fixture = Fixture::new().await;
    // Router holds a clone; exhaust the shared rate rather than mutating its limits.
    let mut publisher = fixture.publisher().await;
    {
        let mut rate = fixture.app.ingress.lock().await;
        rate.started = Instant::now();
        rate.messages = 60;
    }
    publisher
        .send(ClientMessage::Binary(
            encode_binary(&frame(1)).unwrap().into(),
        ))
        .await
        .unwrap();
    expect_throttled_disconnect(&mut publisher).await;
    assert!(
        fixture
            .app
            .telemetry
            .metrics()
            .contains("event=\"publisher\",reason=\"ingress_throttled\"} 1")
    );
}
