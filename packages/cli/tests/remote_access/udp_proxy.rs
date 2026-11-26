//! A UDP forwarder between a client and a host's direct address that can drop every
//! datagram on demand, to take a QUIC path down and bring it back without touching either
//! process.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    blackhole: AtomicBool,
    forwarded: AtomicU64,
    forwarded_bytes: AtomicU64,
    dropped: AtomicU64,
}

/// Forwards datagrams arriving on `listen` to `target`, one upstream socket per client
/// source address, and replies back to that source.
pub struct UdpProxy {
    listen: SocketAddr,
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl UdpProxy {
    /// Listens on `listen` (port 0 picks one). Upstream sockets bind the loopback address of
    /// `target`'s family, so the two sides may use different address families.
    #[must_use]
    pub fn start(listen: SocketAddr, target: SocketAddr) -> Self {
        let front = UdpSocket::bind(listen).expect("bind proxy front socket");
        front.set_read_timeout(Some(POLL)).expect("front timeout");
        let listen = front.local_addr().expect("proxy front address");
        let front = Arc::new(front);
        let shared = Arc::new(Shared::default());
        let upstreams: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> = Arc::default();
        let replies: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let accept = {
            let (front, shared, replies) = (front.clone(), shared.clone(), replies.clone());
            std::thread::spawn(move || {
                let mut buffer = vec![0u8; 65_536];
                while !shared.stop.load(Ordering::SeqCst) {
                    let Ok((len, source)) = front.recv_from(&mut buffer) else {
                        continue;
                    };
                    if shared.blackhole.load(Ordering::SeqCst) {
                        shared.dropped.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    let upstream = {
                        let mut upstreams = upstreams.lock().expect("upstream map");
                        upstreams
                            .entry(source)
                            .or_insert_with(|| {
                                let upstream = Arc::new(bind_toward(target));
                                replies.lock().expect("reply threads").push(spawn_reply(
                                    upstream.clone(),
                                    front.clone(),
                                    source,
                                    shared.clone(),
                                ));
                                upstream
                            })
                            .clone()
                    };
                    if upstream.send(&buffer[..len]).is_ok() {
                        shared.forwarded.fetch_add(1, Ordering::SeqCst);
                        shared
                            .forwarded_bytes
                            .fetch_add(len as u64, Ordering::SeqCst);
                    }
                }
                for reply in replies.lock().expect("reply threads").drain(..) {
                    let _ = reply.join();
                }
            })
        };
        Self {
            listen,
            shared,
            threads: vec![accept],
        }
    }

    /// The address clients send to.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.listen
    }

    /// Drops every datagram in both directions while `on`.
    pub fn blackhole(&self, on: bool) {
        self.shared.blackhole.store(on, Ordering::SeqCst);
    }

    /// Datagrams forwarded in either direction so far.
    #[must_use]
    pub fn forwarded(&self) -> u64 {
        self.shared.forwarded.load(Ordering::SeqCst)
    }

    /// Datagram payload bytes forwarded in either direction so far.
    #[must_use]
    pub fn forwarded_bytes(&self) -> u64 {
        self.shared.forwarded_bytes.load(Ordering::SeqCst)
    }

    /// Datagrams dropped while blackholed.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::SeqCst)
    }
}

impl Drop for UdpProxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn bind_toward(target: SocketAddr) -> UdpSocket {
    let local: SocketAddr = if target.is_ipv4() {
        ([127, 0, 0, 1], 0).into()
    } else {
        (std::net::Ipv6Addr::LOCALHOST, 0).into()
    };
    let socket = UdpSocket::bind(local).expect("bind proxy upstream socket");
    socket.connect(target).expect("connect proxy upstream");
    socket
        .set_read_timeout(Some(POLL))
        .expect("upstream timeout");
    socket
}

fn spawn_reply(
    upstream: Arc<UdpSocket>,
    front: Arc<UdpSocket>,
    source: SocketAddr,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; 65_536];
        while !shared.stop.load(Ordering::SeqCst) {
            let Ok(len) = upstream.recv(&mut buffer) else {
                continue;
            };
            if shared.blackhole.load(Ordering::SeqCst) {
                shared.dropped.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            if front.send_to(&buffer[..len], source).is_ok() {
                shared.forwarded.fetch_add(1, Ordering::SeqCst);
                shared
                    .forwarded_bytes
                    .fetch_add(len as u64, Ordering::SeqCst);
            }
        }
    })
}
