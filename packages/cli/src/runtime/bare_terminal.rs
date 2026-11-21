//! Local single-terminal transport for a server with no activated backends.
//! A disconnected reader leaves the PTY and its bounded output queue in the server.
use anyhow::{Result, ensure};
use bmux_config::ConfigPaths;
use bmux_ipc::{
    IpcEndpoint,
    transport::{LocalIpcListener, LocalIpcStream},
};
use bmux_terminal::terminal;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use bmux_local_terminal::{HELLO, LIMIT, serve_attachment};

fn address(paths: &ConfigPaths, pid: u32, started: u64) -> PathBuf {
    paths.runtime_dir.join(format!("t-{pid:x}-{started:x}"))
}

/// Unknown and dynamically supplied plugins cannot opt into an unmediated
/// local shell. The coexistence declaration must come from packaged metadata.
pub(super) fn compatible(plugin: &super::plugin_runtime::ProductionPlugin) -> bool {
    plugin.registered.bundled_static
        && plugin
            .registered
            .manifest
            .provided_features
            .iter()
            .any(|f| f.as_str() == "core.local_terminal.compatible.v1")
}

pub(super) struct Listener {
    listener: LocalIpcListener,
    path: PathBuf,
}
impl Listener {
    pub(super) fn bind(paths: &ConfigPaths, pid: u32, started: u64) -> Result<Self> {
        let path = address(paths, pid, started);
        // Never honor permissive gateway/peer settings for a local shell.
        let listener = LocalIpcListener::bind(&IpcEndpoint::unix_socket(&path))?;
        Ok(Self { listener, path })
    }
    pub(super) async fn serve(&self, shell: &str) -> Result<()> {
        let mut execution = None;
        loop {
            let mut stream = self.listener.accept().await?;
            if let Err(error) = serve_attachment(&mut stream, shell, &mut execution).await {
                tracing::debug!(%error, "local terminal disconnected");
            }
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
struct RawMode;
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}
fn dimensions() -> Result<[u8; 4]> {
    let (width, height) = terminal::size()?;
    let [width_hi, width_lo] = width.to_be_bytes();
    let [height_hi, height_lo] = height.to_be_bytes();
    Ok([width_hi, width_lo, height_hi, height_lo])
}
pub(super) async fn attach(
    paths: &ConfigPaths,
    meta: &crate::connection::ServerRuntimeMetadata,
) -> Result<u8> {
    let mut stream = LocalIpcStream::connect(&IpcEndpoint::unix_socket(address(
        paths,
        meta.pid,
        meta.started_at_epoch_ms,
    )))
    .await?;
    let mut tty = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty")?;
    let mut display = File::options().write(true).open("/dev/tty")?;
    stream.write_all(HELLO).await?;
    stream.write_all(&dimensions()?).await?;
    let mut ack = [0; 8];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut ack)).await??;
    ensure!(&ack == HELLO, "incompatible local terminal server");
    let mut resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    terminal::enable_raw_mode()?;
    let guard = RawMode;
    let (mut read, mut write) = tokio::io::split(stream);
    let output = async {
        loop {
            let len = usize::try_from(read.read_u32().await?)?;
            ensure!(len <= LIMIT, "invalid local terminal frame");
            if len == 0 {
                return Ok::<u8, anyhow::Error>(read.read_u8().await?);
            }
            let mut bytes = vec![0; len];
            read.read_exact(&mut bytes).await?;
            display.write_all(&bytes)?;
        }
    };
    let input = async {
        let mut buf = [0; 4096];
        let mut tick = tokio::time::interval(Duration::from_millis(8));
        loop {
            tokio::select! {
                _ = resize.recv() => {
                    write.write_u8(2).await?;
                    write.write_all(&dimensions()?).await?;
                }
                _ = tick.tick() => {
                    let n = match tty.read(&mut buf) {
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        other => other?,
                    };
                    // Ctrl-] releases this attachment, never the running shell.
                    let keep = buf[..n].iter().position(|b| *b == 0x1d).unwrap_or(n);
                    if keep > 0 {
                        write.write_u8(1).await?;
                        write.write_u16(u16::try_from(keep)?).await?;
                        write.write_all(&buf[..keep]).await?;
                    }
                    if keep < n || n == 0 {
                        write.write_u8(0).await?;
                        return Ok::<u8, anyhow::Error>(0);
                    }
                }
            }
        }
    };
    let result = tokio::select! {
        r = input => r,
        r = output => r,
        _ = terminate.recv() => Ok(143),
        _ = hangup.recv() => Ok(129),
    };
    terminal::disable_raw_mode()?;
    drop(guard);
    result
}
