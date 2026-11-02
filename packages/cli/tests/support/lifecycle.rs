//! Test-owned command observation and bounded, allowlisted failure evidence.
use std::fs;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn run(command: &mut Command, root: &Path, timeout: Duration) -> std::io::Result<Output> {
    // A descendant inheriting stdout must not keep Command::output waiting for
    // EOF forever. Files also avoid pipe-buffer deadlocks during teardown.
    let stdout = tempfile::NamedTempFile::new_in(root)?;
    let stderr = tempfile::NamedTempFile::new_in(root)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.reopen()?))
        .stderr(Stdio::from(stderr.reopen()?))
        .spawn()?;
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            // This handle is our command, never a PID read from runtime state.
            child.kill()?;
            break (child.wait()?, true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read_output = |path: &Path| -> std::io::Result<Vec<u8>> {
        if fs::metadata(path)?.len() > 8 * 1024 * 1024 {
            return Err(std::io::Error::other(
                "fixture command output exceeds 8 MiB; refusing truncation",
            ));
        }
        fs::read(path)
    };
    let error = std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "fixture command observation timed out",
    );
    if timed_out {
        return Err(error);
    }
    Ok(Output {
        status,
        stdout: read_output(stdout.path())?,
        stderr: read_output(stderr.path())?,
    })
}

pub fn keep_artifacts() -> bool {
    std::env::var_os("BMUX_TEST_KEEP_ARTIFACTS").is_some_and(|v| v == "1")
}

pub fn retain_diagnostics(root: &Path) -> std::io::Result<()> {
    let destination = root.with_file_name(format!(
        "{}-diagnostics",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::DirBuilder::new().create(&destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))?;
    }
    let mut report = String::from(
        "Test failed; runtime retired and removed. Log contents, credentials, databases, recordings, and executable images are not copied. Full retention requires BMUX_TEST_KEEP_ARTIFACTS=1.\n",
    );
    for name in ["server.stdout.log", "server.stderr.log"] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(metadata) if metadata.is_file() => {
                use std::fmt::Write as _;
                let _ = writeln!(report, "{name}: {} bytes (content omitted)", metadata.len());
            }
            Ok(_) => return Err(std::io::Error::other("unexpected diagnostic artifact")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::write(destination.join("README.txt"), report)?;
    eprintln!("bounded test diagnostics: {}", destination.display());
    Ok(())
}
