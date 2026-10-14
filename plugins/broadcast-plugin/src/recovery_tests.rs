use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
};

#[test]
fn expired_epoch_reacquires_only_running_and_requires_fresh_repair() {
    use std::os::unix::fs::PermissionsExt;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (events, received) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut starts = 0;
        let mut expired = false;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 16384);
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
                .unwrap()
                .to_owned();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|v| v.parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let reject = route == "heartbeat" && !expired;
            let response = if route == "start" {
                starts += 1;
                let epoch = Uuid::new_v4();
                let body = format!(
                    "{{\"protocol\":1,\"renderer\":\"bmwp-2\",\"epoch\":\"{epoch}\",\"lease_ms\":15000}}"
                );
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else if reject {
                expired = true;
                "HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            } else {
                "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into()
            };
            stream.write_all(response.as_bytes()).unwrap();
            events.send((route.clone(), starts)).unwrap();
            if route == "stop" {
                break;
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("key");
    std::fs::write(&key, "k".repeat(64)).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    controller
        .control(Control::Start(
            Relay::new(&format!("http://{address}"), "main", &key).unwrap(),
            id.clone(),
        ))
        .unwrap();
    let initial = controller.source(owner, &id, false).unwrap().refresh;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        controller.source(owner, &id, false).unwrap();
        if let Ok((route, starts)) = received.recv_timeout(Duration::from_millis(200)) {
            assert_ne!(
                route, "frame",
                "reacquisition must never replay retained bytes"
            );
            if route == "start" && starts == 2 {
                break;
            }
        }
        assert!(Instant::now() < deadline);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while controller.status().unwrap().local == "recovering" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let fresh = controller.source(owner, &id, false).unwrap();
    assert!(fresh.refresh > initial);
    // A pause closes recovery intent; no further start is permitted.
    controller.control(Control::Pause).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(controller.status().unwrap().local, "paused");
    while let Ok((route, _)) = received.try_recv() {
        assert_ne!(route, "start");
    }
    controller.shutdown();
    server.join().unwrap();
}
