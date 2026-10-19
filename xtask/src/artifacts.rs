use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub fn error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    std::io::Error::other(message.into()).into()
}
pub fn field<'a>(item: &'a Value, name: &str) -> Result<&'a str> {
    item[name]
        .as_str()
        .ok_or_else(|| error("invalid declaration field"))
}
fn safe_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
fn reserved(name: &str) -> bool {
    [
        "bmux",
        "bmux.exe",
        "bmux-install",
        "bmux-install.exe",
        "artifacts.json",
        "installation.json",
        "install-artifacts.py",
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}
pub fn unix_target(target: &str) -> Result<bool> {
    match target {
        "aarch64-apple-darwin"
        | "x86_64-apple-darwin"
        | "aarch64-unknown-linux-gnu"
        | "x86_64-unknown-linux-gnu"
        | "aarch64-unknown-linux-musl"
        | "x86_64-unknown-linux-musl" => Ok(true),
        "x86_64-pc-windows-msvc" => Ok(false),
        _ => Err(error("unsupported release target")),
    }
}

/// Platform opt-in is declaration-owned. Legacy declarations remain Unix-only.
/// This mirrors `AuxiliaryExecutableDeclaration.supported_platforms` without
/// changing the declaration stored in the inventory.
pub(crate) fn supports_target(item: &Value, target: &str) -> Result<bool> {
    let platform = if unix_target(target)? {
        "unix"
    } else {
        "windows"
    };
    let Some(platforms) = item.get("supported_platforms") else {
        return Ok(platform == "unix");
    };
    let platforms = platforms
        .as_array()
        .ok_or_else(|| error("invalid helper platforms"))?;
    let mut seen = BTreeSet::new();
    for value in platforms {
        let value = value
            .as_str()
            .ok_or_else(|| error("invalid helper platform"))?;
        if !matches!(value, "unix" | "windows") || !seen.insert(value) {
            return Err(error("unknown or duplicate helper platform"));
        }
    }
    if seen.is_empty() {
        return Err(error("helper must support at least one platform"));
    }
    Ok(seen.contains(platform))
}

fn helper_name(name: &str, unix: bool) -> Result<String> {
    if !safe_name(name) || reserved(name) || (!unix && name.to_ascii_lowercase().ends_with(".exe"))
    {
        return Err(error("unsafe or suffixed helper name"));
    }
    Ok(if unix {
        name.to_owned()
    } else {
        format!("{name}.exe")
    })
}

/// Reuse the host's manifest parser, including ABI opt-in and declaration validation.
pub fn declarations(workspace: &Path) -> Result<Vec<Value>> {
    safe_path(workspace)?;
    let mut manifests = Vec::new();
    for entry in fs::read_dir(workspace.join("plugins"))? {
        let path = entry?.path().join("plugin.toml");
        if path.exists() {
            safe_path(&path)?;
            manifests.push(path);
        }
    }
    manifests.sort();
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    for path in manifests {
        let manifest = bmux_plugin::PluginManifest::from_path(&path)?;
        for item in &manifest.auxiliary_executables {
            if reserved(&item.relative_path)
                || !names.insert(item.relative_path.to_ascii_lowercase())
            {
                return Err(error("colliding auxiliary destination"));
            }
            let mut value = serde_json::to_value(item)?;
            value["owner"] = Value::String(manifest.id.clone());
            result.push(value);
        }
    }
    Ok(result)
}

/// Reject symlinks in every existing component, not only the final file.
pub fn safe_path(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut cursor = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                cursor.pop();
            }
            Component::CurDir => {}
            _ => cursor.push(component.as_os_str()),
        }
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(error(format!("symlink path refused: {}", cursor.display())));
            }
            Ok(metadata) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt as _;
                    // FILE_ATTRIBUTE_REPARSE_POINT covers junctions and other
                    // redirecting tags, not only Rust's symlink classification.
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(error(format!("reparse path refused: {}", cursor.display())));
                    }
                }
                #[cfg(not(windows))]
                let _ = metadata;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn regular(path: &Path) -> Result<File> {
    safe_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_FLAG_OPEN_REPARSE_POINT and FILE_SHARE_READ: do not follow the
        // leaf or permit writes/deletes while digesting or copying it.
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(error("reparse artifact refused"));
        }
    }
    if !metadata.is_file() {
        return Err(error("not a regular artifact"));
    }
    Ok(file)
}
fn identity(file: &mut File) -> Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    let mut digest = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut digest, "{byte:02x}")?;
    }
    Ok((bytes, digest))
}
fn executable(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o111 == 0 {
            return Err(error("artifact is not executable"));
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}
fn copy(source: &Path, output: &Path) -> Result<(u64, String)> {
    let mut input = regular(source)?;
    let output_path = output;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    std::io::copy(&mut input, &mut output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    output.sync_all()?;
    drop(output);
    identity(&mut regular(output_path)?)
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub relative_path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Value>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema_version: u32,
    pub target: String,
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub unavailable_auxiliary_executables: Vec<Value>,
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn publish(payload: &Path, destination: &Path) -> Result<()> {
    safe_path(destination)?;
    if destination.exists() {
        if !destination.is_dir() || fs::read_dir(destination)?.next().is_some() {
            return Err(error("destination must be absent or empty"));
        }
        fs::remove_dir(destination)?;
    }
    sync_dir(payload)?;
    fs::rename(payload, destination)?;
    sync_dir(
        destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )
}
fn staging(destination: &Path) -> Result<tempfile::TempDir> {
    safe_path(destination)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    Ok(tempfile::Builder::new()
        .prefix(".bmux-stage-")
        .tempdir_in(parent)?)
}
pub fn stage(
    source: &Path,
    destination: &Path,
    target: &str,
    declarations: &[Value],
) -> Result<()> {
    let unix = unix_target(target)?;
    let mut selected = vec![(
        if unix { "bmux" } else { "bmux.exe" }.to_owned(),
        if unix { "bmux" } else { "bmux.exe" }.to_owned(),
        None,
    )];
    selected.push((
        if unix { "xtask" } else { "xtask.exe" }.to_owned(),
        if unix {
            "bmux-install"
        } else {
            "bmux-install.exe"
        }
        .to_owned(),
        None,
    ));
    let mut unavailable = Vec::new();
    let mut names = BTreeSet::new();
    for (_, name, _) in &selected {
        names.insert(name.to_ascii_lowercase());
    }
    for item in declarations {
        if supports_target(item, target)? {
            let source_name = helper_name(field(item, "cargo_binary")?, unix)?;
            let name = helper_name(field(item, "relative_path")?, unix)?;
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(error("colliding auxiliary destination"));
            }
            selected.push((source_name, name, Some(item.clone())));
        } else {
            unavailable.push(item.clone());
        }
    }
    for (name, _, _) in &selected {
        let file = regular(&source.join(name))?;
        if unix {
            executable(&file)?;
        }
    }
    let staging = staging(destination)?;
    let payload = staging.path().join("payload");
    fs::create_dir(&payload)?;
    let mut inventory = Inventory {
        schema_version: 1,
        target: target.into(),
        artifacts: Vec::new(),
        unavailable_auxiliary_executables: unavailable,
    };
    for (source_name, name, declaration) in selected {
        let (bytes, sha256) = copy(&source.join(source_name), &payload.join(&name))?;
        inventory.artifacts.push(Artifact {
            relative_path: name,
            bytes,
            sha256,
            declaration,
        });
    }
    write_json(&payload.join("artifacts.json"), &inventory)?;
    verify(&payload)?;
    publish(&payload, destination)
}
pub fn nfpm(config: &Path, destination: &Path, declarations: &[Value]) -> Result<()> {
    let mut text = String::new();
    regular(config)?.read_to_string(&mut text)?;
    for item in declarations {
        let name = field(item, "relative_path")?;
        text.push_str(&format!("  - src: ./dist/${{TARGET}}/{name}\n    dst: /usr/bin/{name}\n    file_info:\n      mode: 0755\n"));
    }
    text.push_str("  - src: ./dist/${TARGET}/artifacts.json\n    dst: /usr/share/bmux/artifacts.json\n    file_info:\n      mode: 0644\n");
    safe_path(destination)?;
    let staging = staging(destination)?;
    let path = staging.path().join("config");
    fs::write(&path, text)?;
    File::open(&path)?.sync_all()?;
    fs::rename(path, destination)?;
    sync_dir(destination.parent().unwrap_or_else(|| Path::new(".")))
}
pub fn verify(source: &Path) -> Result<Inventory> {
    verify_with_extra_members(source, &[])
}

/// Bterm has its own authenticated manifest covering additional frontend resources.
pub(crate) fn verify_with_extra_members(
    source: &Path,
    extra_members: &[&str],
) -> Result<Inventory> {
    let inventory: Inventory = serde_json::from_reader(regular(&source.join("artifacts.json"))?)?;
    if inventory.schema_version != 1 {
        return Err(error("unsupported artifact inventory"));
    }
    let unix = unix_target(&inventory.target)?;
    for item in &inventory.unavailable_auxiliary_executables {
        if supports_target(item, &inventory.target)? {
            return Err(error("available helper marked unavailable"));
        }
    }
    let mut names = BTreeSet::new();
    for item in &inventory.artifacts {
        if !safe_name(&item.relative_path)
            || [
                "artifacts.json",
                "installation.json",
                "install-artifacts.py",
            ]
            .iter()
            .any(|reserved| item.relative_path.eq_ignore_ascii_case(reserved))
            || !names.insert(item.relative_path.to_ascii_lowercase())
        {
            return Err(error("unsafe or duplicate artifact path"));
        }
        if item.sha256.len() != 64
            || !item
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(error("invalid SHA-256"));
        }
        let mut file = regular(&source.join(&item.relative_path))?;
        if identity(&mut file)? != (item.bytes, item.sha256.clone()) {
            return Err(error("artifact identity mismatch"));
        }
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| error("non-UTF-8 artifact name"))?;
        if name != "artifacts.json"
            && !extra_members.contains(&name)
            && !inventory
                .artifacts
                .iter()
                .any(|item| item.relative_path == name)
        {
            return Err(error("unlisted release payload member"));
        }
    }
    let binary = if unix { "bmux" } else { "bmux.exe" };
    if !names.contains(binary) || names.contains(if unix { "bmux.exe" } else { "bmux" }) {
        return Err(error("inventory binary does not match target"));
    }
    if !names.contains(if unix {
        "bmux-install"
    } else {
        "bmux-install.exe"
    }) {
        return Err(error("inventory contains no Rust installer"));
    }
    if !unix {
        for item in &inventory.artifacts {
            if matches!(item.relative_path.as_str(), "bmux.exe" | "bmux-install.exe") {
                continue;
            }
            let declaration = item
                .declaration
                .as_ref()
                .ok_or_else(|| error("Windows helper lacks declaration"))?;
            if !supports_target(declaration, &inventory.target)?
                || helper_name(field(declaration, "relative_path")?, false)? != item.relative_path
            {
                return Err(error(
                    "Windows helper does not match its platform declaration",
                ));
            }
        }
    }
    Ok(inventory)
}
pub fn install(source: &Path, destination: &Path, binary_name: Option<&str>) -> Result<()> {
    let inventory = verify(source)?;
    if let Some(name) = binary_name
        && (!name.starts_with("bmux-") || !safe_name(name) || name == "bmux-")
    {
        return Err(error("slot binary name must be bmux-<slot>"));
    }
    safe_path(destination)?;
    if destination.exists() && fs::read_dir(destination)?.next().is_some() {
        return Err(error("installation requires an empty dedicated directory"));
    }
    let staging = staging(destination)?;
    let payload = staging.path().join("payload");
    fs::create_dir(&payload)?;
    for item in &inventory.artifacts {
        let name = match binary_name {
            Some(name) if item.relative_path == "bmux" => name.to_string(),
            Some(name) if item.relative_path == "bmux.exe" => format!("{name}.exe"),
            _ => item.relative_path.clone(),
        };
        let identity = copy(&source.join(&item.relative_path), &payload.join(&name))?;
        if identity != (item.bytes, item.sha256.clone())
            || crate::artifacts::identity(&mut regular(&payload.join(name))?)? != identity
        {
            return Err(error("artifact changed while copying"));
        }
    }
    write_json(&payload.join("artifacts.json"), &inventory)?;
    if let Some(name) = binary_name {
        write_json(
            &payload.join("installation.json"),
            &serde_json::json!({"schema_version": 1, "binary_name": name, "source_inventory": "artifacts.json"}),
        )?;
    }
    publish(&payload, destination)
}

