//! Token-free loopback policy acceptance; fixtures explicitly select ephemeral ports.
use super::*;
use crate::{AuthMode, BinaryPresentation, Resource, ViewportMode};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn no_auth_request(address: SocketAddr, method: &str, path: &str, headers: &str) -> String {
    format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\n{headers}\r\n")
}

#[test]
fn default_policy_is_fixed_loopback_without_token() {
    let config = HostConfig::new("/unused".into());
    assert_eq!(config.port, 8080);
    assert_eq!(config.auth_mode, AuthMode::None);
    assert_eq!(AuthMode::default(), AuthMode::None);
}

#[tokio::test]
async fn tokenless_output_control_and_origin_guards() {
    let (_directory, mut config) = fixture();
    config.auth_mode = AuthMode::None;
    config.viewport_mode = ViewportMode::Resizable;
    let (publisher, feed) = snapshot_feed(128).unwrap();
    let mut host = start(config, feed).await.unwrap();
    let mut requests = host.take_viewport_requests().unwrap();
    let address = host.address();
    assert_eq!(
        address.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(host.bootstrap_url(), format!("http://{address}/"));
    assert!(host.token.is_none());
    let bytes: Arc<[u8]> = Arc::from(b"blob".as_slice());
    let id: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let resource_path = format!("/resource/{id}");
    publisher
        .publish_representations(
            1,
            Some(Arc::from(*b"{}")),
            BinaryPresentation {
                snapshot: Arc::from(*b"binary"),
                delta: None,
            },
            BTreeMap::from([(
                id,
                Resource {
                    bytes,
                    mime_type: "application/octet-stream".into(),
                },
            )]),
        )
        .unwrap();
    host.set_generation("local-generation").unwrap();
    let origin = format!("Origin: http://{address}\r\n");
    for (path, body) in [
        ("/snapshot", "{}"),
        ("/presentation?version=1", "binary"),
        (resource_path.as_str(), "blob"),
        ("/", "<!doctype html><title>fixture</title>"),
    ] {
        let response = exchange(address, &no_auth_request(address, "GET", path, "")).await;
        status(&response, 200);
        assert!(response.ends_with(body));
        for headers in [
            "Origin: http://evil.example\r\n",
            "Origin: null\r\n",
            "Sec-Fetch-Site: cross-site\r\n",
            "Sec-Fetch-Site: same-site\r\n",
        ] {
            status(
                &exchange(address, &no_auth_request(address, "GET", path, headers)).await,
                403,
            );
        }
        let wrong_host = no_auth_request(address, "GET", path, "")
            .replace(&format!("Host: {address}"), "Host: localhost:8080");
        status(&exchange(address, &wrong_host).await, 403);
    }
    for headers in [
        origin.clone(),
        format!("{origin}Sec-Fetch-Site: same-origin\r\n"),
        format!("{origin}Sec-Fetch-Site: none\r\n"),
    ] {
        status(
            &exchange(
                address,
                &no_auth_request(address, "GET", "/snapshot", &headers),
            )
            .await,
            200,
        );
    }
    status(
        &exchange(address, &no_auth_request(address, "POST", "/view", "")).await,
        403,
    );
    status(
        &exchange(
            address,
            &no_auth_request(
                address,
                "POST",
                "/view",
                &format!("{origin}Sec-Fetch-Site: cross-site\r\n"),
            ),
        )
        .await,
        403,
    );
    let claim = exchange(address, &no_auth_request(address, "POST", "/view", &origin)).await;
    status(&claim, 200);
    let info: serde_json::Value =
        serde_json::from_str(claim.split_once("\r\n\r\n").unwrap().1).unwrap();
    let controller = info["controller"].as_str().unwrap();
    assert_eq!(info["viewport_allowed"], true);
    let body = r#"{"generation":"local-generation","request_id":"1","cols":100,"rows":40}"#;
    let headers = format!(
        "{origin}X-Bmux-Controller: {controller}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    let submit = format!(
        "{}{body}",
        no_auth_request(address, "POST", "/viewport", &headers)
    );
    for bad in [
        submit.replace(&origin, ""),
        submit.replace(&origin, "Origin: http://evil.example\r\n"),
        submit.replace(&origin, &format!("{origin}Sec-Fetch-Site: cross-site\r\n")),
        submit.replace(&format!("Host: {address}"), "Host: evil.example:8080"),
    ] {
        status(&exchange(address, &bad).await, 403);
    }
    status(&exchange(address, &submit).await, 204);
    let geometry = timeout(DEADLINE, requests.recv()).await.unwrap().unwrap();
    assert_eq!((geometry.cols, geometry.rows), (100, 40));
    assert!(requests.is_current(&geometry));
    let release = no_auth_request(
        address,
        "DELETE",
        "/view",
        &format!("{origin}X-Bmux-Controller: {controller}\r\n"),
    );
    status(&exchange(address, &release).await, 204);
    assert!(!requests.is_current(&geometry));
    host.shutdown().await;
}

#[tokio::test]
async fn configured_port_binds_exactly_and_collision_is_reported() {
    let (_directory, mut config) = fixture();
    config.auth_mode = AuthMode::None;
    let occupied = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    config.port = occupied.local_addr().unwrap().port();
    let (publisher, feed) = snapshot_feed(128).unwrap();
    match start(config.clone(), feed.clone()).await {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::AddrInUse),
        Ok(host) => {
            host.shutdown().await;
            panic!("occupied port unexpectedly accepted");
        }
    }
    drop(occupied);
    let expected_port = config.port;
    let host = start(config.clone(), feed.clone()).await.unwrap();
    assert_eq!(host.address().port(), expected_port);
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    match start(config, feed).await {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::AddrInUse),
        Ok(other) => {
            other.shutdown().await;
            panic!("second host must not switch ports");
        }
    }
    host.shutdown().await;
}
