use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::assets::LoadedAsset;
use crate::{Limits, Snapshot, SnapshotFeed};

pub(crate) struct State {
    pub assets: BTreeMap<String, LoadedAsset>,
    pub limits: Limits,
    pub token: String,
    pub authority: String,
    pub origin: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Asset(&'a str),
    Snapshot(Option<u64>),
}

fn route(target: &str) -> Result<Route<'_>, u16> {
    if target == "/snapshot" {
        return Ok(Route::Snapshot(None));
    }
    if let Some(value) = target.strip_prefix("/snapshot?after=") {
        if value.is_empty() || value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(400);
        }
        return value
            .parse()
            .map(|value| Route::Snapshot(Some(value)))
            .map_err(|_| 400);
    }
    if target == "/"
        || target.strip_prefix('/').is_some_and(|name| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
    {
        return Ok(Route::Asset(target));
    }
    Err(404)
}

fn token_matches(provided: &str, token: &str) -> bool {
    let Some(provided) = provided.strip_prefix("Bearer ") else {
        return false;
    };
    if provided.len() != token.len() {
        return false;
    }
    provided
        .bytes()
        .zip(token.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

/// Strict HTTP/1.1 subset: one GET, no body, no upgrade, no pipelining, no URL credentials.
fn parse<'a>(bytes: &'a [u8], state: &State) -> Result<Route<'a>, u16> {
    if bytes.len() > state.limits.max_header_bytes || !bytes.ends_with(b"\r\n\r\n") {
        return Err(400);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| 400_u16)?;
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || b" \r\n".contains(&byte))
    {
        return Err(400);
    }
    let mut lines = text[..text.len() - 4].split("\r\n");
    let request = lines.next().ok_or(400_u16)?;
    let mut parts = request.split(' ');
    let method = parts.next().ok_or(400_u16)?;
    let target = parts.next().ok_or(400_u16)?;
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(400);
    }
    if method != "GET" {
        return Err(405);
    }
    let mut headers = BTreeMap::new();
    for (index, line) in lines.enumerate() {
        if index >= 48 {
            return Err(431);
        }
        let (name, value) = line.split_once(':').ok_or(400_u16)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(400);
        }
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(400);
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim())
            .is_some()
        {
            return Err(400);
        }
    }
    if headers.get("host").copied() != Some(state.authority.as_str()) {
        return Err(403);
    }
    if headers
        .get("origin")
        .is_some_and(|origin| *origin != state.origin)
    {
        return Err(403);
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|site| !matches!(*site, "same-origin" | "none"))
    {
        return Err(403);
    }
    if headers.contains_key("transfer-encoding")
        || headers.contains_key("upgrade")
        || headers.contains_key("expect")
        || headers
            .get("content-length")
            .is_some_and(|length| *length != "0")
    {
        return Err(400);
    }
    let route = route(target)?;
    if matches!(route, Route::Snapshot(_))
        && !headers
            .get("authorization")
            .is_some_and(|value| token_matches(value, &state.token))
    {
        return Err(401);
    }
    Ok(route)
}

async fn read_request(stream: &mut TcpStream, state: &State) -> Result<Vec<u8>, u16> {
    let mut bytes = Vec::with_capacity(state.limits.max_header_bytes.min(1024));
    let mut chunk = [0_u8; 1024];
    loop {
        let remaining = state.limits.max_header_bytes - bytes.len();
        if remaining == 0 {
            return Err(431);
        }
        let capacity = remaining.min(chunk.len());
        let count = stream
            .read(&mut chunk[..capacity])
            .await
            .map_err(|_| 400_u16)?;
        if count == 0 {
            return Err(400);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            if end + 4 != bytes.len() {
                return Err(400);
            }
            return Ok(bytes);
        }
    }
}

async fn latest(feed: &mut SnapshotFeed, after: Option<u64>) -> Result<Arc<Snapshot>, u16> {
    loop {
        // A retained value is not evidence that its producer is still alive.
        // Check closure before serving either immediate or repaired snapshots.
        if feed.receiver.has_changed().is_err() {
            return Err(503);
        }
        // borrow_and_update avoids losing a publication between inspecting and waiting.
        if let Some(snapshot) = feed.receiver.borrow_and_update().clone() {
            if after != Some(snapshot.revision) {
                return Ok(snapshot);
            }
        } else if after.is_none() {
            return Err(503);
        }
        feed.receiver.changed().await.map_err(|_| 503_u16)?;
    }
}

async fn response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    revision: Option<u64>,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    };
    let revision_header = revision.map_or_else(String::new, |revision| {
        format!("X-Snapshot-Revision: {revision}\r\n")
    });
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n{revision_header}Connection: close\r\nCache-Control: no-store\r\nPragma: no-cache\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; style-src 'self'; img-src 'self' blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

