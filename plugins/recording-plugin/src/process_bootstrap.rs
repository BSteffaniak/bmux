//! Plugin-owned isolated server bootstrap and durable locator routing.
use anyhow::{Context, Result, ensure};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_plugin_sdk::NativeCommandContext;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

// Process waits may run for 30 seconds; leave bounded transport headroom without
// changing the caller's default configuration or durable principal identity.
const PROCESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Serialize, Deserialize)]
pub(crate) struct Locator {
    schema_version: u32,
    run_id: Uuid,
    pub(crate) root: PathBuf,
}

pub(crate) struct DedicatedRuntime {
    pub(crate) root: PathBuf,
    pub(crate) client: BmuxClient,
    pub(crate) armed: bool,
}

fn index_root(context: &NativeCommandContext) -> PathBuf {
    Path::new(&context.connection.data_dir).join("recording-process-v4")
}
fn paths(root: &Path) -> ConfigPaths {
    ConfigPaths::new(
        root.join("config"),
        root.join("runtime"),
        root.join("data"),
        root.join("state"),
    )
}

pub(crate) async fn connect_owned(root: &Path, name: &str) -> Result<BmuxClient> {
    validate_runtime_socket(root)?;
    Ok(
        BmuxClient::connect_with_paths_and_timeout(&paths(root), name, PROCESS_REQUEST_TIMEOUT)
            .await?,
    )
}

fn dedicated_root_name() -> String {
    // 90 random bits fit alongside the standard 56-byte macOS TMPDIR and
    // /runtime/server.sock. Exclusive creation remains the collision authority.
    format!("r-{}", &Uuid::new_v4().simple().to_string()[..24])
}

fn is_dedicated_root(root: &Path) -> bool {
    root.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.strip_prefix("r-")
                .is_some_and(|id| id.len() == 24 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                || name
                    .strip_prefix("bmux-recorded-")
                    .is_some_and(|id| Uuid::parse_str(id).is_ok())
        })
}

