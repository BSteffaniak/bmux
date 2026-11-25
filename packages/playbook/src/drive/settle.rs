//! Waiting until a program has finished reacting, without guessing a sleep.
//!
//! A program has settled when it has written no output for a quiet window
//! and (on Linux) the processes in its terminal's foreground process group
//! have used no CPU in that window and none is running. A compile or a
//! computation that prints nothing is therefore still "busy", and a program
//! blocked reading its terminal is "idle" as soon as it stops drawing.

use std::time::{Duration, Instant};

use anyhow::Result;
use bmux_client::BmuxClient;
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4 as execution;
use serde::Serialize;

use super::session::{self, SessionRecord};

#[derive(Debug, Clone, Copy)]
pub struct SettleOptions {
    /// How long output and CPU must stay still.
    pub quiet: Duration,
    /// Give up after this long and report the program as busy.
    pub timeout: Duration,
}

impl SettleOptions {
    pub const fn new(quiet: Duration, timeout: Duration) -> Self {
        Self { quiet, timeout }
    }
}

/// How a wait for the program ended.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Settled {
    /// Output and CPU were quiet for the quiet window.
    Idle {
        after_ms: u64,
        /// Set when a foreground process is quiet because it waits for
        /// something other than input (a timer, a pipe, the network).
        #[serde(skip_serializing_if = "Option::is_none")]
        waiting_on: Option<String>,
    },
    /// The program exited.
    Exited {
        after_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    /// Still active when the timeout ran out.
    Busy { after_ms: u64, reason: String },
}

impl Settled {
    pub const fn exited(&self) -> bool {
        matches!(self, Self::Exited { .. })
    }

    /// One line for a report.
    pub fn describe(&self) -> String {
        match self {
            Self::Idle {
                after_ms,
                waiting_on: None,
            } => format!("settled after {after_ms}ms"),
            Self::Idle {
                after_ms,
                waiting_on: Some(waiting),
            } => format!(
                "quiet after {after_ms}ms, but not waiting for input: {waiting}; use `wait --text` or `wait --exit` for the outcome"
            ),
            Self::Exited {
                after_ms,
                exit_code,
                signal,
            } => {
                let how = match (exit_code, signal) {
                    (_, Some(signal)) => format!("killed by {}", signal_name(signal)),
                    (Some(code), None) => format!("exit code {code}"),
                    (None, None) => "exit status unknown".to_owned(),
                };
                format!("program EXITED ({how}) after {after_ms}ms")
            }
            Self::Busy { after_ms, reason } => {
                format!("still busy after {after_ms}ms ({reason})")
            }
        }
    }
}

/// `SIGTERM (15)` for a signal number; other descriptions as given.
fn signal_name(signal: &str) -> String {
    let name = match signal.trim().parse::<u32>() {
        Ok(1) => "SIGHUP",
        Ok(2) => "SIGINT",
        Ok(3) => "SIGQUIT",
        Ok(4) => "SIGILL",
        Ok(6) => "SIGABRT",
        Ok(7) => "SIGBUS",
        Ok(8) => "SIGFPE",
        Ok(9) => "SIGKILL",
        Ok(11) => "SIGSEGV",
        Ok(13) => "SIGPIPE",
        Ok(15) => "SIGTERM",
        _ => return signal.to_owned(),
    };
    format!("{name} ({})", signal.trim())
}

pub fn exited_from_status(status: &execution::TerminalExecutionStatus, after: Duration) -> Settled {
    Settled::Exited {
        after_ms: millis(after),
        exit_code: status.exit_code,
        signal: status.signal.clone(),
    }
}

pub fn is_running(status: &execution::TerminalExecutionStatus) -> bool {
    status.phase == execution::TerminalExecutionPhase::Running
}

