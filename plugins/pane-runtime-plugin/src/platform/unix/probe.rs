//! Bounded subprocess probes (`ps`, `lsof`) for observing processes.
use std::time::Duration;
use switchy_thread::sync::Arc;
use switchy_time::Instant;

struct InspectionChild {
    child: Option<std::process::Child>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Drop for InspectionChild {
    fn drop(&mut self) {
        if let (Some(child), Some(permit)) = (self.child.take(), self.permit.take()) {
            retire_inspection_child(child, permit);
        }
    }
}

fn retire_inspection_child(
    mut child: std::process::Child,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let _ = child.kill();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    switchy_thread::thread::spawn(move || {
        let _permit = permit;
        let _ = child.wait();
    });
}

/// Deadline-bound observation. A kernel-stuck child retains an admission slot
/// and its exact owned reaper, never extending the observation caller's wait.
pub(crate) fn bounded_probe_output(
    command: &mut std::process::Command,
    timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek};
    use std::process::Stdio;
    // Process-wide bound on concurrent OS subprocess probes (threads and pipes
    // shared by every activation in this process).
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let permit = Arc::clone(SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4))))
        .try_acquire_owned()
        .map_err(|_| std::io::Error::other("inspection reapers busy"))?;
    let mut output = tempfile::tempfile()?;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::null())
        .spawn()?;
    let mut owned = InspectionChild {
        child: Some(child),
        permit: Some(permit),
    };
    let child = owned.child.as_mut().expect("inspection child owned");
    let deadline = Instant::now() + timeout;
    let status = loop {
        let oversized = output.metadata().is_ok_and(|meta| meta.len() > 1_048_576);
        if oversized {
            return Err(std::io::Error::other(
                "process inspection output exceeds limit",
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                switchy_thread::thread::sleep(Duration::from_millis(2));
            }
            result => {
                return Err(match result {
                    Err(error) => error,
                    _ => std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "process inspection timed out",
                    ),
                });
            }
        }
    };
    if !status.success() {
        return Err(std::io::Error::other("process inspection failed"));
    }
    output.rewind()?;
    let mut bytes = Vec::new();
    output.take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        return Err(std::io::Error::other(
            "process inspection output exceeds limit",
        ));
    }
    Ok(bytes)
}
