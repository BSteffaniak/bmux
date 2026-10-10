//! Owner-local auxiliary artifact resolution. No discovery or executable search.
use bmux_plugin_sdk::ResolvedAuxiliaryExecutable;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::manifest::{AuxiliaryExecutableDeclaration, safe_auxiliary_name};

pub(super) fn resolve(
    base: &Path,
    declaration: &AuxiliaryExecutableDeclaration,
    owner: &str,
) -> Result<ResolvedAuxiliaryExecutable, String> {
    if !base.is_absolute() || !safe_auxiliary_name(&declaration.relative_path) {
        return Err(
            "auxiliary executable requires an absolute authority root and safe filename".into(),
        );
    }
    let base = base
        .canonicalize()
        .map_err(|error| format!("auxiliary authority root unavailable: {error}"))?;
    let path = base.join(&declaration.relative_path);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("auxiliary executable unavailable: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("auxiliary executable must be a regular file, not a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("auxiliary artifact is not executable".into());
        }
    }
    #[cfg(not(unix))]
    return Err(
        "auxiliary executable permission verification is unsupported on this platform".into(),
    );
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("auxiliary canonicalization failed: {error}"))?;
    if canonical != path {
        return Err("auxiliary executable changed identity during resolution".into());
    }
    let mut file =
        File::open(&path).map_err(|error| format!("auxiliary artifact open failed: {error}"))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev()
            || opened.ino() != metadata.ino()
            || !opened.is_file()
            || opened.mode() & 0o111 == 0
        {
            return Err("auxiliary artifact replaced during open".into());
        }
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("auxiliary checksum failed: {error}"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|error| error.to_string())?;
    if after.len() != opened.len() || after.modified().ok() != opened.modified().ok() {
        return Err("auxiliary artifact modified during checksum".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if current.dev() != opened.dev()
            || current.ino() != opened.ino()
            || after.ctime() != opened.ctime()
            || after.ctime_nsec() != opened.ctime_nsec()
            || !current.is_file()
            || current.mode() & 0o111 == 0
        {
            return Err("auxiliary artifact changed during checksum".into());
        }
    }
    let mut artifact_sha256 = String::with_capacity(64);
    for byte in hash.finalize() {
        use std::fmt::Write as _;
        write!(&mut artifact_sha256, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    validate_inventory(&base, declaration, owner, &artifact_sha256, opened.len())?;
    Ok(ResolvedAuxiliaryExecutable {
        path: path
            .to_str()
            .ok_or("auxiliary executable path is not UTF-8")?
            .into(),
        protocol_version: declaration.protocol_version,
        artifact_sha256,
    })
}

/// Installed payloads retain the inventory checked by the installer. Development
/// artifacts without an inventory retain owner-local resolution, but an existing
/// corrupt or unsupported inventory is never treated as a development install.
fn validate_inventory(
    base: &Path,
    declaration: &AuxiliaryExecutableDeclaration,
    owner: &str,
    sha256: &str,
    bytes: u64,
) -> Result<(), String> {
    let path = base.join("artifacts.json");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("auxiliary inventory unavailable: {error}")),
    };
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err("auxiliary inventory must be a bounded regular file".into());
    }
    let mut contents = Vec::new();
    File::open(&path)
        .map_err(|error| error.to_string())?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut contents)
        .map_err(|error| error.to_string())?;
    if contents.len() > 1024 * 1024 {
        return Err("auxiliary inventory exceeds bound".into());
    }
    let inventory: serde_json::Value = serde_json::from_slice(&contents)
        .map_err(|error| format!("corrupt auxiliary inventory: {error}"))?;
    if inventory
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Err("unsupported auxiliary inventory schema".into());
    }
    let artifacts = inventory
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .ok_or("auxiliary inventory lacks artifacts")?;
    let mut matches = artifacts.iter().filter(|artifact| {
        artifact
            .get("relative_path")
            .and_then(serde_json::Value::as_str)
            == Some(declaration.relative_path.as_str())
    });
    let artifact = matches
        .next()
        .ok_or("auxiliary artifact absent from inventory")?;
    if matches.next().is_some() {
        return Err("duplicate auxiliary artifact in inventory".into());
    }
    let mut expected_declaration =
        serde_json::to_value(declaration).map_err(|error| error.to_string())?;
    expected_declaration["owner"] = serde_json::Value::String(owner.into());
    if artifact.get("declaration") != Some(&expected_declaration)
        || artifact.get("sha256").and_then(serde_json::Value::as_str) != Some(sha256)
        || artifact.get("bytes").and_then(serde_json::Value::as_u64) != Some(bytes)
    {
        return Err("auxiliary artifact does not match installed inventory".into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn installed_inventory_rejects_tampering_wrong_owner_and_unknown_schema() {
        let root = std::env::temp_dir().join(format!("bmux-inventory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let spec = declaration();
        let worker = root.join("worker");
        std::fs::write(&worker, b"abc").unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let resolved = resolve(&root, &spec, "fixture.owner").unwrap();
        let mut declared = serde_json::to_value(&spec).unwrap();
        declared["owner"] = serde_json::json!("fixture.owner");
        let mut inventory = serde_json::json!({
            "schema_version": 1,
            "target": "aarch64-apple-darwin",
            "artifacts": [{
                "relative_path": "worker",
                "sha256": resolved.artifact_sha256,
                "bytes": 3,
                "declaration": declared,
            }],
            "unavailable_auxiliary_executables": [],
        });
        let path = root.join("artifacts.json");
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(resolve(&root, &spec, "fixture.owner").is_ok());
        assert!(resolve(&root, &spec, "wrong.owner").is_err());
        std::fs::write(&worker, b"xyz").unwrap();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        std::fs::write(&worker, b"abc").unwrap();
        inventory["schema_version"] = serde_json::json!(99);
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        std::fs::write(&path, b"broken").unwrap();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn declaration() -> AuxiliaryExecutableDeclaration {
        AuxiliaryExecutableDeclaration {
            id: "worker".into(),
            cargo_package: "example".into(),
            cargo_binary: "worker".into(),
            relative_path: "worker".into(),
            protocol_version: 1,
        }
    }

    #[test]
    fn missing_nonexecutable_symlink_and_traversal_fail() {
        let root = std::env::temp_dir().join(format!("bmux-aux-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut spec = declaration();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        std::fs::write(root.join("worker"), b"abc").unwrap();
        std::fs::set_permissions(root.join("worker"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        std::fs::set_permissions(root.join("worker"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let value = resolve(&root, &spec, "fixture.owner").unwrap();
        assert_eq!(
            value.artifact_sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(value.protocol_version, 1);
        symlink(root.join("worker"), root.join("link")).unwrap();
        spec.relative_path = "link".into();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        spec.relative_path = "../worker".into();
        assert!(resolve(&root, &spec, "fixture.owner").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn same_id_isolated_to_authoritative_package_root() {
        let root = std::env::temp_dir().join(format!("bmux-aux-{}", uuid::Uuid::new_v4()));
        for owner in ["first", "second"] {
            std::fs::create_dir_all(root.join(owner)).unwrap();
            std::fs::write(root.join(owner).join("worker"), owner.as_bytes()).unwrap();
            std::fs::set_permissions(
                root.join(owner).join("worker"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let first = resolve(&root.join("first"), &declaration(), "fixture.owner").unwrap();
        let second = resolve(&root.join("second"), &declaration(), "fixture.owner").unwrap();
        assert_ne!(first.path, second.path);
        assert_ne!(first.artifact_sha256, second.artifact_sha256);
        std::fs::remove_dir_all(root).unwrap();
    }
}
