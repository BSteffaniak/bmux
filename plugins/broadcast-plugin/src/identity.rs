//! Identity of the executable actually running this command, never PATH discovery.
use sha2::{Digest, Sha256};
use std::{io::Read, path::PathBuf, process::Stdio, time::Duration};
pub(crate) async fn installed(
    explicit: Option<&str>,
) -> Result<(PathBuf, String, String), &'static str> {
    let executable = match explicit {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe().map_err(|_| "installed_executable_unavailable")?,
    };
    if !executable.is_absolute() {
        return Err("absolute_executable_required");
    }
    let mut file =
        std::fs::File::open(&executable).map_err(|_| "installed_executable_unavailable")?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| "installed_executable_unavailable")?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let digest = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(&executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "runtime_timeout")?
    .map_err(|_| "runtime_unavailable")?;
    if !output.status.success() || output.stdout.len() > 256 {
        return Err("runtime_version");
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|_| "runtime_version")?
        .trim()
        .to_owned();
    if !version.starts_with("bmux ") {
        return Err("runtime_version");
    }
    Ok((executable, digest, version))
}
