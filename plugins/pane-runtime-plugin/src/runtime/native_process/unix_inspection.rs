//! Unix process observation and legacy user-directed signaling.
#[cfg(not(target_os = "macos"))]
use super::super::bounded_probe_output;
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

pub(in super::super) fn send_process_signal_command(
    program: &str,
    process: &bmux_pane_runtime_state::PaneProcessIdentity,
    signal: bmux_pane_runtime_state::PaneProcessSignal,
) -> anyhow::Result<()> {
    let number = match signal {
        bmux_pane_runtime_state::PaneProcessSignal::Interrupt => "INT",
        bmux_pane_runtime_state::PaneProcessSignal::Terminate => "TERM",
        bmux_pane_runtime_state::PaneProcessSignal::Kill => "KILL",
        bmux_pane_runtime_state::PaneProcessSignal::Hangup => "HUP",
    };
    let target = process
        .process_group_id
        .map(|group| format!("-{group}"))
        .or_else(|| process.pid.map(|pid| pid.to_string()))
        .ok_or_else(|| anyhow::anyhow!("pane process identity is not available"))?;
    let status = std::process::Command::new(program)
        .arg(format!("-{number}"))
        .arg("--")
        .arg(target)
        .status()
        .map_err(|error| anyhow::anyhow!("failed invoking kill: {error}"))?;
    if status.success() {
        Ok(())
    } else if let Some(code) = status.code() {
        anyhow::bail!("kill -{number} failed with exit code {code}")
    } else {
        anyhow::bail!("kill -{number} was terminated before completion")
    }
}

pub(in super::super) fn send_process_signal(
    process: &bmux_pane_runtime_state::PaneProcessIdentity,
    signal: bmux_pane_runtime_state::PaneProcessSignal,
) -> anyhow::Result<()> {
    send_process_signal_command("kill", process, signal)
}

pub(in super::super) fn process_command_looks_like_shell(command: &str, shell_path: &str) -> bool {
    let Some(first) = command.split_whitespace().next() else {
        return false;
    };
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    let shell_name = std::path::Path::new(shell_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell_path)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    name == shell_name
        || matches!(
            name.as_str(),
            "sh" | "bash"
                | "zsh"
                | "fish"
                | "nu"
                | "nushell"
                | "dash"
                | "ksh"
                | "mksh"
                | "csh"
                | "tcsh"
        )
}

#[cfg(target_os = "macos")]
pub(in super::super) fn resolve_process_working_directory(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: integer-only C storage is initialized and the kernel receives its
    // exact size. No process-table subprocess or filesystem-wide lsof scan.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(std::mem::size_of_val(&info)).ok()?;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let bytes = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|byte| byte.to_ne_bytes()[0])
        .take_while(|byte| *byte != 0)
        .collect::<Vec<_>>();
    let path = String::from_utf8(bytes).ok()?;
    std::path::Path::new(&path).is_absolute().then_some(path)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(in super::super) fn resolve_process_working_directory(pid: u32) -> Option<String> {
    let proc_cwd = std::path::PathBuf::from(format!("/proc/{pid}/cwd"));
    if let Ok(path) = std::fs::read_link(&proc_cwd)
        && !path.as_os_str().is_empty()
    {
        return Some(path.to_string_lossy().to_string());
    }

    let lsof_output = bounded_probe_output(
        std::process::Command::new("lsof").args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"]),
        Duration::from_millis(100),
    );
    if let Ok(bytes) = lsof_output {
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some(path) = line.strip_prefix('n')
                && !path.trim().is_empty()
            {
                return Some(path.to_string());
            }
        }
    }

    let bytes = bounded_probe_output(
        std::process::Command::new("ps").args(["-o", "cwd=", "-p", &pid.to_string()]),
        Duration::from_millis(100),
    )
    .ok()?;
    let cwd = String::from_utf8_lossy(&bytes).trim().to_string();
    (!cwd.is_empty()).then_some(cwd)
}