#[cfg(unix)]
struct Lock(PathBuf);
#[cfg(unix)]
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}
#[cfg(unix)]
pub fn activate(source: &Path, prefix: &Path, version: &str) -> Result<()> {
    use std::os::unix::fs::symlink;
    if !version
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
    {
        return Err(error("unsafe release version"));
    }
    if !unix_target(&verify(source)?.target)? {
        return Err(error("activation requires a Unix BMUX artifact"));
    }
    safe_path(prefix)?;
    fs::create_dir_all(prefix)?;
    let prefix = fs::canonicalize(prefix)?;
    let versions = prefix.join("lib/bmux/versions");
    let bin = prefix.join("bin");
    safe_path(&versions)?;
    safe_path(&bin)?;
    fs::create_dir_all(&versions)?;
    fs::create_dir_all(&bin)?;
    let lock = versions.join(".install-lock");
    fs::create_dir(&lock)?;
    let _lock = Lock(lock);
    let destination = versions.join(version);
    if destination.exists() || fs::symlink_metadata(&destination).is_ok() {
        return Err(error("immutable release version already installed"));
    }
    let staging = staging(&destination)?;
    let payload = staging.path().join("payload");
    install(source, &payload, None)?;
    fs::rename(payload, &destination)?;
    sync_dir(&versions)?;
    let link_dir = tempfile::Builder::new()
        .prefix(".bmux-link-")
        .tempdir_in(&bin)?;
    let link = link_dir.path().join("bmux");
    symlink(destination.join("bmux"), &link)?;
    // The activation pointer is the sole intentional symlink. Atomic rename never follows it.
    fs::rename(link, bin.join("bmux"))?;
    sync_dir(&bin)
}
#[cfg(not(unix))]
pub fn activate(_source: &Path, _prefix: &Path, _version: &str) -> Result<()> {
    Err(error(
        "atomic symlink activation is Unix-only; use install for Windows",
    ))
}