fn create_dedicated_root(base: &Path) -> Result<PathBuf> {
    for _ in 0..16 {
        let root = base.join(dedicated_root_name());
        // Validate before creating state; never escape the selected volume.
        validate_runtime_socket(&root)?;
        match crate::platform::create_private_directory(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("exclusive dedicated runtime allocation exhausted; target not launched")
}

fn validate_runtime_socket(root: &Path) -> Result<()> {
    crate::platform::validate_runtime_socket(&paths(root).server_socket())
        .context("dedicated runtime endpoint invalid; target not launched")
}

fn configure_isolated_slot(command: &mut Command, root: &Path, executable: &Path) -> Result<()> {
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            if cfg!(windows) {
                name.strip_suffix(".exe").unwrap_or(name)
            } else {
                name
            }
        })
        .and_then(|name| name.strip_prefix("bmux-"));
    if let Some(name) = name {
        ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "invalid installed slot name"
        );
        let quoted = |path: &Path| serde_json::to_string(&path.to_string_lossy());
        let manifest = root.join("config/slots.toml");
        let contents = format!(
            "[slots.{}]\nbinary = {}\ninherit_base = false\nconfig_dir = {}\nruntime_dir = {}\ndata_dir = {}\nstate_dir = {}\nlog_dir = {}\n",
            serde_json::to_string(name)?,
            quoted(executable)?,
            quoted(&root.join("config"))?,
            quoted(&root.join("runtime"))?,
            quoted(&root.join("data"))?,
            quoted(&root.join("state"))?,
            quoted(&root.join("logs"))?
        );
        if manifest.try_exists()? {
            ensure!(
                std::fs::read_to_string(&manifest)? == contents,
                "isolated slot declaration changed"
            );
        } else {
            crate::durable::write_atomic(&manifest, contents.as_bytes())?;
        }
        command
            .env("BMUX_SLOT_NAME", name)
            .env("BMUX_SLOTS_MANIFEST", manifest);
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct RuntimeIdentity {
    schema_version: u32,
    executable: PathBuf,
    executable_sha256: String,
    config_sha256: String,
}
fn file_hash(path: &Path) -> Result<String> {
    use std::fmt::Write;
    use std::io::Read;
    let started = Instant::now();
    let mut file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    tracing::info!(bytes = before.len(), "recording.identity.hash_start");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    ensure!(
        same_hash_file(&before, &after) && same_hash_file(&after, &std::fs::metadata(path)?),
        "identity file changed while hashing"
    );
    let digest = hasher.finalize();
    tracing::info!(
        bytes = after.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "recording.identity.hash_complete"
    );
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}
fn same_hash_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    if left.len() != right.len() || left.modified().ok() != right.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn start_owned(root: &Path, executable: &Path) -> Result<std::process::Output> {
    let paths = paths(root);
    ensure!(executable.is_absolute(), "BMUX executable is not absolute");
    let diagnostics_path = root.join(format!("server-launch-{}.stderr", Uuid::new_v4()));
    let diagnostics = crate::platform::create_new(&diagnostics_path)
        .context("creating dedicated server launch diagnostics")?;
    let mut command = Command::new(executable);
    command.env_clear();
    // Preserve OS loader context and the same endpoint namespace as the caller.
    // These select paths/loader behavior, not authorization identity.
    for name in ["SystemRoot", "WINDIR", "USER", "USERNAME", "USERDOMAIN"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    configure_isolated_slot(&mut command, root, executable)?;
    command.env(
        "PATH",
        std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
    );
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", &paths.config_dir)
        .env("XDG_DATA_HOME", &paths.data_dir)
        .env("XDG_STATE_HOME", &paths.state_dir)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", &paths.runtime_dir)
        .env("TMPDIR", root.join("tmp"))
        .env("BMUX_CONFIG_DIR", &paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &paths.runtime_dir)
        .env("BMUX_DATA_DIR", &paths.data_dir)
        .env("BMUX_STATE_DIR", &paths.state_dir)
        .env("BMUX_LOG_DIR", root.join("logs"))
        .env("BMUX_RECORDINGS_DIR", paths.state_dir.join("recordings"))
        .env("BMUX_NO_BASE_CONFIG", "1")
        .env("BMUX_RUNTIME_NAME", "default")
        .env("BMUX_TARGET", "local")
        .current_dir(root)
        .args(["--runtime", "default", "--target", "local", "--config"])
        .arg(root.join("config/bmux.toml"))
        .args(["server", "start", "--daemon"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostics));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The retained recording runtime must outlive its interactive caller.
        // SAFETY: only async-signal-safe setsid runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS};
        // A dedicated recording runtime outlives its caller's console and SSH
        // job. If the enclosing job forbids breakaway, fail rather than launch
        // a runtime whose lifetime silently depends on the caller.
        command.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
    }
    // Do not capture a pipe: daemon descendants may retain its write handle,
    // so output()/wait_with_output() can wait forever after the launcher exits.
    let status = command
        .status()
        .context("starting dedicated recorded-program server")?;
    let stderr = read_launch_diagnostics(&diagnostics_path)?;
    Ok(std::process::Output {
        status,
        stdout: Vec::new(),
        stderr,
    })
}

fn read_launch_diagnostics(path: &Path) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).context("opening dedicated server launch diagnostics")?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(16_384)))?;
    let mut bytes = Vec::new();
    file.take(16_384)
        .read_to_end(&mut bytes)
        .context("reading dedicated server launch diagnostics")?;
    Ok(bytes)
}

