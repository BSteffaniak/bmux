//! Owner-local auxiliary artifact resolution. No discovery or executable search.
use bmux_plugin_sdk::ResolvedAuxiliaryExecutable;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[path = "auxiliary/retained.rs"]
mod retained;

use crate::manifest::{AuxiliaryExecutableDeclaration, safe_auxiliary_name};

/// Resolve only the declared package, then pin its verified bytes for this runtime.
/// The wire contract stays unchanged; consumers never need the mutable source path.
pub(super) fn resolve_retained(
    base: &Path,
    declaration: &AuxiliaryExecutableDeclaration,
    owner: &str,
    connection: &bmux_plugin_sdk::HostConnectionInfo,
) -> Result<ResolvedAuxiliaryExecutable, String> {
    if !Path::new(&connection.state_dir).is_absolute()
        || !Path::new(&connection.runtime_dir).is_absolute()
    {
        return Err(
            "auxiliary retention requires absolute selected state and runtime roots".into(),
        );
    }
    let mut artifact = resolve(base, declaration, owner)?;
    let scope = format!("{}\0{owner}", connection.runtime_dir);
    let path = retained::retain(
        Path::new(&artifact.path),
        &Path::new(&connection.state_dir).join("auxiliary-images"),
        &scope,
        &artifact.artifact_sha256,
    )?;
    artifact.path = path
        .to_str()
        .ok_or("retained auxiliary path is not UTF-8")?
        .into();
    Ok(artifact)
}

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
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(unix) {
        "unix"
    } else {
        "unsupported"
    };
    let filename = declaration
        .platform_filename(platform)
        .ok_or("auxiliary executable is unavailable on this platform")?;
    let path = base.join(&filename);
    let (mut file, opened) = open_auxiliary_artifact(&path)?;
    #[cfg(windows)]
    let identity = windows_file_identity(&file)?;
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
    #[cfg(windows)]
    {
        let current = open_windows_regular(&path)?;
        if windows_file_identity(&current)? != identity
            || path.canonicalize().map_err(|error| error.to_string())? != path
        {
            return Err("auxiliary artifact changed identity during checksum".into());
        }
    }
    let mut artifact_sha256 = String::with_capacity(64);
    for byte in hash.finalize() {
        use std::fmt::Write as _;
        write!(&mut artifact_sha256, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    validate_inventory(
        &base,
        declaration,
        &filename,
        owner,
        &artifact_sha256,
        opened.len(),
    )?;
    Ok(ResolvedAuxiliaryExecutable {
        path: path
            .to_str()
            .ok_or("auxiliary executable path is not UTF-8")?
            .into(),
        protocol_version: declaration.protocol_version,
        artifact_sha256,
    })
}

/// Validate the leaf and keep its verified read handle alive through checksum
/// and inventory validation. Windows sharing restrictions end only when the
/// caller drops the returned File; native calls borrow rather than own it.
fn open_auxiliary_artifact(path: &Path) -> Result<(File, std::fs::Metadata), String> {
    let metadata = std::fs::symlink_metadata(path)
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
    #[cfg(not(any(unix, windows)))]
    return Err(
        "auxiliary executable permission verification is unsupported on this platform".into(),
    );
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("auxiliary canonicalization failed: {error}"))?;
    if canonical != path {
        return Err("auxiliary executable changed identity during resolution".into());
    }
    #[cfg(not(windows))]
    let file =
        File::open(path).map_err(|error| format!("auxiliary artifact open failed: {error}"))?;
    #[cfg(windows)]
    let file = open_windows_regular(path)?;
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
    Ok((file, opened))
}