#[cfg(test)]
mod platform_tests {
    use super::*;
    use serde_json::json;

    fn helper_declaration() -> Value {
        json!({
            "owner": "fixture.plugin", "id": "fixture-helper",
            "cargo_package": "fixture_helper", "cargo_binary": "fixture-helper",
            "relative_path": "fixture-helper", "protocol_version": 1,
            "supported_platforms": ["unix", "windows"]
        })
    }

    #[test]
    fn platform_selection_is_explicit_and_legacy_helpers_remain_unix_only() {
        let legacy = json!({});
        assert!(supports_target(&legacy, "x86_64-unknown-linux-gnu").unwrap());
        assert!(!supports_target(&legacy, "x86_64-pc-windows-msvc").unwrap());
        assert!(supports_target(&helper_declaration(), "x86_64-pc-windows-msvc").unwrap());
        for platforms in [
            json!([]),
            json!(["win32"]),
            json!(["unix", "unix"]),
            json!("windows"),
        ] {
            assert!(
                supports_target(
                    &json!({"supported_platforms": platforms}),
                    "x86_64-pc-windows-msvc"
                )
                .is_err()
            );
        }
        assert!(supports_target(&helper_declaration(), "unknown-target").is_err());
    }

    #[test]
    fn windows_stages_and_installs_opted_in_exe_with_verified_identity() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let build = root.path().join("build");
        fs::create_dir(&build).unwrap();
        for name in ["bmux.exe", "xtask.exe", "fixture-helper.exe"] {
            fs::write(build.join(name), format!("fixture:{name}")).unwrap();
        }
        let legacy = json!({"relative_path": "unix-helper", "cargo_binary": "unix-helper"});
        let declarations = vec![helper_declaration(), legacy.clone()];
        let destination = root.path().join("stage");
        stage(
            &build,
            &destination,
            "x86_64-pc-windows-msvc",
            &declarations,
        )
        .unwrap();
        let inventory = verify(&destination).unwrap();
        assert_eq!(inventory.artifacts.len(), 3);
        assert_eq!(inventory.unavailable_auxiliary_executables, vec![legacy]);
        let helper = inventory
            .artifacts
            .iter()
            .find(|item| item.relative_path == "fixture-helper.exe")
            .unwrap();
        assert_eq!(helper.declaration, Some(helper_declaration()));
        assert!(!destination.join("fixture-helper").exists());
        let installed = root.path().join("installed");
        install(&destination, &installed, None).unwrap();
        assert_eq!(
            fs::read(installed.join("fixture-helper.exe")).unwrap(),
            b"fixture:fixture-helper.exe"
        );
        verify(&installed).unwrap();

