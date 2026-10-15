use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::assets::LoadedAsset;
use crate::{Limits, Snapshot, SnapshotFeed};

pub(crate) struct State {
    pub(crate) diagnostics: Arc<crate::diagnostics::Diagnostics>,
    pub assets: BTreeMap<String, LoadedAsset>,
    pub limits: Limits,
    pub auth_mode: crate::AuthMode,
    pub token: String,
    pub allowed_hosts: Vec<String>,
    pub viewport: Arc<crate::viewport::Viewport>,
}

pub(crate) fn valid_authority(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let Some((ip, suffix)) = rest.split_once(']') else {
            return false;
        };
        return ip.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port));
    }
    let (host, port) = value
        .split_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    !host.is_empty()
        && port.is_none_or(valid_port)
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn valid_port(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u16>().is_ok_and(|port| port != 0)
}

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Diagnostics,
    Asset(&'a str),
    Snapshot(Option<u64>),
    Document(Option<u64>),
    Presentation {
        after: Option<u64>,
        base: Option<u64>,
    },
    Resource(&'a str),
    View {
        controller: Option<&'a str>,
        release: bool,
    },
    Viewport {
        controller: Option<&'a str>,
        length: usize,
    },
}

fn route(target: &str) -> Result<Route<'_>, u16> {
    if target == "/diagnostics" {
        return Ok(Route::Diagnostics);
    }
    if let Some(id) = target.strip_prefix("/resource/") {
        return if crate::publication::valid_id(id) {
            Ok(Route::Resource(id))
        } else {
            Err(400)
        };
    }
    if target == "/document" || target.starts_with("/document?") {
        let query = target.strip_prefix("/document?").ok_or(400_u16)?;
        let mut version = None;
        let mut after = None;
        for part in query.split('&') {
            let (key, value) = part.split_once('=').ok_or(400_u16)?;
            let slot = match key {
                "version" => &mut version,
                "after" => &mut after,
                _ => return Err(400),
            };
            if slot.is_some()
                || value.is_empty()
                || value.len() > 20
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(400);
            }
            *slot = Some(value.parse::<u64>().map_err(|_| 400_u16)?);
        }
        if version != Some(1) {
            return Err(400);
        }
        return Ok(Route::Document(after));
    }
    if target == "/presentation" || target.starts_with("/presentation?") {
        let mut version = None;
        let mut after = None;
        let mut base = None;
        for part in target
            .strip_prefix("/presentation?")
            .ok_or(400_u16)?
            .split('&')
        {
            let (key, value) = part.split_once('=').ok_or(400_u16)?;
            let slot = match key {
                "version" => &mut version,
                "after" => &mut after,
                "base" => &mut base,
                _ => return Err(400),
            };
            if slot.is_some()
                || value.is_empty()
                || value.len() > 20
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(400);
            }
            *slot = Some(value.parse::<u64>().map_err(|_| 400_u16)?);
        }
        if version != Some(1) {
            return Err(400);
        }
        return Ok(Route::Presentation { after, base });
    }
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
    // Queries select browser capabilities, never a filesystem path or credential.
    // Validate the undecoded path exactly as before; only the allowlisted key is used.
    if let Some((_, query)) = target.split_once('?')
        && query.split('&').any(|part| {
            let key = part.split_once('=').map_or(part, |(key, _)| key);
            key.eq_ignore_ascii_case("token") || key.contains('%')
        })
    {
        // Preserve the pre-query-support response for URL credential attempts.
        return Err(404);
    }
    let asset_path = target.split_once('?').map_or(target, |(path, _)| path);
    if asset_path == "/"
        || asset_path.strip_prefix('/').is_some_and(|name| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
    {
        return Ok(Route::Asset(asset_path));
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
    if method != "GET"
        && !matches!(
            (method, target),
            ("POST" | "DELETE", "/view") | ("POST", "/viewport")
        )
    {
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
    let authority = headers.get("host").copied().ok_or(400_u16)?;
    if !valid_authority(authority) {
        return Err(400);
    }
    if !state.allowed_hosts.is_empty() && !state.allowed_hosts.iter().any(|host| host == authority)
    {
        return Err(403);
    }
    let origin = format!("http://{authority}");
    if headers.get("origin").is_some_and(|value| *value != origin) {
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
    {
        return Err(400);
    }
    let mutation = method != "GET";
    let length = match headers.get("content-length") {
        Some(value)
            if !value.is_empty()
                && value.len() <= 4
                && value.bytes().all(|b| b.is_ascii_digit()) =>
        {
            value.parse::<usize>().map_err(|_| 400_u16)?
        }
        Some(_) => return Err(400),
        None => 0,
    };
    if length > 512 {
        return Err(413);
    }
    if mutation && headers.get("origin").copied() != Some(origin.as_str()) {
        return Err(403);
    }
    let route = if mutation {
        let controller = headers.get("x-bmux-controller").copied();
        if controller
            .is_some_and(|value| value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(400);
        }
        if target == "/viewport" {
            if length == 0 || headers.get("content-type").copied() != Some("application/json") {
                return Err(400);
            }
            Route::Viewport { controller, length }
        } else {
            if length != 0 {
                return Err(400);
            }
            Route::View {
                controller,
                release: method == "DELETE",
            }
        }
    } else {
        if length != 0 {
            return Err(400);
        }
        route(target)?
    };
    if state.auth_mode == crate::AuthMode::Token
        && !matches!(route, Route::Asset(_))
        && !headers
            .get("authorization")
            .is_some_and(|value| token_matches(value, &state.token))
    {
        return Err(401);
    }
    Ok(route)
}

async fn read_request(
    stream: &mut TcpStream,
    state: &State,
    request_started: &mut bool,
) -> Result<Vec<u8>, u16> {
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
        *request_started = true;
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
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
    encoded_response(stream, status, content_type, body, revision, None).await
}

async fn encoded_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    revision: Option<u64>,
    gzip: Option<bool>,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    };
    let revision_header = revision.map_or_else(String::new, |revision| {
        format!("X-Snapshot-Revision: {revision}\r\n")
    });
    let encoding_header = match gzip {
        Some(true) => "Vary: Accept-Encoding\r\nContent-Encoding: gzip\r\n",
        Some(false) => "Vary: Accept-Encoding\r\n",
        None => "",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n{encoding_header}{revision_header}Connection: close\r\nCache-Control: no-store\r\nPragma: no-cache\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; style-src 'self'; img-src 'self' blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

pub(crate) async fn serve(mut stream: TcpStream, state: Arc<State>, mut feed: SnapshotFeed) {
    let mut request_started = false;
    let request = timeout(
        state.limits.header_timeout,
        read_request(&mut stream, &state, &mut request_started),
    )
    .await;
    // Browsers may preconnect without sending HTTP bytes. Expire those sockets
    // within the same budget, but do not invent a rejected HTTP request or write
    // an unsolicited 408 response. Partial headers still receive a real 408.
    if !request_started {
        state.diagnostics.event(
            if request.is_err() {
                "idle_connection_timeout"
            } else {
                "idle_connection_closed"
            },
            None,
        );
        return;
    }
    let header_end = request
        .as_ref()
        .ok()
        .and_then(|result| result.as_ref().ok())
        .and_then(|bytes| bytes.windows(4).position(|window| window == b"\r\n\r\n"))
        .map_or(0, |end| end + 4);
    let route = match &request {
        Ok(Ok(bytes)) => parse(&bytes[..header_end], &state),
        Ok(Err(status)) => Err(*status),
        Err(_) => Err(408),
    };
    let accept_encoding = crate::delivery::AcceptEncoding::from_headers(
        request
            .as_ref()
            .ok()
            .and_then(|result| result.as_ref().ok())
            .map_or(&[][..], |bytes| &bytes[..header_end]),
    );
    let body_prefix = request
        .as_ref()
        .ok()
        .and_then(|result| result.as_ref().ok())
        .map_or(&[][..], |bytes| &bytes[header_end..]);
    // Preserve authoritative policy errors even when a rejected request has a body.
    if route.is_ok() && !matches!(route, Ok(Route::Viewport { .. })) && !body_prefix.is_empty() {
        error(&mut stream, &state, 400).await;
        return;
    }
    match route {
        Ok(Route::Diagnostics) => {
            if let Ok(body) = serde_json::to_vec(&state.diagnostics.snapshot()) {
                let _ = timeout(
                    state.limits.write_timeout,
                    response(&mut stream, 200, "application/json", &body, None),
                )
                .await;
            }
        }
        Ok(Route::View {
            controller,
            release,
        }) => {
            // A closed producer must never grant a new controller over stale output.
            if feed.receiver.has_changed().is_err() {
                error(&mut stream, &state, 503).await;
                return;
            }
            let result = if release {
                state.viewport.release(controller).map(|()| Vec::new())
            } else {
                state.viewport.claim(controller)
            };
            match result {
                Ok(body) => {
                    let _ = timeout(
                        state.limits.write_timeout,
                        response(
                            &mut stream,
                            if release { 204 } else { 200 },
                            "application/json",
                            &body,
                            None,
                        ),
                    )
                    .await;
                }
                Err(status) => error(&mut stream, &state, status).await,
            }
        }
        Ok(Route::Viewport { controller, length }) => {
            if feed.receiver.has_changed().is_err() {
                error(&mut stream, &state, 503).await;
                return;
            }
            if body_prefix.len() > length {
                error(&mut stream, &state, 400).await;
                return;
            }
            let mut body = vec![0; length];
            body[..body_prefix.len()].copy_from_slice(body_prefix);
            let read = timeout(
                state.limits.header_timeout,
                stream.read_exact(&mut body[body_prefix.len()..]),
            )
            .await;
            let result = match read {
                Ok(Ok(_)) => state.viewport.submit(controller, &body),
                Ok(Err(_)) => Err(400),
                Err(_) => Err(408),
            };
            error(
                &mut stream,
                &state,
                result.map_or_else(|status| status, |()| 204),
            )
            .await;
        }
        Ok(Route::Asset(path)) => {
            if let Some(asset) = state.assets.get(path) {
                observed_response(
                    &mut stream,
                    &state,
                    200,
                    asset.content_type.header(),
                    &asset.bytes,
                    None,
                )
                .await;
            } else {
                error(&mut stream, &state, 404).await;
            }
        }
        Ok(Route::Resource(id)) => {
            if feed.receiver.has_changed().is_err() {
                error(&mut stream, &state, 503).await;
                return;
            }
            let resource = feed
                .receiver
                .borrow()
                .as_ref()
                .and_then(|value| {
                    value
                        .resources
                        .get(id)
                        .or_else(|| value.previous_resources.get(id))
                })
                .cloned();
            if let Some(resource) = resource {
                // Resources (especially PNG) are always identity, never recompressed.
                if accept_encoding.select(&resource.bytes, None).is_none() {
                    error(&mut stream, &state, 406).await;
                    return;
                }
                observed_response(
                    &mut stream,
                    &state,
                    200,
                    &resource.mime_type,
                    &resource.bytes,
                    None,
                )
                .await;
            } else {
                error(&mut stream, &state, 404).await;
            }
        }
        Ok(Route::Document(after)) => {
            match timeout(state.limits.poll_timeout, latest(&mut feed, after)).await {
                Ok(Ok(snapshot)) => {
                    if let Some(document) = &snapshot.document {
                        publication_response(
                            &mut stream,
                            &state,
                            "application/vnd.bmux.document-v1+json",
                            accept_encoding
                                .select(&document.bytes, snapshot.delivery.document.as_ref()),
                            snapshot.revision,
                            "document",
                        )
                        .await;
                    } else {
                        error(&mut stream, &state, 406).await;
                    }
                }
                Ok(Err(status)) => error(&mut stream, &state, status).await,
                Err(_) => error(&mut stream, &state, 204).await,
            }
        }
        Ok(Route::Presentation { after, base }) => {
            match timeout(
                state.limits.poll_timeout,
                latest(&mut feed, if after == base { after } else { None }),
            )
            .await
            {
                Ok(Ok(snapshot)) => {
                    if let Some(binary) = &snapshot.binary {
                        let delta = binary
                            .delta
                            .as_ref()
                            .filter(|(revision, _)| Some(*revision) == base);
                        let (bytes, gzip, representation) = delta.map_or(
                            (&binary.snapshot, snapshot.delivery.full.as_ref(), "full"),
                            |(_, bytes)| (bytes, snapshot.delivery.delta.as_ref(), "delta"),
                        );
                        publication_response(
                            &mut stream,
                            &state,
                            "application/vnd.bmux.presentation-v1",
                            accept_encoding.select(bytes, gzip),
                            snapshot.revision,
                            representation,
                        )
                        .await;
                    } else {
                        error(&mut stream, &state, 503).await;
                    }
                }
                Ok(Err(status)) => error(&mut stream, &state, status).await,
                Err(_) => error(&mut stream, &state, 204).await,
            }
        }
        Ok(Route::Snapshot(after)) => {
            if feed.receiver.has_changed().is_ok()
                && feed
                    .receiver
                    .borrow()
                    .as_ref()
                    .is_some_and(|snapshot| !snapshot.json_available)
            {
                error(&mut stream, &state, 406).await;
                return;
            }
            match timeout(state.limits.poll_timeout, latest(&mut feed, after)).await {
                Ok(Ok(snapshot)) => {
                    if !snapshot.json_available {
                        error(&mut stream, &state, 406).await;
                        return;
                    }
                    publication_response(
                        &mut stream,
                        &state,
                        "application/json",
                        accept_encoding.select(&snapshot.bytes, snapshot.delivery.json.as_ref()),
                        snapshot.revision,
                        "json",
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

async fn publication_response(
    stream: &mut TcpStream,
    state: &State,
    content_type: &str,
    selected: Option<crate::delivery::Selected<'_>>,
    revision: u64,
    representation: &'static str,
) {
    let Some(selected) = selected else {
        error(stream, state, 406).await;
        return;
    };
    match timeout(
        state.limits.write_timeout,
        encoded_response(
            stream,
            200,
            content_type,
            selected.bytes,
            Some(revision),
            Some(selected.gzip),
        ),
    )
    .await
    {
        Ok(Ok(())) => state.diagnostics.delivered(&selected, representation),
        Ok(Err(error)) => state.diagnostics.event(
            "response_write_failed",
            error
                .raw_os_error()
                .and_then(|code| u64::try_from(code).ok()),
        ),
        Err(_) => state
            .diagnostics
            .event("response_write_timeout", Some(revision)),
    }
}

async fn observed_response(
    stream: &mut TcpStream,
    state: &State,
    status: u16,
    content_type: &str,
    body: &[u8],
    revision: Option<u64>,
) {
    match timeout(
        state.limits.write_timeout,
        response(stream, status, content_type, body, revision),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => state.diagnostics.event(
            "response_write_failed",
            error
                .raw_os_error()
                .and_then(|code| u64::try_from(code).ok()),
        ),
        Err(_) => state.diagnostics.event("response_write_timeout", revision),
    }
}

async fn error(stream: &mut TcpStream, state: &State, status: u16) {
    if status >= 400 {
        state
            .diagnostics
            .event("request_rejected", Some(u64::from(status)));
    }
    observed_response(
        stream,
        state,
        status,
        "text/plain; charset=utf-8",
        &[],
        None,
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
            auth_mode: crate::AuthMode::Token,
            token: "a".repeat(64),
            allowed_hosts: vec!["127.0.0.1:1234".into()],
            diagnostics: Arc::new(crate::diagnostics::Diagnostics::new()),
            viewport: Arc::new(crate::viewport::Viewport::new(crate::ViewportMode::Observer).0),
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

    #[test]
    fn control_routes_require_origin_auth_and_bounded_exact_body() {
        let state = state();
        let auth = format!("Authorization: Bearer {}\r\n", state.token);
        let origin = "Origin: http://127.0.0.1:1234\r\n";
        for (method, path) in [
            ("POST", "/view"),
            ("DELETE", "/view"),
            ("POST", "/viewport"),
        ] {
            assert_eq!(parse(&request(method, path, &auth), &state), Err(403));
            let headers = format!(
                "{origin}Content-Type: application/json\r\nContent-Length: {}\r\n",
                if path == "/viewport" { 10 } else { 0 }
            );
            assert_eq!(parse(&request(method, path, &headers), &state), Err(401));
            let headers = format!("{auth}{headers}");
            assert!(parse(&request(method, path, &headers), &state).is_ok());
            for extra in [
                "Origin: null\r\n",
                "Host: evil.example\r\n",
                "Transfer-Encoding: chunked\r\n",
                "Content-Length: 513\r\n",
                "X-Bmux-Controller: short\r\n",
            ] {
                assert!(
                    parse(&request(method, path, &format!("{headers}{extra}")), &state).is_err()
                );
            }
        }
        assert_eq!(
            parse(
                &request(
                    "POST",
                    "/viewport",
                    &format!(
                        "{auth}{origin}Content-Length: 513\r\nContent-Type: application/json\r\n"
                    )
                ),
                &state
            ),
            Err(413)
        );
        assert_eq!(
            parse(
                &request(
                    "POST",
                    "/viewport",
                    &format!("{auth}{origin}Content-Length: 10\r\n")
                ),
                &state
            ),
            Err(400)
        );
    }

    #[tokio::test]
    async fn cached_compression_does_not_wake_an_unchanged_poll() {
        let (publisher, mut feed) = crate::snapshot_feed(8192).unwrap();
        publisher.publish(1, vec![b'x'; 4096].into()).unwrap();
        let cached = feed
            .receiver
            .borrow()
            .as_ref()
            .unwrap()
            .delivery
            .json
            .clone()
            .unwrap();
        assert!(
            timeout(
                std::time::Duration::from_millis(10),
                latest(&mut feed, Some(1))
            )
            .await
            .is_err()
        );
        assert!(Arc::ptr_eq(
            &cached,
            feed.receiver
                .borrow()
                .as_ref()
                .unwrap()
                .delivery
                .json
                .as_ref()
                .unwrap()
        ));
        publisher.publish(2, vec![b'y'; 4096].into()).unwrap();
        assert_eq!(latest(&mut feed, Some(1)).await.unwrap().revision, 2);
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
