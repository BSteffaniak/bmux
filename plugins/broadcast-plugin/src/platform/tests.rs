use super::*;
use std::io::Write;

#[test]
fn private_files_roundtrip_and_publication_replaces_complete_contents() {
    let root = tempfile::tempdir().unwrap();
    let private = root.path().join("private");
    private_dir(&private, true).unwrap();
    let target = private.join("value");
    let temporary = private.join("next");
    let mut old = create_new(&target).unwrap();
    old.write_all(b"old").unwrap();
    drop(old);
    let mut next = create_new(&temporary).unwrap();
    next.write_all(b"new").unwrap();
    next.sync_all().unwrap();
    drop(next);
    publish(&temporary, &target).unwrap();
    let mut file = open_private(&target).unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
    assert_eq!(bytes, b"new");
    assert!(create_new(&target).is_err());
}

#[test]
fn publication_of_new_asset_never_overwrites_an_existing_identity() {
    let root = tempfile::tempdir().unwrap();
    let private = root.path().join("private");
    private_dir(&private, true).unwrap();
    let target = private.join("asset");
    let first = private.join("first");
    let second = private.join("second");
    for (path, bytes) in [(&first, b"first"), (&second, b"other")] {
        let mut file = create_new(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    publish_new(&first, &target).unwrap();
    let error = publish_new(&second, &target).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&target).unwrap(), b"first");
    assert_eq!(std::fs::read(&second).unwrap(), b"other");
    sync_directory(&private).unwrap();
}

#[test]
fn profile_lock_excludes_second_writer_and_releases_on_drop() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("lock");
    let first = lock(&path).unwrap();
    assert!(lock(&path).is_err());
    drop(first);
    assert!(lock(&path).is_ok());
}