        let mut invalid = inventory;
        invalid
            .artifacts
            .iter_mut()
            .find(|item| item.relative_path == "fixture-helper.exe")
            .unwrap()
            .declaration
            .as_mut()
            .unwrap()["supported_platforms"] = json!(["unix"]);
        fs::write(
            destination.join("artifacts.json"),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();
        assert!(verify(&destination).is_err());
        invalid
            .artifacts
            .iter_mut()
            .find(|item| item.relative_path == "fixture-helper.exe")
            .unwrap()
            .declaration = Some(helper_declaration());
        fs::write(
            destination.join("artifacts.json"),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();
        fs::write(destination.join("fixture-helper.exe"), b"tampered").unwrap();
        assert!(verify(&destination).is_err());
    }

    #[test]
    fn missing_windows_helper_preserves_last_valid_destination() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let build = root.path().join("build");
        fs::create_dir(&build).unwrap();
        for name in ["bmux.exe", "xtask.exe"] {
            fs::write(build.join(name), b"fixture").unwrap();
        }
        let destination = root.path().join("stage");
        stage(&build, &destination, "x86_64-pc-windows-msvc", &[]).unwrap();
        let previous = fs::read(destination.join("artifacts.json")).unwrap();
        assert!(
            stage(
                &build,
                &destination,
                "x86_64-pc-windows-msvc",
                &[helper_declaration()]
            )
            .is_err()
        );
        assert_eq!(
            fs::read(destination.join("artifacts.json")).unwrap(),
            previous
        );
        verify(&destination).unwrap();
        let absent = root.path().join("absent");
        assert!(
            stage(
                &build,
                &absent,
                "x86_64-pc-windows-msvc",
                &[helper_declaration()]
            )
            .is_err()
        );
        assert!(!absent.exists());
    }
}
