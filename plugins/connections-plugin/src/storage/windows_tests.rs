//! Native tests need no symlink privilege; hardlinks and ACLs are checked by handle.
use super::*;
use crate::storage::PairingStore;
use std::sync::{Arc, Barrier};

fn store(root: &Path) -> PairingStore {
    PairingStore::client(root, "windows", c"pairing.json").unwrap()
}

#[test]
fn publication_is_immutable_and_atomic_between_racers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private");
    let directory = Arc::new(PrivateDirectory::open(&path).unwrap());
    let barrier = Barrier::new(8);
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let directory = &directory;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    directory.publish(&[index; 1024]).unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    for winner in &results {
        assert_eq!(winner, &results[0]);
    }
    assert_eq!(directory.read().unwrap(), Some(results[0].clone()));
    assert_eq!(std::fs::read_dir(path).unwrap().count(), 1);
}

#[test]
fn transaction_subprocess() {
    let Some(root) = std::env::var_os("BMUX_WINDOWS_PAIRING_TEST_ROOT") else {
        return;
    };
    let store = store(Path::new(&root));
    for _ in 0..12 {
        increment(&store);
    }
}
fn increment(store: &PairingStore) {
    store
        .transaction(|bytes| {
            let value = bytes
                .as_ref()
                .map_or(0, |bytes| serde_json::from_slice::<u64>(bytes).unwrap());
            *bytes = Some(serde_json::to_vec(&(value + 1))?);
            Ok(())
        })
        .unwrap();
}
#[test]
fn transactions_serialize_processes_and_threads_and_remove_records() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mut children: Vec<_> = (0..3)
        .map(|_| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "storage::windows::tests::transaction_subprocess"])
                .env("BMUX_WINDOWS_PAIRING_TEST_ROOT", root.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..3 {
            let store = &store;
            scope.spawn(move || {
                for _ in 0..12 {
                    increment(store);
                }
            });
        }
    });
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    store
        .transaction(|bytes| {
            assert_eq!(serde_json::from_slice::<u64>(bytes.as_ref().unwrap())?, 72);
            *bytes = None;
            Ok(())
        })
        .unwrap();
    store
        .transaction(|bytes| {
            assert!(bytes.is_none());
            Ok(())
        })
        .unwrap();
}

#[test]
fn private_directory_rejects_inherited_acl_and_pins_its_entry() {
    let root = tempfile::tempdir().unwrap();
    let directory = PrivateDirectory::open(&root.path().join("private")).unwrap();
    private(&directory.handle, true).unwrap();
    // Unlike Unix, Windows pins ancestry by withholding FILE_SHARE_DELETE.
    assert!(std::fs::rename(root.path().join("private"), root.path().join("moved")).is_err());
    let inherited = root.path().join("inherited");
    std::fs::create_dir(&inherited).unwrap();
    assert!(PrivateDirectory::open(&inherited).is_err());
}
#[test]
fn hardlinked_and_directory_records_are_rejected_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private");
    let directory = PrivateDirectory::open(&path).unwrap();
    directory.publish(b"immutable identity").unwrap();
    let alias = root.path().join("outside");
    std::fs::hard_link(path.join("identity.json"), &alias).unwrap();
    assert!(directory.read().is_err());
    assert_eq!(std::fs::read(&alias).unwrap(), b"immutable identity");
    std::fs::remove_file(alias).unwrap();
    assert_eq!(directory.read().unwrap().unwrap(), b"immutable identity");
    std::fs::create_dir(path.join("pairing.json")).unwrap();
    assert!(directory.read_record(c"pairing.json", 1024).is_err());
}
#[test]
fn relative_names_reject_ads_and_escape_components() {
    for name in ["../outside", "..", ".", "foo:secret", "a\\b", "a/b", ""] {
        assert!(component(std::ffi::OsStr::new(name)).is_err());
    }
}
