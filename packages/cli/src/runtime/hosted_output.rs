#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostedHostState {
    Running,
    Offline,
    Stale(u32),
}

pub(super) fn status_ready_lines(next: Option<&str>) -> Vec<String> {
    let mut lines = vec!["Status: ready".to_string()];
    if let Some(value) = next {
        lines.push(format!("Next: {value}"));
    }
    lines
}

pub(super) fn status_not_ready_lines(
    reason: Option<&str>,
    fix: &str,
    advanced: Option<&str>,
) -> Vec<String> {
    let mut lines = vec!["Status: not ready".to_string()];
    if let Some(value) = reason {
        lines.push(format!("Reason: {value}"));
    }
    lines.push(format!("Fix: {fix}"));
    if let Some(value) = advanced {
        lines.push(format!("Advanced: {value}"));
    }
    lines
}

pub(super) fn hosted_not_ready_reason(
    auth_missing: bool,
    host_state: HostedHostState,
    share_missing: bool,
) -> String {
    let mut reasons = Vec::new();
    if auth_missing {
        reasons.push("not signed in".to_string());
    }
    match host_state {
        HostedHostState::Running => {}
        HostedHostState::Offline => reasons.push("host is offline".to_string()),
        HostedHostState::Stale(pid) => reasons.push(format!("host state is stale (pid {pid})")),
    }
    if share_missing && host_state == HostedHostState::Running {
        reasons.push("share link unavailable".to_string());
    }
    if reasons.is_empty() {
        "not ready".to_string()
    } else {
        reasons.join("; ")
    }
}
