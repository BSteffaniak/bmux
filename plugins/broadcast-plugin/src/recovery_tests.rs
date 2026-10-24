use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) struct TestRelay {
    pub events: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    _credential: tempfile::NamedTempFile,
    pub relay: Option<Relay>,
    pub fail_pause: Arc<AtomicBool>,
}
impl TestRelay {
    pub fn new(repair_once: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (send, events) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let fail_pause = Arc::new(AtomicBool::new(false));
        let reject_pause = fail_pause.clone();
        let worker = std::thread::spawn(move || {
            let epoch = Uuid::new_v4();
            let mut fence = 1_u64;
            let mut sockets = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                // Accepted sockets inherit nonblocking mode on some Unix platforms.
                // The fixture's HTTP parser and WebSocket handler use blocking IO.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut probe = [0; 16384];
                loop {
                    let count = stream.peek(&mut probe).unwrap();
                    if probe[..count].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                    assert!(count < probe.len());
                    std::thread::sleep(Duration::from_millis(1));
                }
                if probe.starts_with(b"GET /v1/publisher/ws ") {
                    let send = send.clone();
                    let stop = stop.clone();
                    sockets.push(std::thread::spawn(move || {
                        use tokio_tungstenite::tungstenite::{accept, Message};
                        let mut socket = accept(stream).unwrap();
                        let mut base = None;
                        let mut repair = repair_once;
                        while !stop.load(Ordering::SeqCst) {
                            let message = match socket.read() { Ok(value) => value, Err(_) => break };
                            let Message::Binary(bytes) = message else { continue; };
                            send.send(("frame".into(), bytes.to_vec())).unwrap();
                            let frame = bmux_web_renderer::decode_binary(&bytes, base.as_ref()).unwrap();
                            if repair {
                                repair = false;
                                socket.send(Message::Text("{\"type\":\"repair\"}".into())).unwrap();
                                base = None;
                                continue;
                            }
                            let ack = serde_json::json!({"type":"ack", "generation":frame.generation(), "revision":frame.revision().to_string()});
                            socket.send(Message::Text(ack.to_string().into())).unwrap();
                            base = Some(frame);
                        }
                    }));
                    continue;
                }
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                let route = header
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|value| value.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                if route == "pause" && reject_pause.load(Ordering::SeqCst) {
                    write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    send.send((route.into(), Vec::new())).unwrap();
                    continue;
                }
                if matches!(route, "pause" | "resume" | "stop") {
                    fence += 1;
                }
                let phase = match route {
                    "pause" => "paused",
                    "stop" => "offline",
                    _ => "starting",
                };
                let body = serde_json::json!({"type":"state", "epoch":if route == "stop" { None } else { Some(epoch) }, "fence":fence.to_string(), "state":phase, "lease_ms":15000});
                let body = body.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                send.send((route.into(), Vec::new())).unwrap();
            }
            for socket in sockets {
                socket.join().unwrap();
            }
        });
        let credential = tempfile::NamedTempFile::new().unwrap();
        std::fs::set_permissions(credential.path(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::write(credential.path(), "x".repeat(32)).unwrap();
        let relay = Relay::new(&endpoint, "main", credential.path()).unwrap();
        Self {
            events,
            stopped,
            worker: Some(worker),
            _credential: credential,
            relay: Some(relay),
            fail_pause,
        }
    }
    pub fn event(&self, expected: &str) -> Vec<u8> {
        loop {
            let (event, bytes) = self.events.recv_timeout(Duration::from_secs(5)).unwrap();
            if event == expected {
                return bytes;
            }
            assert_eq!(event, "heartbeat", "unexpected relay event");
        }
    }
}
impl Drop for TestRelay {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
#[test]
fn websocket_repair_and_acknowledged_delta_baseline() {
    let mut server = TestRelay::new(true);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut relay = server.relay.take().unwrap();
    runtime.block_on(async {
        relay.start().await.unwrap();
        server.event("start");
        let first = super::lifecycle_tests::frame("scene", 1);
        relay.mutation("frame", Some((1, &first))).await.unwrap();
        assert_eq!(server.event("frame"), first);
        assert_eq!(server.event("frame"), first);
        let second = super::lifecycle_tests::frame("scene", 2);
        relay.mutation("frame", Some((2, &second))).await.unwrap();
        let delta = server.event("frame");
        assert!(delta.len() < second.len());
        assert!(bmux_web_renderer::decode_binary(&delta, None).is_err());
        let base = bmux_web_renderer::decode_binary(&first, None).unwrap();
        assert_eq!(
            bmux_web_renderer::decode_binary(&delta, Some(&base)).unwrap(),
            bmux_web_renderer::decode_binary(&second, None).unwrap()
        );
        relay.mutation("pause", None).await.unwrap();
        server.event("pause");
        relay.mutation("resume", None).await.unwrap();
        server.event("resume");
        relay.mutation("frame", Some((3, &second))).await.unwrap();
        assert_eq!(server.event("frame"), second);
        server.event("frame"); // Explicit repair on each newly admitted socket.
        relay.mutation("stop", None).await.unwrap();
        server.event("stop");
    });
}
