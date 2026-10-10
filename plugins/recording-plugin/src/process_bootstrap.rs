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

// v1 waits may run for 30 seconds; leave bounded transport headroom without
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
    Path::new(&context.connection.data_dir).join("recording-process-v1")
}
fn paths(root: &Path) -> ConfigPaths {
    ConfigPaths::new(
        root.join("config"),
        root.join("runtime"),
        root.join("data"),
        root.join("state"),
    )
}

pub(crate) async fn bootstrap(context: &NativeCommandContext) -> Result<DedicatedRuntime> {
    let index = index_root(context);
    crate::durable::private_directory(&index)?;
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let root = base.join(format!("bmux-recorded-{}", Uuid::new_v4()));
    std::fs::create_dir(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let paths = paths(&root);
    paths.ensure_dirs()?;
    std::fs::create_dir(root.join("home"))?;
    std::fs::create_dir(root.join("tmp"))?;
    let config = isolated_config(context, &paths)?;
    let config_file = paths.config_dir.join("bmux.toml");
    crate::durable::write_atomic(&config_file, config.as_bytes())?;
    let executable = std::env::current_exe().context("locating current BMUX executable")?;
    ensure!(executable.is_absolute(), "BMUX executable is not absolute");
    let mut command = Command::new(executable);
    command.env_clear();
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
        .current_dir(&root)
        .args(["--runtime", "default", "--target", "local", "--config"])
        .arg(&config_file)
        .args(["server", "start", "--daemon"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .context("starting dedicated recorded-program server")?;
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
        schema_version: 1,
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
        value.schema_version == 1 && value.run_id == id,
        "invalid or unsupported process locator"
    );
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let canonical_base = std::fs::canonicalize(base)?;
    let root = std::fs::canonicalize(&value.root)?;
    ensure!(
        root.parent() == Some(canonical_base.as_path()),
        "process runtime outside dedicated temporary parent"
    );
    ensure!(
        root.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("bmux-recorded-")),
        "invalid dedicated runtime locator"
    );
    let client = BmuxClient::connect_with_paths_and_timeout(
        &paths(&root),
        "recording-process",
        PROCESS_REQUEST_TIMEOUT,
    )
    .await?;
    Ok(DedicatedRuntime {
        root,
        client,
        armed: false,
    })
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
        bmux_recording_plugin_api::recording_process_v1::RunStatus,
    )>,
> {
    let path = index_root(context).join(format!("{id}.json"));
    if !path.try_exists()? {
        return Ok(None);
    }
    let locator: Locator = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        locator.schema_version == 1 && locator.run_id == id,
        "invalid run locator"
    );
    let root = std::fs::canonicalize(&locator.root)?;
    let base = std::fs::canonicalize(if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    })?;
    ensure!(
        root.parent() == Some(base.as_path())
            && root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("bmux-recorded-")),
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
        let mut claim = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(format!("launch-{key}.claim")))
            .context("launch identity already admitted; refusing ambiguous duplicate")?;
        claim.write_all(b"admitted")?;
        claim.sync_all()?;
        std::fs::File::open(&root)?.sync_all()?;
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
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.env_clear();
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
            entry.ok().is_some_and(|entry| {
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
        return Ok(());
    }
    let mut runtime = resolve(context, id).await?;
    runtime.client.stop_server().await?;
    crate::durable::write_atomic(&marker, b"true")?;
    Ok(())
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