pub(crate) async fn bootstrap(context: &NativeCommandContext) -> Result<DedicatedRuntime> {
    let index = index_root(context);
    crate::durable::private_directory(&index)?;
    let base = std::fs::canonicalize(std::env::temp_dir())
        .context("resolving dedicated platform temporary parent (TMPDIR)")?;
    let root = create_dedicated_root(&base).context("allocating private dedicated runtime root")?;
    let paths = paths(&root);
    paths
        .ensure_dirs()
        .context("creating dedicated runtime paths")?;
    crate::platform::create_private_directory(&root.join("home"))
        .context("creating dedicated home")?;
    crate::platform::create_private_directory(&root.join("tmp"))
        .context("creating dedicated temporary directory")?;
    let config = isolated_config(context, &paths)?;
    let config_file = paths.config_dir.join("bmux.toml");
    crate::durable::write_atomic(&config_file, config.as_bytes())?;
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    let identity_executable = executable.clone();
    let identity_config = config_file.clone();
    let (executable_sha256, config_sha256) = tokio::task::spawn_blocking(move || {
        Ok::<_, anyhow::Error>((
            file_hash(&identity_executable)?,
            file_hash(&identity_config)?,
        ))
    })
    .await
    .context("identity hashing worker failed")??;
    let identity = RuntimeIdentity {
        schema_version: 4,
        executable: executable.clone(),
        executable_sha256,
        config_sha256,
    };
    crate::durable::write_atomic(
        &root.join("runtime-identity.json"),
        &serde_json::to_vec(&identity)?,
    )?;
    tracing::info!("recording.bootstrap.server_start");
    let start_root = root.clone();
    let start_executable = executable.clone();
    let output = tokio::task::spawn_blocking(move || start_owned(&start_root, &start_executable))
        .await
        .context("server bootstrap worker failed")??;
    tracing::info!("recording.bootstrap.server_start_complete");
    if !output.status.success() {
        let cleanup = stop_owned(&root);
        anyhow::bail!(
            "dedicated server failed: {}; cleanup: {cleanup:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let started = Instant::now();
    loop {
        match BmuxClient::connect_with_paths_and_timeout(
            &paths,
            "recording-process",
            PROCESS_REQUEST_TIMEOUT,
        )
        .await
        {
            Ok(client) => {
                return Ok(DedicatedRuntime {
                    root,
                    client,
                    armed: true,
                });
            }
            Err(error) if started.elapsed() >= Duration::from_secs(10) => {
                stop_owned(&root)?;
                return Err(error.into());
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

fn isolated_config(context: &NativeCommandContext, paths: &ConfigPaths) -> Result<String> {
    let runtime = serde_json::to_string(&paths.runtime_dir.to_string_lossy())?;
    let mut enabled = std::collections::BTreeSet::from([
        "bmux.recording".to_owned(),
        "bmux.sessions".to_owned(),
        "bmux.clients".to_owned(),
        "bmux.pane_runtime".to_owned(),
    ]);
    loop {
        let mut additions = Vec::new();
        for plugin in &context.registered_plugins {
            if !enabled.contains(&plugin.id) {
                continue;
            }
            for capability in &plugin.required_capabilities {
                let providers: Vec<_> = context
                    .registered_plugins
                    .iter()
                    .filter(|candidate| candidate.provided_capabilities.contains(capability))
                    .collect();
                // Host capability resolution already includes neutral built-ins.
                // Only require a plugin provider when discovery actually declares one;
                // never invent plugins for generic host primitives such as commands/storage.
                if providers.is_empty() && context.available_capabilities.contains(capability) {
                    continue;
                }
                if providers
                    .iter()
                    .any(|candidate| enabled.contains(&candidate.id))
                {
                    continue;
                }
                ensure!(
                    providers.len() == 1,
                    "required capability {capability} has no unique isolated provider"
                );
                additions.push(providers[0].id.clone());
            }
        }
        let previous = enabled.len();
        enabled.extend(additions);
        if enabled.len() == previous {
            break;
        }
    }
    let enabled_json = serde_json::to_string(&enabled)?;
    let mut config = format!(
        "[general]\ndefault_shell = '/bin/sh'\n[plugins]\nenabled = {enabled_json}\n[plugins.settings.\"bmux.recording\"]\nprocess-runtime-root = {runtime}\n"
    );
    let disabled: Vec<_> = context
        .registered_plugins
        .iter()
        .filter(|plugin| !enabled.contains(&plugin.id))
        .map(|plugin| plugin.id.clone())
        .collect();
    if !disabled.is_empty() {
        let disabled = serde_json::to_string(&disabled)?;
        config = config.replace(
            "[plugins]\n",
            &format!("[plugins]\ndisabled = {disabled}\n"),
        );
    }
    // Preserve explicit plugin discovery roots, never inherited user product settings.
    if !context.plugin_search_roots.is_empty() {
        let roots = serde_json::to_string(&context.plugin_search_roots)?;
        config = config.replace(
            "[plugins]\n",
            &format!("[plugins]\nsearch_paths = {roots}\n"),
        );
    }
    Ok(config)
}

pub(crate) fn remember(context: &NativeCommandContext, id: Uuid, root: &Path) -> Result<()> {
    let index = index_root(context);
    let value = Locator {
        schema_version: 4,
        run_id: id,
        root: root.to_path_buf(),
    };
    crate::durable::write_atomic(
        &index.join(format!("{id}.json")),
        &serde_json::to_vec(&value)?,
    )
}

pub(crate) async fn resolve(context: &NativeCommandContext, id: Uuid) -> Result<DedicatedRuntime> {
    let index = index_root(context);
    let value: Locator = serde_json::from_slice(&std::fs::read(index.join(format!("{id}.json")))?)?;
    ensure!(
        value.schema_version == 4 && value.run_id == id,
        "invalid or unsupported process locator"
    );
    let canonical_base = std::fs::canonicalize(std::env::temp_dir())?;
    let root = std::fs::canonicalize(&value.root)?;
    validate_runtime_socket(&root)?;
    ensure!(
        root.parent() == Some(canonical_base.as_path()),
        "process runtime outside dedicated temporary parent"
    );
    ensure!(
        is_dedicated_root(&root),
        "invalid dedicated runtime locator"
    );
    let client = match BmuxClient::connect_with_paths_and_timeout(
        &paths(&root),
        "recording-process",
        PROCESS_REQUEST_TIMEOUT,
    )
    .await
    {
        Ok(client) => client,
        Err(connect_error) => {
            ensure!(
                !root.join("server-stopped.json").try_exists()?,
                "finalized runtime was explicitly stopped; offline evidence remains available"
            );
            let identity: RuntimeIdentity =
                serde_json::from_slice(&std::fs::read(root.join("runtime-identity.json"))?)?;
            let identity_executable = identity.executable.clone();
            let identity_config = root.join("config/bmux.toml");
            let (executable_hash, config_hash) = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    file_hash(&identity_executable)?,
                    file_hash(&identity_config)?,
                ))
            })
            .await
            .context("restart identity hashing worker failed")??;
            ensure!(
                identity.schema_version == 4
                    && identity.executable == std::fs::canonicalize(std::env::current_exe()?)?
                    && identity.executable_sha256 == executable_hash
                    && identity.config_sha256 == config_hash,
                "owned runtime executable/config identity changed; refusing restart"
            );
            let start_root = root.clone();
            let output =
                tokio::task::spawn_blocking(move || start_owned(&start_root, &identity.executable))
                    .await
                    .context("owned restart worker failed")??;
            ensure!(
                output.status.success(),
                "owned daemon restart failed after {connect_error}: {}; retained root {}",
                String::from_utf8_lossy(&output.stderr),
                root.display()
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match BmuxClient::connect_with_paths_and_timeout(
                    &paths(&root),
                    "recording-process",
                    PROCESS_REQUEST_TIMEOUT,
                )
                .await
                {
                    Ok(client) => break client,
                    Err(error) if Instant::now() >= deadline => {
                        return Err(anyhow::anyhow!(
                            "owned daemon reconnect unavailable: {error}; retained root {}; no execution relaunch",
                            root.display()
                        ));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        }
    };
    Ok(DedicatedRuntime {
        root,
        client,
        armed: false,
    })
}

pub(crate) async fn connect_finalized(root: &Path) -> Result<BmuxClient> {
    // This is observation only: never restart a finalized runtime or route to
    // the user's default server just to refresh a cleanup receipt.
    BmuxClient::connect_with_paths_and_timeout(
        &paths(root),
        "recording-process",
        PROCESS_REQUEST_TIMEOUT,
    )
    .await
    .map_err(Into::into)
}

pub(crate) fn recording_root(runtime: &DedicatedRuntime) -> PathBuf {
    runtime.root.join("state/recordings")
}

pub(crate) fn offline(
    context: &NativeCommandContext,
    id: Uuid,
) -> Result<
    Option<(
        PathBuf,
        bmux_recording_plugin_api::recording_process_v4::RunStatus,
    )>,
> {
    let path = index_root(context).join(format!("{id}.json"));
    if !path.try_exists()? {
        return Ok(None);
    }
    let locator: Locator = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        locator.schema_version == 4 && locator.run_id == id,
        "invalid run locator"
    );
    let root = std::fs::canonicalize(&locator.root)?;
    let base = std::fs::canonicalize(std::env::temp_dir())?;
    validate_runtime_socket(&root)?;
    ensure!(
        root.parent() == Some(base.as_path()) && is_dedicated_root(&root),
        "invalid dedicated offline locator"
    );
    let recordings = root.join("state/recordings");
    let status = crate::process_store::ProcessStore::new(&recordings)?.load(id)?;
    ensure!(
        std::fs::canonicalize(&status.runtime_root)? == root,
        "offline runtime identity mismatch"
    );
    Ok(status.capture_finalized.then_some((recordings, status)))
}

pub(crate) fn launch_receipt(
    context: &NativeCommandContext,
    key: Uuid,
    args: &[String],
) -> Result<Option<Uuid>> {
    let path = index_root(context).join(format!("launch-{key}.json"));
    if !path.try_exists()? {
        return Ok(None);
    }
    let (previous, id): (String, Option<Uuid>) = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        previous == fingerprint(args)?,
        "conflicting reuse of launch idempotency key"
    );
    Ok(Some(id.context(
        "launch outcome ambiguous; refusing duplicate launch",
    )?))
}
pub(crate) fn retain_launch_root(
    context: &NativeCommandContext,
    key: Uuid,
    root: &Path,
) -> Result<PathBuf> {
    let path = index_root(context).join(format!("launch-{key}-runtime.json"));
    crate::durable::write_atomic(
        &path,
        &serde_json::to_vec(&serde_json::json!({
            "schema_version":4,"launch_mutation_id":key,"runtime_root":root,
            "contract":"recording-process-v4","recovery_required":true,
            "automatic_relaunch":false,"automatic_cleanup":false
        }))?,
    )?;
    Ok(path)
}

pub(crate) fn record_native_launch_identity(
    path: &Path,
    key: Uuid,
    root: &Path,
    id: Uuid,
    recovery_required: bool,
) -> Result<()> {
    crate::durable::write_atomic(
        path,
        &serde_json::to_vec(&serde_json::json!({
            "schema_version":4,"launch_mutation_id":key,"runtime_root":root,"run_id":id,
            "contract":"recording-process-v4","recovery_required":recovery_required,
            "automatic_relaunch":false,"automatic_cleanup":false
        }))?,
    )
}

pub(crate) fn record_launch(
    context: &NativeCommandContext,
    key: Uuid,
    args: &[String],
    id: Option<Uuid>,
) -> Result<()> {
    let root = index_root(context);
    crate::durable::private_directory(&root)?;
    if id.is_none() {
        use std::io::Write;
        let mut claim = crate::platform::create_new(&root.join(format!("launch-{key}.claim")))
            .context("launch identity already admitted; refusing ambiguous duplicate")?;
        claim.write_all(b"admitted")?;
        claim.sync_all()?;
        crate::platform::sync_directory(&root).context("publishing launch admission directory")?;
    }
    crate::durable::write_atomic(
        &root.join(format!("launch-{key}.json")),
        &serde_json::to_vec(&(fingerprint(args)?, id))?,
    )
}

impl Drop for DedicatedRuntime {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = stop_owned(&self.root) {
            eprintln!("dedicated runtime cleanup failed: {error}");
        }
    }
}
fn stop_owned(root: &Path) -> Result<()> {
    let identity: RuntimeIdentity =
        serde_json::from_slice(&std::fs::read(root.join("runtime-identity.json"))?)?;
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    ensure!(
        identity.schema_version == 4
            && identity.executable == executable
            && identity.executable_sha256 == file_hash(&executable)?
            && identity.config_sha256 == file_hash(&root.join("config/bmux.toml"))?,
        "owned cleanup executable/config identity mismatch"
    );
    let mut command = Command::new(&executable);
    command.env_clear();
    configure_isolated_slot(&mut command, root, &executable)?;
    command.env(
        "PATH",
        std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
    );
    command
        .env("BMUX_RUNTIME_DIR", root.join("runtime"))
        .env("BMUX_CONFIG_DIR", root.join("config"))
        .env("BMUX_DATA_DIR", root.join("data"))
        .env("BMUX_STATE_DIR", root.join("state"))
        .env("HOME", root.join("home"))
        .env("BMUX_NO_BASE_CONFIG", "1")
        .args(["--runtime", "default", "--target", "local", "--config"])
        .arg(root.join("config/bmux.toml"))
        .args(["server", "stop"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = command.status()?;
    if !status.success() {
        let runtime = paths(root).runtime_dir;
        let has_endpoint = std::fs::read_dir(&runtime)?.any(|entry| {
            entry.is_ok_and(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".sock") || name.contains("server.pid")
            })
        });
        ensure!(
            !has_endpoint,
            "owned server stop failed with retained endpoint/process marker"
        );
    }
    Ok(())
}
pub(crate) async fn cleanup(context: &NativeCommandContext, id: Uuid) -> Result<()> {
    let path = index_root(context).join(format!("{id}.json"));
    let locator: Locator = serde_json::from_slice(&std::fs::read(path)?)?;
    let marker = locator.root.join("server-stopped.json");
    ensure!(
        offline(context, id)?.is_some(),
        "cleanup requires finalized capture"
    );
    if marker.try_exists()? {
        ensure!(
            std::fs::read(&marker)? == b"true",
            "invalid owned server cleanup acknowledgment"
        );
        return Ok(());
    }
    let mut runtime = resolve(context, id).await?;
    runtime.client.stop_server().await?;
    crate::durable::write_atomic(&marker, b"true")?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn macos_length_tmpdir_supports_private_root_and_real_socket() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // An exclusively owned fixture with the same byte length as macOS's
        // default /private/var/folders/.../T, without mutating process TMPDIR.
        let base = PathBuf::from(format!("/tmp/bp-{}", Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .unwrap();
        let base = base.canonicalize().unwrap();
        let parent = base.join("x".repeat(55 - base.as_os_str().len()));
        std::fs::create_dir(&parent).unwrap();
        assert_eq!(parent.as_os_str().len(), 56);
        let root = create_dedicated_root(&parent).unwrap();
        assert_eq!(root.parent(), Some(parent.as_path()));
        assert!(is_dedicated_root(&root));
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        paths(&root).ensure_dirs().unwrap();
        let socket = paths(&root).server_socket();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let _client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let _server = listener.accept().unwrap();
        drop(listener);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn root_locator_recognizes_compact_and_existing_names_only() {
        assert!(is_dedicated_root(Path::new(&dedicated_root_name())));
        assert!(is_dedicated_root(Path::new(&format!(
            "bmux-recorded-{}",
            Uuid::nil()
        ))));
        for name in ["r-", "r-../../elsewhere", "bmux-recorded-garbage"] {
            assert!(!is_dedicated_root(Path::new(name)));
        }
    }

    #[test]
    fn owned_short_tmpdir_root_accepts_platform_socket_bounds() {
        let root =
            PathBuf::from("/Volumes/ehdd/tmp").join(format!("bmux-recorded-{}", Uuid::nil()));
        validate_runtime_socket(&root).unwrap();
    }

    #[test]
    fn oversized_tmpdir_root_is_rejected_without_fallback_or_creation() {
        let root = PathBuf::from("/Volumes/ehdd/tmp")
            .join("x".repeat(300))
            .join(format!("bmux-recorded-{}", Uuid::nil()));
        let error = validate_runtime_socket(&root).unwrap_err();
        // The Unix backend owns socket limits and their actionable diagnostic.
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("dedicated runtime endpoint invalid; target not launched"));
        assert!(diagnostic.contains("Unix socket path exceeds platform bounds"));
        assert!(diagnostic.contains("select shorter TMPDIR"));
        assert!(!root.exists());
    }
}

fn fingerprint(args: &[String]) -> Result<String> {
    use std::fmt::Write;
    let digest = Sha256::digest(serde_json::to_vec(args)?);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}

#[cfg(test)]
mod launch_diagnostic_tests {
    use super::*;

    #[test]
    fn launch_diagnostic_tail_is_bounded_and_reads_without_eof() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("bmux-launch-log-{}", Uuid::new_v4()));
        crate::durable::private_directory(&root).unwrap();
        let path = root.join("launch.stderr");
        let mut writer = crate::platform::create_new(&path).unwrap();
        writer.write_all(&vec![b'x'; 20_000]).unwrap();
        writer.write_all(b"last startup failure").unwrap();
        writer.flush().unwrap();
        // A live descendant can retain this writer: reading the file tail must
        // not depend on every inheriting process closing it, unlike a pipe EOF.
        let bytes = read_launch_diagnostics(&path).unwrap();
        assert_eq!(bytes.len(), 16_384);
        assert!(bytes.ends_with(b"last startup failure"));
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod installed_slot_tests {
    use super::*;

    #[test]
    fn dotted_installed_slot_is_one_quoted_manifest_key_and_stays_isolated() {
        let root = std::env::temp_dir().join(format!("bmux-slot-test-{}", Uuid::new_v4()));
        crate::durable::private_directory(&root.join("config")).unwrap();
        let executable = root.join(if cfg!(windows) {
            "bmux-release.1.exe"
        } else {
            "bmux-release.1"
        });
        let mut command = Command::new(&executable);
        configure_isolated_slot(&mut command, &root, &executable).unwrap();
        let manifest = root.join("config/slots.toml");
        let contents = std::fs::read_to_string(&manifest).unwrap();
        assert!(contents.starts_with("[slots.\"release.1\"]\n"));
        assert!(contents.contains(&format!(
            "binary = {}",
            serde_json::to_string(&executable.to_string_lossy()).unwrap()
        )));
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env.get(std::ffi::OsStr::new("BMUX_SLOT_NAME")),
            Some(&Some(std::ffi::OsStr::new("release.1")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("BMUX_SLOTS_MANIFEST")),
            Some(&Some(manifest.as_os_str()))
        );
        configure_isolated_slot(&mut command, &root, &executable).unwrap();
        let other = root.join(if cfg!(windows) {
            "bmux-other.1.exe"
        } else {
            "bmux-other.1"
        });
        assert!(configure_isolated_slot(&mut command, &root, &other).is_err());
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), contents);
        std::fs::remove_dir_all(root).unwrap();
    }
}
