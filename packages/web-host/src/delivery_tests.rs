//! Raw TCP coverage: compressed bodies must not be decoded as UTF-8 by the client.
use super::*;
use crate::{BinaryPresentation, DocumentPublication};
use std::collections::BTreeMap;
use std::io::Read;

async fn fetch(host: &HostHandle, path: &str, encoding: &str) -> (String, Vec<u8>) {
    let request = request(host.address(), "GET", path, &token(host), encoding);
    let mut stream = connect(host.address(), &request).await;
    let mut bytes = Vec::new();
    timeout(DEADLINE, (&mut stream).take(65536).read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.len() < 65536);
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let body = bytes[end..].to_vec();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(length, body.len());
    (headers, body)
}

fn decoded(headers: &str, body: &[u8]) -> Vec<u8> {
    assert!(headers.contains("Vary: Accept-Encoding\r\n"));
    assert!(headers.contains("Content-Encoding: gzip\r\n"));
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(body)
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[tokio::test]
async fn gzip_full_delta_exact_base_repairs_and_unchanged_poll() {
    let (_directory, mut config) = fixture();
    config.limits.poll_timeout = Duration::from_millis(20);
    config.limits.max_snapshot_bytes = 16384;
    let (publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let full: Arc<[u8]> = vec![b'f'; 4096].into();
    let delta: Arc<[u8]> = vec![b'd'; 2048].into();
    publisher
        .publish_representations(
            1,
            None,
            BinaryPresentation {
                snapshot: Arc::clone(&full),
                delta: None,
            },
            BTreeMap::new(),
        )
        .unwrap();
    publisher
        .publish_representations(
            2,
            None,
            BinaryPresentation {
                snapshot: Arc::clone(&full),
                delta: Some((1, Arc::clone(&delta))),
            },
            BTreeMap::new(),
        )
        .unwrap();
    for (query, expected) in [
        ("after=1&base=1", &delta),
        ("after=1&base=0", &full),
        ("after=2&base=0", &full),
        ("after=99&base=99", &full),
    ] {
        let (headers, body) = fetch(
            &host,
            &format!("/presentation?version=1&{query}"),
            "Accept-Encoding: gzip\r\n",
        )
        .await;
        status(&headers, 200);
        assert!(headers.contains("X-Snapshot-Revision: 2\r\n"));
        assert_eq!(decoded(&headers, &body), expected.as_ref());
    }
    let (headers, body) = fetch(
        &host,
        "/presentation?version=1&after=2&base=2",
        "Accept-Encoding: gzip\r\n",
    )
    .await;
    status(&headers, 204);
    assert!(body.is_empty());
    let (headers, body) = fetch(
        &host,
        "/presentation?version=1",
        "Accept-Encoding: gzip;q=0\r\n",
    )
    .await;
    status(&headers, 200);
    assert!(!headers.contains("Content-Encoding:"));
    assert_eq!(body, full.as_ref());
    let (headers, _) = fetch(
        &host,
        "/presentation?version=1",
        "Accept-Encoding: gzip;q=0, identity;q=0\r\n",
    )
    .await;
    status(&headers, 406);
    timeout(DEADLINE, async {
        while host.diagnostics().delivery.identity_responses < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let counters = host.diagnostics().delivery;
    assert_eq!(counters.gzip_responses, 4);
    assert_eq!(counters.identity_responses, 1);
    assert_eq!(counters.delta_responses, 1);
    assert_eq!(counters.full_responses, 4);
    assert!(counters.transport_bytes < counters.identity_bytes);
    // Producer closure is covered directly by http::tests::full_repair_handles_future_revision_and_closed_feed;
    // the live listener may cancel before it can send a 503.
    host.shutdown().await;
}

#[tokio::test]
async fn json_snapshot_negotiation_preserves_bytes_and_small_identity_fallback() {
    let (_directory, mut config) = fixture();
    config.limits.max_snapshot_bytes = 16384;
    let (publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let json = serde_json::to_vec(&vec!["repeated text"; 256]).unwrap();
    publisher.publish(1, json.clone().into()).unwrap();
    let (headers, body) = fetch(&host, "/snapshot", "Accept-Encoding: gzip\r\n").await;
    status(&headers, 200);
    assert_eq!(decoded(&headers, &body), json);
    let (headers, body) = fetch(
        &host,
        "/snapshot",
        "Accept-Encoding: gzip;q=0.5, identity;q=1\r\n",
    )
    .await;
    status(&headers, 200);
    assert!(!headers.contains("Content-Encoding:"));
    assert_eq!(body, json);
    publisher.publish(2, Arc::from(*b"{}")).unwrap();
    let (headers, body) = fetch(&host, "/snapshot", "Accept-Encoding: gzip\r\n").await;
    status(&headers, 200);
    assert_eq!(body, b"{}");
    assert!(!headers.contains("Content-Encoding:"));
    let (headers, _) = fetch(
        &host,
        "/snapshot",
        "Accept-Encoding: gzip, identity;q=0\r\n",
    )
    .await;
    status(&headers, 406);
    host.shutdown().await;
}

#[tokio::test]
async fn document_gzip_preserves_v1_json_and_png_resources_remain_identity() {
    let (_directory, mut config) = fixture();
    config.limits.max_snapshot_bytes = 16384;
    let (publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let mut png = vec![0; 2048];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let id = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&png)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    publisher
        .publish_document(
            1,
            DocumentPublication {
                generation: "generation-one".into(),
                revision: 1,
                capability: "test.v1".into(),
                content: vec![b'x'; 2048].into(),
            },
            BTreeMap::from([(
                id.clone(),
                crate::Resource {
                    bytes: png.clone().into(),
                    mime_type: "image/png".into(),
                },
            )]),
        )
        .unwrap();
    let (identity_headers, identity) = fetch(&host, "/document?version=1", "").await;
    status(&identity_headers, 200);
    assert!(!identity_headers.contains("Content-Encoding:"));
    let (headers, body) = fetch(
        &host,
        "/document?version=1&after=99",
        "Accept-Encoding: gzip\r\n",
    )
    .await;
    assert_eq!(decoded(&headers, &body), identity);
    let envelope: serde_json::Value = serde_json::from_slice(&identity).unwrap();
    assert_eq!(envelope["version"], 1);
    assert!(envelope["content"].is_array());
    let (headers, body) = fetch(
        &host,
        &format!("/resource/{id}"),
        "Accept-Encoding: gzip\r\n",
    )
    .await;
    status(&headers, 200);
    assert!(!headers.contains("Content-Encoding:"));
    assert_eq!(body, png);
    timeout(DEADLINE, async {
        while host.diagnostics().delivery.document_responses < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(host.diagnostics().delivery.document_responses, 2);
    host.shutdown().await;
}