pub(crate) async fn serve(mut stream: TcpStream, state: Arc<State>, mut feed: SnapshotFeed) {
    let request = timeout(
        state.limits.header_timeout,
        read_request(&mut stream, &state),
    )
    .await;
    let route = match &request {
        Ok(Ok(bytes)) => parse(bytes, &state),
        Ok(Err(status)) => Err(*status),
        Err(_) => Err(408),
    };
    match route {
        Ok(Route::Asset(path)) => {
            if let Some(asset) = state.assets.get(path) {
                let _ = timeout(
                    state.limits.write_timeout,
                    response(
                        &mut stream,
                        200,
                        asset.content_type.header(),
                        &asset.bytes,
                        None,
                    ),
                )
                .await;
            } else {
                error(&mut stream, &state, 404).await;
            }
        }
        Ok(Route::Snapshot(after)) => {
            match timeout(state.limits.poll_timeout, latest(&mut feed, after)).await {
                Ok(Ok(snapshot)) => {
                    let _ = timeout(
                        state.limits.write_timeout,
                        response(
                            &mut stream,
                            200,
                            "application/json",
                            &snapshot.bytes,
                            Some(snapshot.revision),
                        ),
                    )
                    .await;
                }
                Ok(Err(status)) => error(&mut stream, &state, status).await,
                Err(_) => error(&mut stream, &state, 204).await,
            }
        }
        Err(status) => error(&mut stream, &state, status).await,
    }
}

async fn error(stream: &mut TcpStream, state: &State, status: u16) {
    let _ = timeout(
        state.limits.write_timeout,
        response(stream, status, "text/plain; charset=utf-8", &[], None),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            assets: BTreeMap::new(),
            limits: Limits::default(),
            token: "a".repeat(64),
            authority: "127.0.0.1:1234".into(),
            origin: "http://127.0.0.1:1234".into(),
        }
    }

    fn request(method: &str, target: &str, headers: &str) -> Vec<u8> {
        format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n{headers}\r\n").into_bytes()
    }

    #[test]
    fn authentication_and_origin_host_enforcement() {
        let state = state();
        assert_eq!(parse(&request("GET", "/snapshot", ""), &state), Err(401));
        let auth = format!("Authorization: Bearer {}\r\n", state.token);
        let bytes = request("GET", "/snapshot?after=7", &auth);
        assert_eq!(parse(&bytes, &state), Ok(Route::Snapshot(Some(7))));
        for header in [
            "Origin: null\r\n",
            "Origin: http://evil.example\r\n",
            "Sec-Fetch-Site: cross-site\r\n",
            "Sec-Fetch-Site: same-site\r\n",
        ] {
            assert_eq!(parse(&request("GET", "/", header), &state), Err(403));
        }
        for host in [
            "localhost:1234",
            "evil.example:1234",
            "127.0.0.1",
            "127.0.0.1:99",
        ] {
            let bytes = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert_eq!(parse(bytes.as_bytes(), &state), Err(403));
        }
        assert!(!token_matches(
            &format!("Bearer {}b", "a".repeat(63)),
            &state.token
        ));
        assert!(!token_matches("Bearer a", &state.token));
    }

    #[test]
    fn mutations_bodies_smuggling_and_path_traversal_are_rejected() {
        let state = state();
        for method in [
            "POST", "PUT", "DELETE", "PATCH", "CONNECT", "OPTIONS", "HEAD",
        ] {
            assert_eq!(parse(&request(method, "/input", ""), &state), Err(405));
        }
        for header in [
            "Content-Length: 1\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Host: evil.example\r\n",
            "Upgrade: websocket\r\n",
            "Expect: 100-continue\r\n",
            "Host : 127.0.0.1:1234\r\n",
            " Authorization: x\r\n",
            "X: one\r\nx: two\r\n",
            "X: one\nInjected: value\r\n",
        ] {
            assert!(
                parse(&request("GET", "/", header), &state).is_err(),
                "{header}"
            );
        }
        for path in [
            "/../secret",
            "/%2e%2e/secret",
            "/x?token=secret",
            "//evil.example/x",
            "http://127.0.0.1:1234/",
            "/snapshot?after=-1",
            "/snapshot?after=18446744073709551616",
            "/snapshot?after=1&token=secret",
            "/snapshot?after=",
        ] {
            assert!(parse(&request("GET", path, ""), &state).is_err(), "{path}");
        }
        let mut bytes = request("GET", "/", "");
        bytes.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
        assert!(parse(&bytes, &state).is_err());
        let headers = format!("X: {}\r\n", "a".repeat(9000));
        assert!(parse(&request("GET", "/", &headers), &state).is_err());
    }

    #[tokio::test]
    async fn full_repair_handles_future_revision_and_closed_feed() {
        let (publisher, mut feed) = crate::snapshot_feed(10).unwrap();
        publisher.publish(5, Arc::from(*b"{}")).unwrap();
        assert_eq!(latest(&mut feed, Some(99)).await.unwrap().revision, 5);
        drop(publisher);
        assert!(matches!(latest(&mut feed, None).await, Err(503)));
        assert!(matches!(latest(&mut feed, Some(99)).await, Err(503)));
        assert!(matches!(latest(&mut feed, Some(5)).await, Err(503)));
    }
}
