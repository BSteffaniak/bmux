use bmux_config_composition::*;
use std::collections::BTreeMap;

#[test]
fn infallible_legacy_merge_matches_document_default_including_empty_tables() {
    for (base, incoming) in [
        ("[a]\nx=1", "[a]"),
        ("a=[1,2]", "a=[3]"),
        ("a=1", "[a]\nx=2"),
        ("[a]\nx=1", "a=2"),
        ("[a]\nx=1\ny=2", "[a]\nx=3"),
    ] {
        let base = TomlCodec::parse(base).unwrap();
        let incoming = TomlCodec::parse(incoming).unwrap();
        let mut legacy = base.clone();
        merge_recursive_replace(&mut legacy, incoming.clone());
        let mut doc = Document::new(base, SourceId(0));
        doc.merge(
            Document::new(incoming, SourceId(1)),
            &MergePolicy::default(),
        )
        .unwrap();
        assert_eq!(legacy, doc.value);
    }
}
#[test]
fn single_file_capture_is_bounded_explicit_and_owned() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file");
    assert_eq!(
        capture_file(&path, 1024).unwrap_err().kind,
        ErrorKind::Missing
    );
    assert_eq!(
        capture_file(std::path::Path::new("relative"), 1024)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidContext
    );
    std::fs::write(&path, "secret=1").unwrap();
    assert_eq!(capture_file(&path, 7).unwrap_err().kind, ErrorKind::Limit);
    let captured = capture_file(&path, 8).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(captured, b"secret=1");
    assert_eq!(
        capture_file(root.path(), usize::MAX).unwrap_err().kind,
        ErrorKind::NotRegular
    );
}
#[test]
fn profile_depth_is_only_caller_limited_not_rust_stack_limited() {
    let mut profiles = BTreeMap::new();
    for index in 0..2048 {
        profiles.insert(
            index.to_string(),
            Profile {
                parents: if index == 0 {
                    vec![]
                } else {
                    vec![(index - 1).to_string()]
                },
                value: 1usize,
            },
        );
    }
    let merge = |old: &mut usize, new| {
        *old += new;
        Ok(())
    };
    assert_eq!(
        resolve_profile(
            "2047",
            &profiles,
            ProfileLimits {
                max_depth: 2048,
                max_expansions: 2048
            },
            merge
        )
        .unwrap(),
        2048
    );
    assert_eq!(
        resolve_profile(
            "2047",
            &profiles,
            ProfileLimits {
                max_depth: 2047,
                max_expansions: 2048
            },
            merge
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit
    );
    assert_eq!(
        resolve_profile(
            "2047",
            &profiles,
            ProfileLimits {
                max_depth: 2048,
                max_expansions: 2047
            },
            merge
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit
    );
}
#[cfg(unix)]
#[test]
fn standalone_capture_preserves_symlink_parent_semantics_and_rejects_fifo() {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("real/child")).unwrap();
    symlink("real/child", root.path().join("link")).unwrap();
    std::fs::write(root.path().join("real/config"), "selected=1").unwrap();
    assert_eq!(
        capture_file(&root.path().join("link/../config"), usize::MAX).unwrap(),
        b"selected=1"
    );
    let fifo = root.path().join("fifo");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a NUL-terminated path and mkfifo retains no pointers.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_eq!(
        capture_file(&fifo, 1024).unwrap_err().kind,
        ErrorKind::NotRegular
    );
}
