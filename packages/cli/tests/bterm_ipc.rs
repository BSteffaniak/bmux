//! Public local IPC startup contention regressions with caller-owned endpoints.
//!
//! These tests never inspect or manage an existing BMUX runtime. They exercise
//! actual listener binding, not a duplicated lock or pathname model.
#![cfg(unix)]

use std::sync::{Arc, Barrier};

use bmux_ipc::IpcEndpoint;
use bmux_ipc::transport::{LocalIpcListener, LocalIpcStream};

#[tokio::test]
async fn competing_startups_have_one_owner_and_losers_do_not_remove_live_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = IpcEndpoint::unix_socket(directory.path().join("runtime.sock"));
    let start = Arc::new(Barrier::new(8));
    let reactor = tokio::runtime::Handle::current();
    let results = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let start = Arc::clone(&start);
                let endpoint = endpoint.clone();
                let reactor = reactor.clone();
                scope.spawn(move || {
                    let _entered = reactor.enter();
                    start.wait();
                    LocalIpcListener::bind(&endpoint)
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().expect("competing bind worker"))
            .collect::<Vec<_>>()
    });
    let mut owners: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
    assert_eq!(owners.len(), 1, "exactly one startup must acquire the name");
    let owner = owners.pop().unwrap();
    let connection = LocalIpcStream::connect(&endpoint).await.unwrap();
    let accepted = owner.accept().await.unwrap();
    assert!(LocalIpcListener::bind(&endpoint).is_err());
    drop((connection, accepted, owner));
}

#[tokio::test]
async fn competing_startups_do_not_claim_or_remove_an_unresponsive_existing_socket() {
    use std::os::unix::fs::MetadataExt as _;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sock");
    let endpoint = IpcEndpoint::unix_socket(&path);
    // Closing a listener leaves a stale socket pathname. A failed connection
    // is not authorization to unlink it or select another runtime root.
    let existing = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let before = std::fs::symlink_metadata(&path).unwrap();
    drop(existing);
    assert!(LocalIpcStream::connect(&endpoint).await.is_err());
    for _ in 0..8 {
        assert!(LocalIpcListener::bind(&endpoint).is_err());
    }
    let after = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
}
