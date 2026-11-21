use super::{artifacts, run};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn root() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap()
}
fn write_executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
fn fixture(root: &Path) -> PathBuf {
    let build = root.join("build");
    fs::create_dir(&build).unwrap();
    for name in ["bmux", "bmux.exe", "xtask", "xtask.exe"] {
        write_executable(&build.join(name), format!("fixture:{name}").as_bytes());
    }
    build
}
fn staged(root: &Path) -> PathBuf {
    let build = fixture(root);
    let stage = root.join("stage");
    artifacts::stage(&build, &stage, "x86_64-unknown-linux-gnu").unwrap();
    stage
}
fn inventory(source: &Path) -> Value {
    serde_json::from_slice(&fs::read(source.join("artifacts.json")).unwrap()).unwrap()
}
fn change_inventory(source: &Path, mutation: impl FnOnce(&mut Value)) {
    let mut value = inventory(source);
    mutation(&mut value);
    fs::write(
        source.join("artifacts.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

#[test]
fn stage_install_slots_and_platform_policy() {
    let root = root();
    let build = fixture(root.path());
    let stage = root.path().join("stage");
    artifacts::stage(&build, &stage, "x86_64-unknown-linux-gnu").unwrap();
    let value = inventory(&stage);
    let listed: Vec<_> = value["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["relative_path"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["bmux", "bmux-install"]);
    assert!(value.get("unavailable_auxiliary_executables").is_none());
    assert!(
        value["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("declaration").is_none())
    );
    assert!(stage.join("bmux-install").is_file());
    assert!(!stage.join("install-artifacts.py").exists());
    let slot = root.path().join("slot");
    artifacts::install(&stage, &slot, Some("bmux-test")).unwrap();
    assert!(slot.join("bmux-test").is_file());
    assert_eq!(inventory(&slot), value);
    let installed: Value =
        serde_json::from_slice(&fs::read(slot.join("installation.json")).unwrap()).unwrap();
    assert_eq!(installed["binary_name"], "bmux-test");
    assert!(artifacts::install(&stage, &slot, Some("bmux-test")).is_err());
    let windows = root.path().join("windows");
    artifacts::stage(&build, &windows, "x86_64-pc-windows-msvc").unwrap();
    assert!(windows.join("bmux.exe").is_file());
    assert!(windows.join("bmux-install.exe").is_file());
    artifacts::install(
        &windows,
        &root.path().join("windows-slot"),
        Some("bmux-test"),
    )
    .unwrap();
    assert!(root.path().join("windows-slot/bmux-test.exe").is_file());
    assert!(artifacts::unix_target("unrelated-unknown-linux-malicious").is_err());
    let config = root.path().join("nfpm.yaml");
    fs::write(
        &config,
        "contents:\n  - src: bmux\n    dst: /usr/bin/bmux\n",
    )
    .unwrap();
    let generated = root.path().join("generated.yaml");
    artifacts::nfpm(&config, &generated).unwrap();
    let text = fs::read_to_string(generated).unwrap();
    assert!(text.starts_with("contents:\n  - src: bmux\n    dst: /usr/bin/bmux\n"));
    assert!(text.contains("dst: /usr/share/bmux/artifacts.json"));
    assert_eq!(text.matches("dst: /usr/bin/").count(), 1);
}

#[test]
fn legacy_inventory_with_helper_metadata_still_verifies_and_installs() {
    let root = root();
    let stage = staged(root.path());
    write_executable(&stage.join("legacy-helper"), b"legacy helper");
    let digest = {
        use sha2::{Digest, Sha256};
        Sha256::digest(b"legacy helper")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    change_inventory(&stage, |value| {
        value["unavailable_auxiliary_executables"] = json!([{"id": "windows-only"}]);
        value["artifacts"].as_array_mut().unwrap().push(json!({
            "relative_path": "legacy-helper",
            "sha256": digest,
            "bytes": 13,
            "declaration": {"id": "legacy", "owner": "legacy.owner"},
        }));
    });
    artifacts::verify(&stage).unwrap();
    let destination = root.path().join("installed");
    artifacts::install(&stage, &destination, None).unwrap();
    assert_eq!(inventory(&destination), inventory(&stage));
}

#[test]
fn missing_installer_prevents_product_stage_publication() {
    let root = root();
    let build = fixture(root.path());
    fs::remove_file(build.join("xtask")).unwrap();
    let destination = root.path().join("incomplete");
    assert!(artifacts::stage(&build, &destination, "x86_64-unknown-linux-gnu").is_err());
    assert!(!destination.exists());
}

#[test]
fn corrupt_and_missing_artifacts_never_publish_partial_installations() {
    let root = root();
    let stage = staged(root.path());
    let destination = root.path().join("bad");
    fs::write(stage.join("bmux-install"), "corrupt").unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    assert!(!destination.exists());
    fs::remove_file(stage.join("bmux-install")).unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    assert!(!destination.exists());
    let build = {
        let another = root.path().join("another");
        fs::create_dir(&another).unwrap();
        fixture(&another)
    };
    fs::remove_file(build.join("bmux")).unwrap();
    assert!(artifacts::stage(&build, &destination, "x86_64-unknown-linux-gnu").is_err());
    assert!(!destination.exists());
}

#[test]
fn inventory_rejects_traversal_duplicates_unknown_schema_and_wrong_binary() {
    let root = root();
    let stage = staged(root.path());
    let original = fs::read(stage.join("artifacts.json")).unwrap();
    for name in [
        "../escape",
        "a/b",
        "/absolute",
        "artifacts.json",
        "installation.json",
        "install-artifacts.py",
        "ARTIFACTS.JSON",
        "INSTALLATION.JSON",
        "INSTALL-ARTIFACTS.PY",
        "BMUX",
    ] {
        change_inventory(&stage, |value| {
            value["artifacts"][1]["relative_path"] = json!(name)
        });
        assert!(artifacts::install(&stage, &root.path().join("bad"), None).is_err());
        assert!(!root.path().join("bad").exists());
        fs::write(stage.join("artifacts.json"), &original).unwrap();
    }
    for mutation in 0..5 {
        change_inventory(&stage, |value| match mutation {
            0 => value["schema_version"] = json!(2),
            1 => value["target"] = json!("x86_64-pc-windows-msvc"),
            2 => value["artifacts"] = json!([]),
            3 => value["artifacts"][0]["bytes"] = json!(999),
            _ => value["artifacts"][0]["sha256"] = json!("0".repeat(64)),
        });
        assert!(artifacts::verify(&stage).is_err());
        fs::write(stage.join("artifacts.json"), &original).unwrap();
    }
    fs::remove_file(stage.join("artifacts.json")).unwrap();
    assert!(artifacts::install(&stage, &root.path().join("legacy"), None).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_files_inventory_and_parent_directories_are_refused() {
    use std::os::unix::fs::symlink;
    let root = root();
    let stage = staged(root.path());
    let destination = root.path().join("bad");
    let helper = stage.join("bmux-install");
    fs::rename(&helper, stage.join("real-helper")).unwrap();
    symlink("real-helper", &helper).unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    fs::remove_file(&helper).unwrap();
    fs::rename(stage.join("real-helper"), &helper).unwrap();
    let manifest = stage.join("artifacts.json");
    fs::rename(&manifest, stage.join("real-inventory")).unwrap();
    symlink("real-inventory", &manifest).unwrap();
    assert!(artifacts::verify(&stage).is_err());
    fs::remove_file(&manifest).unwrap();
    fs::rename(stage.join("real-inventory"), &manifest).unwrap();
    symlink(&stage, root.path().join("linked-source")).unwrap();
    assert!(artifacts::install(&root.path().join("linked-source"), &destination, None).is_err());
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, root.path().join("linked-parent")).unwrap();
    assert!(artifacts::install(&stage, &root.path().join("linked-parent/output"), None).is_err());
    assert!(!outside.join("output").exists());
    symlink(&outside, &destination).unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
}

#[cfg(unix)]
#[test]
fn immutable_activation_preserves_old_versions_and_failed_input_preserves_pointer() {
    let root = root();
    let stage = staged(root.path());
    let prefix = root.path().join("prefix");
    artifacts::activate(&stage, &prefix, "test-version").unwrap();
    let link = prefix.join("bin/bmux");
    let original = fs::read_link(&link).unwrap();
    assert_eq!(
        fs::read(original.with_file_name("bmux-install")).unwrap(),
        b"fixture:xtask"
    );
    assert!(artifacts::activate(&stage, &prefix, "test-version").is_err());
    assert_eq!(fs::read_link(&link).unwrap(), original);
    artifacts::activate(&stage, &prefix, "second-version").unwrap();
    let second = fs::read_link(&link).unwrap();
    assert_ne!(second, original);
    assert_eq!(fs::read(&original).unwrap(), b"fixture:bmux");
    fs::write(stage.join("bmux-install"), "corrupt").unwrap();
    assert!(artifacts::activate(&stage, &prefix, "bad-version").is_err());
    assert_eq!(fs::read_link(&link).unwrap(), second);
    assert!(!prefix.join("lib/bmux/versions/bad-version").exists());
    assert!(artifacts::activate(&stage, &prefix, "../escape").is_err());
}

#[cfg(unix)]
#[test]
fn activation_lock_and_symlink_prefix_are_not_bypassed() {
    use std::os::unix::fs::symlink;
    let root = root();
    let stage = staged(root.path());
    let prefix = root.path().join("prefix");
    fs::create_dir_all(prefix.join("lib/bmux/versions/.install-lock")).unwrap();
    assert!(artifacts::activate(&stage, &prefix, "version").is_err());
    assert!(prefix.join("lib/bmux/versions/.install-lock").is_dir());
    assert!(!prefix.join("bin/bmux").exists());
    symlink(&prefix, root.path().join("linked-prefix")).unwrap();
    assert!(artifacts::activate(&stage, &root.path().join("linked-prefix"), "version").is_err());
}

#[test]
fn occupied_destinations_and_invalid_slots_are_preserved() {
    let root = root();
    let stage = staged(root.path());
    let destination = root.path().join("occupied");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), "untouched").unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"untouched");
    for name in ["../escape", "bmux-", "arbitrary", "bmux-a/b"] {
        assert!(artifacts::install(&stage, &root.path().join("slot"), Some(name)).is_err());
        assert!(!root.path().join("slot").exists());
    }
}

#[test]
fn cli_rejects_unknown_duplicate_conflicting_and_missing_flags() {
    for args in [
        vec!["install", "source"],
        vec!["install", "source", "dest", "--prefix", "other"],
        vec![
            "install",
            "source",
            "dest",
            "--version",
            "v1",
            "--binary-name",
            "bmux-slot",
        ],
        vec!["stage"],
        vec!["stage", "--target", "bad", "--target", "bad"],
        vec!["stage", "--unknown", "value"],
        vec!["stage", "extra", "--target", "x86_64-unknown-linux-gnu"],
        vec!["stage", "--workspace", "."],
        vec![
            "nfpm",
            "--target",
            "x86_64-pc-windows-msvc",
            "--config",
            "c",
            "--destination",
            "d",
        ],
        vec!["auxiliary", "stage", "--target", "x86_64-unknown-linux-gnu"],
    ] {
        assert!(run(args.into_iter().map(str::to_string).collect()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn packaged_installer_accepts_explicit_install_and_implicit_install() {
    let root = root();
    let stage = staged(root.path());
    let prefix = root.path().join("prefix");
    let explicit = vec![
        "install".to_string(),
        stage.to_str().unwrap().to_string(),
        "--prefix".to_string(),
        prefix.to_str().unwrap().to_string(),
        "--version".to_string(),
        "explicit".to_string(),
    ];
    let parsed = super::invocation_args(Some("/release/bmux-install"), explicit.clone());
    assert_eq!(parsed, explicit);
    run(parsed).unwrap();
    assert!(prefix.join("lib/bmux/versions/explicit/bmux").is_file());
    let mut implicit = explicit[1..].to_vec();
    *implicit.last_mut().unwrap() = "implicit".to_string();
    run(super::invocation_args(
        Some("/release/bmux-install"),
        implicit,
    ))
    .unwrap();
    assert!(prefix.join("lib/bmux/versions/implicit/bmux").is_file());
    assert_eq!(
        super::invocation_args(Some("xtask"), explicit.clone()),
        explicit
    );
}

#[test]
fn unlisted_members_and_empty_existing_destination() {
    let root = root();
    let stage = staged(root.path());
    fs::write(stage.join("unexpected"), "untrusted").unwrap();
    assert!(artifacts::verify(&stage).is_err());
    fs::remove_file(stage.join("unexpected")).unwrap();
    let destination = root.path().join("empty");
    fs::create_dir(&destination).unwrap();
    artifacts::install(&stage, &destination, None).unwrap();
    assert!(destination.join("bmux-install").exists());
}

#[cfg(unix)]
#[test]
fn non_executable_build_artifact_is_not_published() {
    use std::os::unix::fs::PermissionsExt;
    let root = root();
    let build = fixture(root.path());
    fs::set_permissions(build.join("xtask"), fs::Permissions::from_mode(0o644)).unwrap();
    let stage = root.path().join("stage");
    assert!(artifacts::stage(&build, &stage, "x86_64-unknown-linux-gnu").is_err());
    assert!(!stage.exists());
}