/// Wait until the program settles, exits, or the timeout passes.
pub async fn settle(
    client: &mut BmuxClient,
    record: &SessionRecord,
    options: SettleOptions,
) -> Result<Settled> {
    let start = Instant::now();
    // The pane runtime learns the program's pid as it starts: ask until it knows.
    let mut pid = session::program_pid(client, record).await;
    let mut last_change = start;
    let mut last_watermark = None;
    let mut last_cpu = None;
    let mut reason = "drawing output";
    let mut next_cpu_check = start;
    let mut waiting_on = None;
    loop {
        let status = session::status(client, record).await?;
        if !is_running(&status) {
            // Give the last output a moment to reach the screen.
            if !status.output_drained {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let status = session::status(client, record).await?;
                return Ok(exited_from_status(&status, start.elapsed()));
            }
            return Ok(exited_from_status(&status, start.elapsed()));
        }
        let now = Instant::now();
        if last_watermark != Some(status.output_watermark) {
            if last_watermark.is_some() {
                last_change = now;
                reason = "still drawing output";
            }
            last_watermark = Some(status.output_watermark);
        }
        if now >= next_cpu_check {
            next_cpu_check = now + Duration::from_millis(60);
            if pid.is_none() {
                pid = session::program_pid(client, record).await;
            }
            if let Some(cpu) = pid.and_then(foreground_activity) {
                if cpu.running || last_cpu.is_some_and(|ticks| ticks != cpu.ticks) {
                    last_change = now;
                    reason = "still using CPU without drawing";
                }
                last_cpu = Some(cpu.ticks);
                waiting_on = cpu.waiting_on;
            }
        }
        if now.duration_since(last_change) >= options.quiet {
            return Ok(Settled::Idle {
                after_ms: millis(start.elapsed()),
                waiting_on,
            });
        }
        if start.elapsed() >= options.timeout {
            return Ok(Settled::Busy {
                after_ms: millis(start.elapsed()),
                reason: reason.to_owned(),
            });
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// CPU use of the terminal's foreground process group.
#[derive(Debug, Clone)]
struct Activity {
    ticks: u64,
    running: bool,
    /// A process blocked on something other than input, described.
    waiting_on: Option<String>,
}

#[cfg(target_os = "linux")]
fn foreground_activity(pid: u32) -> Option<Activity> {
    let leader = read_stat(pid)?;
    let group = leader.tpgid;
    if group <= 0 {
        return None;
    }
    let mut activity = Activity {
        ticks: 0,
        running: false,
        waiting_on: None,
    };
    for entry in std::fs::read_dir("/proc").ok()?.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(stat) = read_stat(pid) else {
            continue;
        };
        if stat.pgrp == group {
            activity.ticks = activity.ticks.saturating_add(stat.ticks);
            activity.running |= stat.state == 'R';
            if activity.waiting_on.is_none()
                && let Some(wait) = std::fs::read_to_string(format!("/proc/{pid}/wchan"))
                    .ok()
                    .as_deref()
                    .and_then(non_input_wait)
            {
                activity.waiting_on = Some(format!("`{}` is {wait}", stat.comm));
            }
        }
    }
    Some(activity)
}

#[cfg(not(target_os = "linux"))]
fn foreground_activity(_pid: u32) -> Option<Activity> {
    None
}

/// What a kernel wait channel says a quiet process waits for, when it is
/// clearly not terminal input. Ambiguous waits (poll, epoll, futexes) are
/// left alone: a program may be polling its terminal among other things.
#[cfg(target_os = "linux")]
fn non_input_wait(wchan: &str) -> Option<&'static str> {
    let wchan = wchan.trim();
    if wchan.contains("nanosleep") {
        Some("sleeping on a timer")
    } else if wchan == "pipe_read" || wchan == "pipe_write" {
        Some("waiting on a pipe")
    } else if matches!(
        wchan,
        "sk_wait_data" | "inet_csk_accept" | "tcp_recvmsg_locked" | "inet_wait_for_connect"
    ) {
        Some("waiting on the network")
    } else if wchan.starts_with("flock") || wchan == "locks_lock_inode_wait" {
        Some("waiting for a file lock")
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
struct ProcStat {
    comm: String,
    state: char,
    pgrp: i64,
    tpgid: i64,
    ticks: u64,
}

#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<ProcStat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(target_os = "linux")]
fn parse_stat(text: &str) -> Option<ProcStat> {
    // The command name may hold spaces and parentheses: fields follow the last ')'.
    let close = text.rfind(')')?;
    let comm = text.get(text.find('(')? + 1..close)?.to_owned();
    let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    Some(ProcStat {
        comm,
        state: fields.first()?.chars().next()?,
        pgrp: fields.get(2)?.parse().ok()?,
        tpgid: fields.get(5)?.parse().ok()?,
        ticks: fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_follow_the_last_parenthesis() {
        let stat = parse_stat(
            "42 (my (odd) app) S 1 40 40 34816 40 4194304 1 2 3 4 17 5 0 0 20 0 1 0 1 1 1",
        )
        .unwrap();
        assert_eq!(stat.comm, "my (odd) app");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.pgrp, 40);
        assert_eq!(stat.tpgid, 40);
        assert_eq!(stat.ticks, 22);
    }

    #[test]
    fn only_clear_non_input_waits_are_named() {
        assert_eq!(non_input_wait("hrtimer_nanosleep\n"), Some("sleeping on a timer"));
        assert_eq!(non_input_wait("pipe_read"), Some("waiting on a pipe"));
        assert_eq!(non_input_wait("do_sys_poll"), None);
        assert_eq!(non_input_wait("wait_woken"), None);
        assert_eq!(non_input_wait("do_wait"), None);
    }

    #[test]
    fn own_process_is_readable() {
        assert!(read_stat(std::process::id()).is_some());
    }
}
