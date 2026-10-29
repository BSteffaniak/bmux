use std::{os::windows::io::AsRawHandle, path::Path};

use anyhow::{Context, Result, ensure};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::{
    Storage::FileSystem::SECURITY_IDENTIFICATION, System::Pipes::GetNamedPipeServerProcessId,
};

fn validate_endpoint(path: &Path) -> Result<()> {
    let name = path
        .to_str()
        .context("observer pipe name must be Unicode")?;
    let suffix = name
        .strip_prefix(r"\\.\pipe\")
        .context("observer requires a local named pipe")?;
    ensure!(
        !suffix.is_empty() && suffix.len() <= 200 && !suffix.contains(['\\', '/', ':', '\0']),
        "invalid private observer pipe name"
    );
    Ok(())
}

/// The launching authority supplies its PID through the deliberately inherited
/// process environment. A random pipe locator alone is not authentication.
/// This value is not accepted from a pipe peer or inferred from the endpoint.
fn expected_parent() -> Result<u32> {
    let pid = std::env::var("BMUX_OBSERVER_PARENT_PID")
        .context("private observer parent authority unavailable")?
        .parse::<u32>()
        .context("invalid private observer parent authority")?;
    ensure!(pid != 0, "invalid private observer parent authority");
    Ok(pid)
}

async fn connect_to_parent(path: &Path, expected: u32) -> Result<NamedPipeClient> {
    validate_endpoint(path)?;
    ensure!(expected != 0, "invalid private observer parent authority");
    let stream = loop {
        match ClientOptions::new()
            // A server must not impersonate this output worker's credentials.
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(path)
        {
            Ok(stream) => break stream,
            Err(error) if error.raw_os_error() == Some(231) /* ERROR_PIPE_BUSY */ => {
                // The caller's existing connection deadline bounds all retries.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error).context("connect private observer pipe"),
        }
    };
    let mut server = 0;
    // SAFETY: stream owns a connected pipe handle; server is writable PID storage.
    ensure!(
        unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &raw mut server) } != 0,
        "private observer server identity unavailable"
    );
    ensure!(
        server == expected,
        "private observer server authority mismatch"
    );
    Ok(stream)
}

pub(in super::super) async fn connect(path: &Path) -> Result<NamedPipeClient> {
    connect_to_parent(path, expected_parent()?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ServerOptions;

    #[test]
    fn only_single_local_pipe_names_are_accepted() {
        for name in [
            r"\\remote\pipe\output",
            r"C:\output",
            r"\\.\pipe\",
            r"\\.\pipe\a\b",
        ] {
            assert!(validate_endpoint(Path::new(name)).is_err());
        }
        assert!(validate_endpoint(Path::new(r"\\.\pipe\private-output-test")).is_ok());
    }

    #[tokio::test]
    async fn pipe_server_must_match_explicit_parent_authority() {
        let name = format!(r"\\.\pipe\bmux-output-test-{}", uuid::Uuid::new_v4());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let wrong = std::process::id().wrapping_add(1);
        assert!(connect_to_parent(Path::new(&name), wrong).await.is_err());
        drop(server);
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let stream = connect_to_parent(Path::new(&name), std::process::id())
            .await
            .unwrap();
        server.connect().await.unwrap();
        drop(stream);
    }
}