#[cfg(windows)]
fn open_windows_regular(path: &Path) -> Result<File, String> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_TYPE_DISK, GetFileType,
    };
    // No write/delete sharing: a verified artifact cannot be replaced or changed
    // through a newly opened handle while this read handle remains alive. Open
    // the leaf itself, never a reparse target (including non-symlink reparse tags).
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| format!("auxiliary no-follow open failed: {error}"))?;
    let information = windows_file_information(&file)?;
    // SAFETY: the borrowed handle belongs to a live File for this call.
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || unsafe { GetFileType(file.as_raw_handle().cast()) } != FILE_TYPE_DISK
    {
        return Err("auxiliary artifact must be a regular disk file, not a reparse point".into());
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_file_information(
    file: &File,
) -> Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION, String> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: writable initialized output and a borrowed live file handle.
    unsafe {
        let mut information: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle().cast(), &raw mut information) == 0 {
            return Err(format!(
                "auxiliary handle identity unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(information)
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u64, [u8; 16]), String> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    // SAFETY: FileIdInfo requires this initialized structure and exact size;
    // the file handle is borrowed for the call. The 128-bit ID also covers ReFS.
    unsafe {
        let mut information: FILE_ID_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            std::ptr::from_mut(&mut information).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>())
                .expect("file identity structure size fits u32"),
        ) == 0
        {
            return Err(format!(
                "auxiliary file identity unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        if information.FileId.Identifier == [0; 16] {
            return Err("auxiliary filesystem did not provide a stable file identity".into());
        }
        Ok((
            information.VolumeSerialNumber,
            information.FileId.Identifier,
        ))
    }
}

/// Installed payloads retain the inventory checked by the installer. Development
/// artifacts without an inventory retain owner-local resolution, but an existing
/// corrupt or unsupported inventory is never treated as a development install.
fn validate_inventory(
    base: &Path,
    declaration: &AuxiliaryExecutableDeclaration,
    filename: &str,
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
    #[cfg(windows)]
    let inventory_file = open_windows_regular(&path)?;
    #[cfg(not(windows))]
    let inventory_file = File::open(&path).map_err(|error| error.to_string())?;
    let mut contents = Vec::new();
    inventory_file
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
    #[cfg(windows)]
    if inventory.get("target").and_then(serde_json::Value::as_str) != Some("x86_64-pc-windows-msvc")
    {
        return Err("auxiliary inventory target is incompatible with Windows resolution".into());
    }
    let artifacts = inventory
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .ok_or("auxiliary inventory lacks artifacts")?;
    let mut matches = artifacts.iter().filter(|artifact| {
        artifact
            .get("relative_path")
            .and_then(serde_json::Value::as_str)
            == Some(filename)
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_resolution_requires_opt_in_and_inventory_exe_identity() {
        let root = std::env::temp_dir().join(format!("bmux-windows-aux-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let worker = root.join("worker.exe");
        std::fs::write(&worker, b"abc").unwrap();
        let mut declaration = AuxiliaryExecutableDeclaration {
            id: "worker".into(),
            cargo_package: "example".into(),
            cargo_binary: "worker".into(),
            relative_path: "worker".into(),
            protocol_version: 1,
            supported_platforms: vec!["unix".into()],
        };
        assert!(resolve(&root, &declaration, "fixture.owner").is_err());
        declaration.supported_platforms.push("windows".into());
        let resolved = resolve(&root, &declaration, "fixture.owner").unwrap();
        assert!(resolved.path.ends_with("worker.exe"));
        let mut declared = serde_json::to_value(&declaration).unwrap();
        declared["owner"] = serde_json::json!("fixture.owner");
        let mut inventory = serde_json::json!({
            "schema_version": 1, "target": "x86_64-pc-windows-msvc",
            "artifacts": [{"relative_path": "worker.exe", "sha256": resolved.artifact_sha256,
                "bytes": 3, "declaration": declared}]
        });
        let path = root.join("artifacts.json");
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(resolve(&root, &declaration, "fixture.owner").is_ok());
        assert!(resolve(&root, &declaration, "wrong.owner").is_err());
        inventory["artifacts"][0]["relative_path"] = serde_json::json!("worker");
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(resolve(&root, &declaration, "fixture.owner").is_err());
        inventory["artifacts"][0]["relative_path"] = serde_json::json!("worker.exe");
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        std::fs::write(&worker, b"xyz").unwrap();
        assert!(resolve(&root, &declaration, "fixture.owner").is_err());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(worker).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn windows_verified_open_denies_writes_and_replacement() {
        let root = std::env::temp_dir().join(format!("bmux-windows-open-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("worker.exe");
        std::fs::write(&path, b"abc").unwrap();
        let file = open_windows_regular(&path).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::rename(&path, root.join("replacement.exe")).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert_eq!(
            windows_file_identity(&file).unwrap(),
            windows_file_identity(&open_windows_regular(&path).unwrap()).unwrap()
        );
        assert!(open_windows_regular(&root).is_err());
        drop(file);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn windows_verified_open_rejects_junctions_without_symlink_privileges() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        const SET_REPARSE_POINT: u32 = 0x0009_00a4;
        const MOUNT_POINT_TAG: u32 = 0xa000_0003;
        let root =
            std::env::temp_dir().join(format!("bmux-windows-reparse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let link = root.join("junction");
        let target = root.join("target");
        std::fs::create_dir(&link).unwrap();
        std::fs::create_dir(&target).unwrap();
        let substitute: Vec<u16> = format!("\\??\\{}", target.display())
            .encode_utf16()
            .collect();
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&MOUNT_POINT_TAG.to_le_bytes());
        let data_bytes = u16::try_from(8 + (substitute.len() + 2) * 2).unwrap();
        buffer.extend_from_slice(&data_bytes.to_le_bytes());
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        buffer.extend_from_slice(&u16::try_from(substitute.len() * 2).unwrap().to_le_bytes());
        buffer.extend_from_slice(
            &u16::try_from((substitute.len() + 1) * 2)
                .unwrap()
                .to_le_bytes(),
        );
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        for unit in substitute.into_iter().chain([0, 0]) {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&link)
            .unwrap();
        let mut returned = 0_u32;
        // SAFETY: handle belongs to the open directory, buffer is a complete
        // mount-point reparse structure, and no asynchronous IO is requested.
        unsafe {
            assert_ne!(
                DeviceIoControl(
                    file.as_raw_handle().cast(),
                    SET_REPARSE_POINT,
                    buffer.as_ptr().cast(),
                    u32::try_from(buffer.len()).unwrap(),
                    std::ptr::null_mut(),
                    0,
                    &raw mut returned,
                    std::ptr::null_mut()
                ),
                0
            );
        }
        drop(file);
        assert!(open_windows_regular(&link).is_err());
        std::fs::remove_dir(link).unwrap();
        std::fs::remove_dir(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn retained_resolution_preserves_inventory_checks_and_selected_scope() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        let state = root.path().join("state");
        std::fs::create_dir(&package).unwrap();
        std::fs::create_dir(&state).unwrap();
        let worker = package.join("worker");
        std::fs::write(&worker, b"original").unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut connection = bmux_plugin_sdk::HostConnectionInfo {
            config_dir: String::new(),
            config_dir_candidates: Vec::new(),
            data_dir: String::new(),
            state_dir: state.to_str().unwrap().into(),
            runtime_dir: root.path().join("runtime-a").to_str().unwrap().into(),
        };
        let spec = declaration();
        let first = resolve_retained(&package, &spec, "owner", &connection).unwrap();
        connection.runtime_dir = root.path().join("runtime-b").to_str().unwrap().into();
        let second = resolve_retained(&package, &spec, "owner", &connection).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(first.artifact_sha256, second.artifact_sha256);
        // Invalid authoritative metadata must not be bypassed using a cached image.
        std::fs::write(package.join("artifacts.json"), br#"{"schema_version":99}"#).unwrap();
        assert!(resolve_retained(&package, &spec, "owner", &connection).is_err());
        std::fs::remove_dir_all(&package).unwrap();
        assert_eq!(std::fs::read(&first.path).unwrap(), b"original");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"original");
        assert!(resolve_retained(&package, &spec, "owner", &connection).is_err());
    }

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
            supported_platforms: vec!["unix".into()],
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
