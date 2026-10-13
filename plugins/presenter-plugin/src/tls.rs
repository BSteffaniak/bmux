//! Plugin-local file policy; runtime owns TLS configuration construction.
use anyhow::{Context, Result, ensure};
use std::{io::Read, path::Path, sync::Arc};

pub(crate) fn server(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let certificates = read(cert, 1024 * 1024, false)?;
    let private_key = read(key, 64 * 1024, true)?;
    Ok(bmux_presenter_runtime::transport::tls_server_config(
        &certificates,
        &private_key,
    )?)
}
pub(crate) fn client(ca: &Path) -> Result<Arc<rustls::ClientConfig>> {
    Ok(bmux_presenter_runtime::transport::tls_client_config(
        &read(ca, 1024 * 1024, false)?,
    )?)
}
fn read(path: &Path, limit: usize, private: bool) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).context("opening TLS material")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit as u64,
        "TLS material exceeds file budget"
    );
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "TLS key must not be group/world accessible"
        );
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "TLS material grew beyond budget");
    Ok(bytes)
}
