//! Native TCP policy for configurable IPv4/IPv6 listeners.
use super::*;
use crate::AuthMode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[test]
fn network_policy_rejects_unsafe_or_ambiguous_settings() {
    let (_directory, base) = fixture();
    for (bind, public, auth) in [
        ("0.0.0.0", None, AuthMode::Token),
        ("::", None, AuthMode::Token),
        ("0.0.0.0", Some("::1"), AuthMode::Token),
        ("::", Some("0.0.0.0"), AuthMode::Token),
        ("0.0.0.0", Some("224.0.0.1"), AuthMode::Token),
        ("224.0.0.1", None, AuthMode::Token),
        ("127.0.0.1", Some("192.0.2.1"), AuthMode::Token),
    ] {
        let mut config = base.clone();
        config.host = bind.parse().unwrap();
        config.public_host = public.map(|ip| ip.parse().unwrap());
        config.auth_mode = auth;
        assert_eq!(
            crate::validate_network(&config).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    for bind in [
        "127.0.0.1",
        "::1",
        "192.0.2.1",
        "2001:db8::1",
        "::ffff:127.0.0.1",
    ] {
        for auth in [AuthMode::None, AuthMode::Token] {
            let mut config = base.clone();
            config.host = bind.parse().unwrap();
            config.auth_mode = auth;
            crate::validate_network(&config).unwrap();
        }
    }
}

async fn verify_listener(bind: IpAddr, public: IpAddr) {
    let (_directory, mut config) = fixture();
    config.host = bind;
    config.public_host = Some(public);
    config.viewport_mode = crate::ViewportMode::Resizable;
    let (publisher, feed) = snapshot_feed(128).unwrap();
    let mut host = start(config, feed).await.unwrap();
    let requests = host.take_viewport_requests().unwrap();
    host.set_generation("network-generation").unwrap();
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    assert_eq!(host.address().ip(), bind);
    let address = SocketAddr::new(public, host.address().port());
    let url = host.bootstrap_url();
    let prefix = format!("http://{address}/#token=");
    assert!(url.starts_with(&prefix));
    let capability = token(&host);
    status(
        &exchange(
            address,
            &request(address, "GET", "/snapshot", &capability, ""),
        )
        .await,
        200,
    );
    status(
        &exchange(address, &request(address, "GET", "/snapshot", "bad", "")).await,
        401,
    );
    let origin = format!("Origin: http://{address}\r\n");
    status(
        &exchange(
            address,
            &request(address, "POST", "/view", &capability, &origin),
        )
        .await,
        200,
    );
    for headers in [
        "Origin: http://evil.example\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
    ] {
        status(
            &exchange(
                address,
                &request(address, "GET", "/snapshot", &capability, headers),
            )
            .await,
            403,
        );
    }
    let bad_host = request(address, "GET", "/snapshot", &capability, "")
        .replace(&format!("Host: {address}"), "Host: evil.example");
    status(&exchange(address, &bad_host).await, 403);
    drop(requests);
    host.shutdown().await;
    assert!(TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn wildcard_ipv4_serves_advertised_host_with_strict_auth_and_origin() {
    verify_listener(Ipv4Addr::UNSPECIFIED.into(), Ipv4Addr::LOCALHOST.into()).await;
}

#[tokio::test]
async fn ipv6_loopback_formats_url_and_accepts_exact_origin() {
    verify_listener(Ipv6Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()).await;
}

#[tokio::test]
async fn wildcard_token_free_serves_without_bearer_and_preserves_trust_checks() {
    let (_directory, mut config) = fixture();
    config.host = Ipv4Addr::UNSPECIFIED.into();
    config.public_host = Some(Ipv4Addr::LOCALHOST.into());
    config.auth_mode = AuthMode::None;
    config.viewport_mode = crate::ViewportMode::Resizable;
    let (publisher, feed) = snapshot_feed(128).unwrap();
    let host = start(config, feed).await.unwrap();
    host.set_generation("token-free-network").unwrap();
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    let address = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), host.address().port());
    assert_eq!(host.bootstrap_url(), format!("http://{address}/"));
    let get = format!("GET /snapshot HTTP/1.1\r\nHost: {address}\r\n\r\n");
    status(&exchange(address, &get).await, 200);
    let claim =
        format!("POST /view HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\n\r\n");
    status(&exchange(address, &claim).await, 200);
    status(
        &exchange(
            address,
            &claim.replace(
                &format!("Origin: http://{address}"),
                "Origin: http://evil.example",
            ),
        )
        .await,
        403,
    );
    status(
        &exchange(
            address,
            &get.replace(&format!("Host: {address}"), "Host: evil.example"),
        )
        .await,
        403,
    );
    status(
        &exchange(
            address,
            &get.replace("\r\n\r\n", "\r\nSec-Fetch-Site: cross-site\r\n\r\n"),
        )
        .await,
        403,
    );
    host.shutdown().await;
}
