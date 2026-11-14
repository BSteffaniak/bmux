use super::*;
fn document(runtime: &Path) -> String {
    format!(
        "schema_version=1\nruntime_dir={:?}\n[[approvals]]\nplugin_id='example.provider'\nrole='backend'\nartifact_sha256='{}'\nrequest_policy='example-policy-v1'\n",
        runtime.to_str().unwrap(),
        "a".repeat(64)
    )
}
#[test]
fn exact_matching_rejects_wrong_digest_role_identity() {
    let store = ArtifactApprovalStore::parse_for_runtime(
        &document(Path::new("/runtime/a")),
        Path::new("/runtime/a"),
    )
    .unwrap();
    assert!(
        store
            .find("example.provider", "backend", &"a".repeat(64))
            .is_some()
    );
    assert!(
        store
            .find("example.provider", "frontend", &"a".repeat(64))
            .is_none()
    );
    assert!(
        store
            .find("other.provider", "backend", &"a".repeat(64))
            .is_none()
    );
    assert!(
        store
            .find("example.provider", "backend", &"b".repeat(64))
            .is_none()
    );
}
#[test]
fn closed_schema_and_runtime_isolation() {
    let runtime = Path::new("/runtime/a");
    let text = document(runtime);
    for invalid in [
        text.replace("schema_version=1", "schema_version=2"),
        text.replace("role='backend'", "role='all'"),
        text.replace("role='backend'", "role='backend'\narbitrary_grant=true"),
        text.replace(&"a".repeat(64), "bad"),
        format!(
            "{text}\n[[approvals]]\nplugin_id='example.provider'\nrole='backend'\nartifact_sha256='{}'\nrequest_policy='other-policy-v1'",
            "b".repeat(64)
        ),
    ] {
        assert!(ArtifactApprovalStore::parse_for_runtime(&invalid, runtime).is_err());
    }
    assert!(ArtifactApprovalStore::parse_for_runtime(&text, Path::new("/runtime/b")).is_err());
}
#[cfg(unix)]
#[test]
fn user_ancestor_and_final_directory_symlinks_and_regular_ancestors_reject() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("bmux-approval-links-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    fs::create_dir(real.join("config")).unwrap();
    let alias = root.join("alias");
    symlink(&real, &alias).unwrap();
    let paths = |config| {
        ConfigPaths::new(
            config,
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        )
    };
    // Even root-owned aliases must not be followed as the final config directory.
    assert!(ArtifactApprovalStore::load(&paths(alias.clone())).is_err());
    // A non-root-owned ancestor link cannot stand in for a system alias.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        assert!(ArtifactApprovalStore::load(&paths(alias.join("config"))).is_err());
    }
    let regular = root.join("regular");
    fs::write(&regular, "not a directory").unwrap();
    assert!(ArtifactApprovalStore::load(&paths(regular.join("config"))).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn protected_system_aliases_preserve_absence_and_valid_approval() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let name = format!("bmux-approval-system-alias-{}", std::process::id());
    let alias_root = Path::new("/tmp").join(&name);
    fs::create_dir(&alias_root).unwrap();
    fs::set_permissions(&alias_root, fs::Permissions::from_mode(0o700)).unwrap();
    let host = alias_root.join("config");
    fs::create_dir(&host).unwrap();
    let runtime = alias_root.join("runtime");
    let paths = ConfigPaths::new(
        host.clone(),
        runtime.clone(),
        alias_root.join("data"),
        alias_root.join("state"),
    );
    assert!(ArtifactApprovalStore::load(&paths).unwrap().is_none());
    fs::write(host.join(APPROVAL_FILE), document(&runtime)).unwrap();
    assert!(ArtifactApprovalStore::load(&paths).unwrap().is_some());
    let var_host = Path::new("/var/empty").join(&name).join("config");
    let var_paths = ConfigPaths::new(
        var_host,
        PathBuf::from("/selected/runtime"),
        PathBuf::from("/selected/data"),
        PathBuf::from("/selected/state"),
    );
    // /var is a trusted system alias; its absent installation remains absent.
    assert!(ArtifactApprovalStore::load(&var_paths).unwrap().is_none());
    fs::remove_dir_all(alias_root).unwrap();
}

#[cfg(unix)]
#[test]
fn absent_invalid_insecure_and_repository_overlay_are_distinct() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "bmux-approval-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let host = root.join("host");
    let repo = root.join("repo");
    fs::create_dir(&host).unwrap();
    fs::create_dir(&repo).unwrap();
    let runtime = root.join("runtime-a");
    let paths = ConfigPaths::new(
        host.clone(),
        runtime.clone(),
        root.join("data"),
        root.join("state"),
    );
    fs::write(
        repo.join("bmux.toml"),
        "[plugins]\ntrusted_plugin_artifacts='all'",
    )
    .unwrap();
    fs::write(repo.join(APPROVAL_FILE), document(&runtime)).unwrap();
    // The loader has no cwd/merged-config input and never traverses the repository.
    assert!(ArtifactApprovalStore::load(&paths).unwrap().is_none());
    let authority = host.join(APPROVAL_FILE);
    fs::write(&authority, "corrupt").unwrap();
    assert!(ArtifactApprovalStore::load(&paths).is_err());
    fs::write(&authority, document(&runtime)).unwrap();
    assert!(ArtifactApprovalStore::load(&paths).unwrap().is_some());
    let other = ConfigPaths::new(
        host,
        root.join("runtime-b"),
        root.join("data"),
        root.join("state"),
    );
    assert!(ArtifactApprovalStore::load(&other).is_err());
    fs::set_permissions(&authority, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(ArtifactApprovalStore::load(&paths).is_err());
    fs::remove_file(&authority).unwrap();
    symlink(repo.join(APPROVAL_FILE), &authority).unwrap();
    assert!(ArtifactApprovalStore::load(&paths).is_err());
    // Test-owned tree only.
    fs::remove_dir_all(root).unwrap();
}
