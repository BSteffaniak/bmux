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
            Ok(_) => {}
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
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
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
        if unix { "bmux" } else { "bmux.exe" },
        if unix { "bmux" } else { "bmux.exe" },
        None,
    )];
    selected.push((
        if unix { "xtask" } else { "xtask.exe" },
        if unix {
            "bmux-install"
        } else {
            "bmux-install.exe"
        },
        None,
    ));
    if unix {
        for item in declarations {
            selected.push((
                field(item, "cargo_binary")?,
                field(item, "relative_path")?,
                Some(item.clone()),
            ));
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
        unavailable_auxiliary_executables: if unix {
            Vec::new()
        } else {
            declarations.to_vec()
        },
    };
    for (source_name, name, declaration) in selected {
        let (bytes, sha256) = copy(&source.join(source_name), &payload.join(name))?;
        inventory.artifacts.push(Artifact {
            relative_path: name.into(),
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
    let inventory: Inventory = serde_json::from_reader(regular(&source.join("artifacts.json"))?)?;
    if inventory.schema_version != 1 {
        return Err(error("unsupported artifact inventory"));
    }
    let unix = unix_target(&inventory.target)?;
    if unix && !inventory.unavailable_auxiliary_executables.is_empty() {
        return Err(error(
            "Unix inventory cannot claim unavailable auxiliary executables",
        ));
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
    if !unix && inventory.artifacts.len() != 2 {
        return Err(error("Windows auxiliary executables are unavailable"));
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
