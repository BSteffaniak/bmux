use super::*;
use std::os::unix::fs::{PermissionsExt as _, symlink};

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("worker");
    let bytes =
        b"#!/bin/sh\nif [ \"$1\" = child ]; then echo retained; else exec \"$0\" child; fi\n";
    std::fs::write(&source, bytes).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cache = root.path().join("images");
    let hash = hex(&Sha256::digest(bytes));
    (root, source, cache, hash)
}

#[test]
fn retained_image_launches_and_reexecutes_after_source_replacement_and_removal() {
    let (_root, source, cache, hash) = fixture();
    let image = retain(&source, &cache, "owner", &hash).unwrap();
    assert_ne!(image, source);
    std::fs::write(&source, b"replacement").unwrap();
    std::fs::remove_file(&source).unwrap();
    let output = std::process::Command::new(&image).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"retained\n");
    assert_eq!(retain(&source, &cache, "owner", &hash).unwrap(), image);
}

#[test]
fn corrupt_images_and_changed_sources_are_rejected_without_replacement() {
    let (_root, source, cache, hash) = fixture();
    let image = retain(&source, &cache, "owner", &hash).unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&image, b"corrupt").unwrap();
    let error = retain(&source, &cache, "owner", &hash).unwrap_err();
    assert!(error.contains("checksum mismatch"), "{error}");
    assert!(error.contains(image.to_str().unwrap()), "{error}");
    assert_eq!(std::fs::read(image).unwrap(), b"corrupt");
    std::fs::write(&source, b"changed source").unwrap();
    assert!(retain(&source, &cache, "other", &hash).is_err());
}

#[test]
fn concurrent_publication_reuses_verified_image() {
    let (_root, source, cache, hash) = fixture();
    std::thread::scope(|scope| {
        // Spawn every publisher before joining; a lazy iterator would serialize them.
        let threads: [_; 8] = std::array::from_fn(|_| {
            scope.spawn(|| retain(&source, &cache, "owner", &hash).unwrap())
        });
        let images: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(images.iter().all(|image| image == &images[0]));
        verify(&images[0], &hash).unwrap();
    });
}

#[test]
fn owner_and_artifact_identity_are_independent() {
    let (_root, source, cache, hash) = fixture();
    let first = retain(&source, &cache, "first", &hash).unwrap();
    let other = retain(&source, &cache, "second", &hash).unwrap();
    assert_ne!(first, other);
    std::fs::write(&source, b"new bytes").unwrap();
    let next_hash = hex(&Sha256::digest(b"new bytes"));
    let next = retain(&source, &cache, "first", &next_hash).unwrap();
    assert_ne!(first, next);
    verify(&first, &hash).unwrap();
}

#[test]
fn unsafe_directories_and_symlink_images_are_rejected() {
    let (root, source, cache, hash) = fixture();
    std::fs::create_dir(&cache).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(retain(&source, &cache, "owner", &hash).is_err());
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700)).unwrap();
    let image = retain(&source, &cache, "owner", &hash).unwrap();
    std::fs::remove_file(&image).unwrap();
    symlink(&source, &image).unwrap();
    assert!(retain(&source, &cache, "owner", &hash).is_err());
    let linked_cache = root.path().join("linked");
    symlink(&cache, &linked_cache).unwrap();
    assert!(retain(&source, &linked_cache, "owner", &hash).is_err());
    assert!(retain(&source, Path::new("relative"), "owner", &hash).is_err());
}
