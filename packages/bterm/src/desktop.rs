//! Linux desktop selection for native launches outside the desktop process tree.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Read as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

type Environment = BTreeMap<OsString, OsString>;
const KEYS: [&str; 4] = [
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
    "XAUTHORITY",
];

fn query(arguments: &[&str]) -> Result<String> {
    let mut child = Command::new("loginctl")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("query local desktop sessions with loginctl")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "local desktop session query failed");
            let mut text = String::new();
            child
                .stdout
                .take()
                .context("desktop query output missing")?
                .take(65537)
                .read_to_string(&mut text)?;
            ensure!(
                text.len() <= 65536,
                "desktop session query exceeded output limit"
            );
            return Ok(text);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("local desktop session query timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn properties(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .collect()
}

fn display_environment(bytes: &[u8]) -> Environment {
    use std::os::unix::ffi::OsStrExt as _;
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let index = entry.iter().position(|byte| *byte == b'=')?;
            let key = std::str::from_utf8(&entry[..index]).ok()?;
            (KEYS.contains(&key) && index + 1 < entry.len()).then(|| {
                (
                    OsString::from(key),
                    std::ffi::OsStr::from_bytes(&entry[index + 1..]).to_owned(),
                )
            })
        })
        .collect()
}

fn normalize(mut environment: Environment, wayland: bool) -> Option<Environment> {
    if wayland {
        environment.get(std::ffi::OsStr::new("WAYLAND_DISPLAY"))?;
        environment.get(std::ffi::OsStr::new("XDG_RUNTIME_DIR"))?;
        environment.remove(std::ffi::OsStr::new("DISPLAY"));
        environment.remove(std::ffi::OsStr::new("XAUTHORITY"));
    } else {
        environment.get(std::ffi::OsStr::new("DISPLAY"))?;
        environment.remove(std::ffi::OsStr::new("WAYLAND_DISPLAY"));
    }
    Some(environment)
}

fn select(candidates: Vec<(String, Environment)>, selected: Option<&str>) -> Result<Environment> {
    let mut candidates: Vec<_> = candidates
        .into_iter()
        .filter(|(id, _)| selected.is_none_or(|selected| id == selected))
        .collect();
    ensure!(
        !candidates.is_empty(),
        "no active local desktop found for this user; set display variables or launch from the desktop"
    );
    ensure!(
        candidates.len() == 1,
        "multiple local desktops found; select one with --desktop-session ID or set display variables"
    );
    Ok(candidates.remove(0).1)
}

pub(crate) fn discover(selected: Option<&str>) -> Result<Environment> {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: read-only identity query.
    let uid = unsafe { libc::geteuid() };
    let list = query(&["list-sessions", "--no-legend", "--no-pager"])?;
    ensure!(
        list.lines().count() <= 64,
        "too many login sessions for automatic desktop discovery; set display variables explicitly"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut sessions = Vec::new();
    let mut active_desktops = 0;
    for line in list.lines() {
        ensure!(
            Instant::now() < deadline,
            "desktop discovery timed out; set display variables explicitly"
        );
        let Some(id) = line.split_whitespace().next() else {
            continue;
        };
        if selected.is_some_and(|selected| selected != id) {
            continue;
        }
        let text = query(&[
            "show-session",
            id,
            "--no-pager",
            "-p",
            "User",
            "-p",
            "Active",
            "-p",
            "Remote",
            "-p",
            "Type",
            "-p",
            "Scope",
        ])?;
        let p = properties(&text);
        if p.get("User").and_then(|value| value.parse::<u32>().ok()) != Some(uid)
            || p.get("Active") != Some(&"yes")
            || p.get("Remote") != Some(&"no")
        {
            continue;
        }
        let wayland = match p.get("Type") {
            Some(&"wayland") => true,
            Some(&"x11") => false,
            _ => continue,
        };
        active_desktops += 1;
        ensure!(
            active_desktops == 1,
            "multiple active local desktops found; select one with --desktop-session ID"
        );
        let scope = p.get("Scope").context("desktop session scope missing")?;
        ensure!(
            scope.starts_with("session-") && scope.ends_with(".scope") && !scope.contains('/'),
            "invalid desktop session scope"
        );
        let mut candidates = Vec::new();
        for (count, entry) in std::fs::read_dir("/proc")?.enumerate() {
            ensure!(
                count < 32768 && Instant::now() < deadline,
                "desktop process discovery exceeded its bound; set display variables explicitly"
            );
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
            {
                continue;
            }
            let path = entry.path();
            if std::fs::metadata(&path).map_or(true, |metadata| metadata.uid() != uid) {
                continue;
            }
            let Ok(cgroup) = std::fs::read_to_string(path.join("cgroup")) else {
                continue;
            };
            if !cgroup
                .lines()
                .any(|line| line.split('/').any(|part| part == *scope))
            {
                continue;
            }
            let Ok(file) = std::fs::File::open(path.join("environ")) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file.take(1_048_577).read_to_end(&mut bytes).is_err() || bytes.len() > 1_048_576 {
                continue;
            }
            if let Some(environment) = normalize(display_environment(&bytes), wayland)
                && !candidates.contains(&environment)
            {
                candidates.push(environment);
            }
        }
        ensure!(
            candidates.len() <= 1,
            "desktop session has conflicting display settings; set display variables explicitly"
        );
        if let Some(environment) = candidates.pop() {
            sessions.push((id.to_owned(), environment));
        }
    }
    select(sessions, selected)
}

/// Re-exec rather than mutating process environment after threads may exist.
pub(crate) fn prepare(selected: Option<&str>, sandboxed: bool) -> Result<()> {
    if ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return Ok(());
    }
    ensure!(
        !sandboxed,
        "sandbox has no display connection; host desktop discovery is disabled"
    );
    let environment = discover(selected)?;
    use std::os::unix::process::CommandExt as _;
    let error = Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XAUTHORITY")
        .env_remove("XDG_RUNTIME_DIR")
        .envs(environment)
        .exec();
    Err(error).context("enter selected local desktop environment")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_display_keys_survive() {
        let env = display_environment(
            b"SECRET=private\0DISPLAY=:7\0XAUTHORITY=/private/path\0PATH=bad\0",
        );
        assert_eq!(env.len(), 2);
        assert!(!env.contains_key(std::ffi::OsStr::new("SECRET")));
        assert!(normalize(env, false).is_some());
    }
    #[test]
    fn wayland_does_not_mix_in_x11() {
        let env = display_environment(b"WAYLAND_DISPLAY=wayland-7\0XDG_RUNTIME_DIR=/run/user/123\0DISPLAY=:8\0XAUTHORITY=other\0");
        let env = normalize(env, true).unwrap();
        assert_eq!(env.len(), 2);
    }
    #[test]
    fn no_guessing_between_desktops() {
        let candidates = vec![
            ("2".into(), Environment::new()),
            ("5".into(), Environment::new()),
        ];
        assert!(select(candidates.clone(), None).is_err());
        assert!(select(candidates.clone(), Some("5")).is_ok());
        assert!(select(candidates, Some("9")).is_err());
        assert!(select(vec![], None).is_err());
    }
}
