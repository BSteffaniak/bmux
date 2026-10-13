//! Native TCP policy for configurable IPv4/IPv6 listeners.
use super::*;
use crate::AuthMode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[test]
fn authority_syntax_and_optional_host_policy() {
    for host in [
        "localhost",
        "bmux.lan:8080",
        "192.168.1.1:8080",
        "[::1]:8080",
        "[::1]",
    ] {
        assert!(crate::http::valid_authority(host), "{host}");
    }
    for host in [
        "",
        "a/b",
        "user@host",
        "host:0",
        "host:65536",
        "host:",
        "host:80:90",
        "::1",
        "[bad]:80",
        "a..b",
        "-bad",
        "bad-",
        "a?b",
        "host\\evil",
        "host: 80",
    ] {
        assert!(!crate::http::valid_authority(host), "{host}");
    }
    let (_directory, mut config) = fixture();
    config.host = Ipv4Addr::UNSPECIFIED.into();
    crate::validate_network(&config).unwrap();
    config.host = Ipv6Addr::UNSPECIFIED.into();
    crate::validate_network(&config).unwrap();
    config.allowed_hosts = vec!["bad/path".into()];
    assert!(crate::validate_network(&config).is_err());
}

#[tokio::test]
async fn request_hosts_accept_aliases_and_optional_allowlist_is_independent_of_display() {
    for restricted in [false, true] {
        let (_directory, mut config) = fixture();
        config.host = Ipv4Addr::UNSPECIFIED.into();
        config.public_host = Some("192.0.2.10".parse().unwrap());
        config.auth_mode = AuthMode::None;
        config.viewport_mode = crate::ViewportMode::Resizable;
        if restricted {
            config.allowed_hosts = vec!["bmux.lan:8080".into(), "[::1]:8080".into()];
        }
        let (publisher, feed) = snapshot_feed(128).unwrap();
        let host = start(config, feed).await.unwrap();
        host.set_generation("aliases").unwrap();
        publisher.publish(1, Arc::from(*b"{}")).unwrap();
        assert!(host.bootstrap_url().starts_with("http://192.0.2.10:"));
        let address = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), host.address().port());
        for authority in ["bmux.lan:8080", "[::1]:8080", "other.lan:8080"] {
            let req = format!(
                "GET /snapshot HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://{authority}\r\n\r\n"
            );
            status(
                &exchange(address, &req).await,
                if restricted && authority == "other.lan:8080" {
                    403
                } else {
                    200
                },
            );
            status(
                &exchange(
                    address,
                    &req.replace(
                        &format!("Origin: http://{authority}"),
                        "Origin: http://foreign.lan:8080",
                    ),
                )
                .await,
                403,
            );
        }
        let claim =
            "POST /view HTTP/1.1\r\nHost: bmux.lan:8080\r\nOrigin: http://bmux.lan:8080\r\n\r\n";
        status(&exchange(address, claim).await, 200);
        status(
            &exchange(
                address,
                &claim.replace("Origin: http://bmux.lan:8080\r\n", ""),
            )
            .await,
            403,
        );
        host.shutdown().await;
    }
}

#[test]
fn network_policy_rejects_unsafe_or_ambiguous_settings() {
    let (_directory, base) = fixture();
    for (bind, public, auth) in [
        ("::", Some("0.0.0.0"), AuthMode::Token),
        ("0.0.0.0", Some("224.0.0.1"), AuthMode::Token),
        ("224.0.0.1", None, AuthMode::Token),
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
    status(&exchange(address, &bad_host).await, 200);
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
        200,
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
