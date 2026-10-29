use super::*;
use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

#[test]
fn private_cache_files_lock_and_publish_without_overwrite() {
    let temporary = tempfile::tempdir().unwrap();
    let cache = temporary.path().join("cache");
    private_directory(&cache).unwrap();
    let directory = open_directory(&cache).unwrap();
    Security::current_user()
        .unwrap()
        .check_file(directory.last().as_raw_handle())
        .unwrap();
    drop(directory);
    let lock_path = cache.join("setup.lock");
    let lock = Lock::acquire(&lock_path).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _lock = Lock::acquire(&lock_path).unwrap();
        send.send(()).unwrap();
    });
    assert!(receive.recv_timeout(Duration::from_millis(50)).is_err());
    drop(lock);
    receive.recv_timeout(Duration::from_secs(3)).unwrap();
    waiter.join().unwrap();
    let stage = cache.join("stage");
    private_directory(&stage).unwrap();
    let file = stage.join("index.html");
    new_file(&file).unwrap().write_all(b"safe").unwrap();
    sync_regular(&file).unwrap();
    let destination = cache.join("bundle");
    publish(&stage, &destination).unwrap();
    assert_eq!(fs::read(destination.join("index.html")).unwrap(), b"safe");
    private_directory(&stage).unwrap();
    assert!(publish(&stage, &destination).is_err());
}

#[test]
fn assets_are_handle_relative_and_hold_ancestors_against_rename() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("assets");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("index.html"), b"hello").unwrap();
    let directory = open_directory(&path).unwrap();
    assert!(fs::rename(&path, temporary.path().join("replacement")).is_err());
    let mut bytes = Vec::new();
    open_asset(&directory, "index.html")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"hello");
    assert!(open_asset(&directory, "../index.html").is_err());
    assert!(open_asset(&directory, "index.html:stream").is_err());
    assert!(open_asset(&directory, ".").is_err());
    assert!(open_directory(Path::new(r"\\server\share\assets")).is_err());
    assert!(open_directory(Path::new(r"\\.\pipe\assets")).is_err());
}

#[test]
fn junction_ancestor_is_rejected_without_symlink_privileges() {
    let temporary = tempfile::tempdir().unwrap();
    let target = temporary.path().join("target");
    let alias = temporary.path().join("alias");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("index.html"), b"outside").unwrap();
    let status = Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(open_directory(&alias).is_err());
    assert!(open_regular(&alias.join("index.html")).is_err());
    assert!(private_directory(&alias).is_err());
    fs::remove_dir(alias).unwrap();
}

#[test]
fn source_setup_rejects_ambient_toolchain_but_asset_access_remains_available() {
    let error = source_command(Path::new(r"C:\source"), &["cargo", "build"])
        .err()
        .expect("native source driver is not implemented");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("verified release bundle"));
}
