use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use bmux_playbook_plugin_api::playbook_types::{Attempt, Preparation};
use serde::{Deserialize, Serialize};

const MAX_CHANNEL_BYTES: u64 = 16 * 1024;
const STOP_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Owner {
    runs: BTreeMap<u64, Run>,
    generation: u64,
}

struct Run {
    spec: Preparation,
    original_request: Vec<u8>,
    probe_deadline: Option<Instant>,
    snapshot: Attempt,
    child: Option<Child>,
    control: Control,
    last_snapshot: Vec<u8>,
    action_labels: Vec<&'static str>,
    // Explicit host opt-in, retained only in memory; never serialized with evidence.
    application_environment: BTreeMap<String, std::ffi::OsString>,
    stop_requested: Option<Instant>,
}

#[derive(Serialize, Deserialize)]
struct RuntimeOwnership {
    generation: u64,
    attempt_root: String,
    runtime_dir: String,
}

#[derive(Default, Serialize)]
struct Control {
    generation: u64,
    paused: bool,
    permits: u64,
    stop: bool,
    finish: bool,
    playback_speed: String,
}

#[derive(Deserialize)]
struct RunnerSnapshot {
    generation: u64,
    phase: String,
    step: u64,
    #[serde(default)]
    current_action: String,
    #[serde(default)]
    action_count: u64,
    #[serde(default)]
    typing_done: u64,
    #[serde(default)]
    typing_total: u64,
    session_id: Option<String>,
    cleanup_acknowledged: bool,
    diagnostic: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) enum Command {
    Pause,
    Step,
    Resume,
    Stop,
}

type Result<T> = std::result::Result<T, String>;

impl Owner {
    pub(super) fn poll(&mut self) {
        for run in self.runs.values_mut() {
            if let Err(error) = run.refresh() {
                run.snapshot.diagnostic = Some(error.chars().take(2048).collect());
                run.snapshot.revision += 1;
            }
        }
    }

    pub(super) fn prepare(&mut self, mut spec: Preparation) -> Result<Attempt> {
        uuid::Uuid::parse_str(&spec.request_id).map_err(message)?;
        let original_request = serde_json::to_vec(&spec).map_err(message)?;
        let speed = if spec.playback_speed.is_empty() {
            bmux_playbook::PlaybackSpeed::Instant
        } else {
            bmux_playbook::PlaybackSpeed::parse(&spec.playback_speed)
                .ok_or("invalid playback speed; expected instant, half, normal, double")?
        };
        spec.playback_speed = speed.as_str().into();
        if let Some(existing) = self
            .runs
            .values_mut()
            .find(|run| run.spec.request_id == spec.request_id)
        {
            if existing.original_request != original_request {
                return Err("preparation request_id reused with different fields".into());
            }
            existing.refresh()?;
            return Ok(existing.snapshot.clone());
        }
        if self.runs.len() >= 16 {
            return Err("managed attempt capacity reached (16); retained diagnostics require explicit owner disposal".into());
        }
        if !spec.trusted_executable_input {
            return Err("playbooks are executable input; explicit trust is required".into());
        }
        let allowed = canonical(&spec.allowed_root)?;
        let source_path = Path::new(&spec.source);
        if !source_path.is_absolute() {
            return Err("source path must be absolute".into());
        }
        let source = source_path.to_path_buf();
        if !source.starts_with(&allowed) {
            return Err("playbook must be a file beneath allowed_root".into());
        }
        let executable = canonical(&spec.executable)?;
        if !executable.is_file() {
            return Err("selected native executable is not a file".into());
        }
        let attempts_root = canonical(&spec.attempts_root)?;
        if !attempts_root.is_dir() {
            return Err("attempts_root must be an existing explicitly selected directory".into());
        }
        spec.source = source.to_string_lossy().into_owned();
        spec.allowed_root = allowed.to_string_lossy().into_owned();
        spec.executable = executable.to_string_lossy().into_owned();
        spec.attempts_root = attempts_root.to_string_lossy().into_owned();
        loop {
            let bytes = uuid::Uuid::new_v4().into_bytes();
            self.generation = u64::from_le_bytes(bytes[..8].try_into().map_err(message)?) >> 1;
            if self.generation != 0 && !self.runs.contains_key(&self.generation) {
                break;
            }
        }
        let root = attempts_root.join(format!("playbook-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).map_err(message)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(message)?;
        }
        let sandbox = root.join("sandbox");
        let temporary = tempfile::Builder::new()
            .prefix("bpr-")
            .tempdir()
            .map_err(message)?;
        let runtime = temporary.path().to_path_buf();
        // Leave room for the longest currently used socket basename, including
        // server.sock. No alternate runtime is selected when TMPDIR is too deep.
        if runtime
            .join("server.sock")
            .as_os_str()
            .as_encoded_bytes()
            .len()
            >= 104
        {
            return Err(
                "selected temporary root cannot accommodate managed Unix socket paths".into(),
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).map_err(message)?;
        }
        let snapshot = Attempt {
            generation: self.generation,
            revision: 1,
            phase: "prepared".into(),
            step: 0,
            typing_done: 0,
            typing_total: 0,
            viewport_cols: 80,
            viewport_rows: 24,
            playback_speed: spec.playback_speed.clone(),
            current_action: String::new(),
            action_count: 0,
            paused: false,
            cleanup_acknowledged: true,
            root: root.to_string_lossy().into_owned(),
            runtime_dir: runtime.to_string_lossy().into_owned(),
            config_dir: sandbox.join("c").to_string_lossy().into_owned(),
            data_dir: sandbox.join("d").to_string_lossy().into_owned(),
            state_dir: sandbox.join("s").to_string_lossy().into_owned(),
            session_id: None,
            diagnostic: None,
        };
        let frozen = bmux_playbook::parser::freeze_sources_verified(
            &source,
            &allowed,
            &root.join("sources"),
            &spec.expected_closure_sha256,
        )
        .map_err(message)?;
        let deck = bmux_playbook::parser::parse_file_scoped(&frozen, Some(&root.join("sources")))
            .map_err(message)?;
        if !matches!(
            deck.config.driver,
            bmux_playbook::types::PlaybookDriver::Sandbox
        ) || deck.config.sandbox_config_file.is_some()
        {
            return Err(
                "managed execution requires sandbox driver and generated isolated configuration"
                    .into(),
            );
        }
        for key in deck.config.env.keys() {
            if key.starts_with("BMUX_") || key.starts_with("XDG_") {
                return Err(format!(
                    "managed deck cannot override runtime selector {key}"
                ));
            }
        }
        let mut snapshot = snapshot;
        snapshot.viewport_cols = deck.config.viewport.cols;
        snapshot.viewport_rows = deck.config.viewport.rows;
        snapshot.action_count = u64::try_from(deck.steps.len()).map_err(message)?;
        spec.source = frozen.to_string_lossy().into_owned();
        spec.allowed_root = root.join("sources").to_string_lossy().into_owned();
        let manifest = serde_json::to_vec(&RuntimeOwnership {
            generation: self.generation,
            attempt_root: snapshot.root.clone(),
            runtime_dir: snapshot.runtime_dir.clone(),
        })
        .map_err(message)?;
        fs::write(runtime.join("playbook-owner.json"), &manifest).map_err(message)?;
        fs::write(root.join("runtime.json"), &manifest).map_err(message)?;
        let application_environment = deck
            .config
            .application_environment(|name| std::env::var_os(name))
            .map_err(message)?
            .into_iter()
            .filter(|(name, _)| deck.config.env_from.contains(name))
            .map(|(name, value)| (name, value.into()))
            .collect();
        // Ownership now transfers to Run; failed/unacknowledged attempts retain
        // the path rather than letting a temporary guard delete a live socket.
        let _runtime = temporary.keep();
        let action_labels = deck.steps.iter().map(|step| step.action.name()).collect();
        let playback_speed = spec.playback_speed.clone();
        let run = Run {
            spec,
            application_environment,
            original_request,
            probe_deadline: None,
            snapshot,
            child: None,
            control: Control {
                generation: self.generation,
                playback_speed,
                ..Control::default()
            },
            last_snapshot: Vec::new(),
            action_labels,
            stop_requested: None,
        };
        run.write_control()?;
        let result = run.snapshot.clone();
        self.runs.insert(self.generation, run);
        Ok(result)
    }

    pub(super) fn start(&mut self, generation: u64) -> Result<Attempt> {
        let run = self.run(generation)?;
        if run.snapshot.phase != "prepared" || run.control.stop {
            return Err("start requires a prepared attempt".into());
        }
        // The private probe negotiates this exact worker protocol before any
        // deck is parsed or runtime launched. No PATH or daemon-default binary.
        let probe = Path::new(&run.snapshot.root).join("probe.json");
        let mut probe_command = run
            .command_with_source(&Path::new(&run.snapshot.root).join("protocol-probe-not-a-deck"));
        // An incompatible executable must encounter a missing input, never the
        // trusted deck: older versions do not know the probe environment flag.
        let child = probe_command
            .env("BMUX_PLAYBOOK_PROTOCOL_PROBE", &probe)
            .spawn()
            .map_err(message)?;
        run.child = Some(child);
        run.probe_deadline = Some(Instant::now() + Duration::from_secs(5));
        run.snapshot.cleanup_acknowledged = false;
        run.snapshot.phase = "probing".into();
        run.snapshot.revision += 1;
        Ok(run.snapshot.clone())
    }

    pub(super) fn lookup_preparation(&mut self, request_id: &str) -> Result<Attempt> {
        let run = self.runs.values_mut().find(|run| run.spec.request_id == request_id)
            .ok_or("preparation identity not found in this owner lifetime; absence does not prove an earlier owner never admitted it")?;
        run.refresh()?;
        Ok(run.snapshot.clone())
    }

    pub(super) fn status(&mut self, generation: u64) -> Result<Attempt> {
        let run = self.run(generation)?;
        run.refresh()?;
        Ok(run.snapshot.clone())
    }

    pub(super) fn watch(&mut self, generation: u64, after_revision: u64) -> Result<Attempt> {
        let snapshot = self.status(generation)?;
        if after_revision > snapshot.revision {
            return Err("watch cursor is ahead of authoritative revision".into());
        }
        Ok(snapshot)
    }

    pub(super) fn control(&mut self, generation: u64, command: Command) -> Result<Attempt> {
        let run = self.run(generation)?;
        run.refresh()?;
        if run.control.stop && !matches!(command, Command::Stop) {
            return Err("attempt is stopping; cancellation cannot be reversed".into());
        }
        match command {
            Command::Pause => run.control.paused = true,
            Command::Resume => run.control.paused = false,
            Command::Step => {
                run.control.paused = true;
                run.control.permits = run
                    .control
                    .permits
                    .checked_add(1)
                    .ok_or("step permits exhausted")?;
            }
            Command::Stop => {
                run.control.stop = true;
                if run.stop_requested.is_none() {
                    run.stop_requested = Some(Instant::now());
                }
                if run.child.is_some() {
                    run.snapshot.phase = "stopping".into();
                }
            }
        }
        run.write_control()?;
        run.snapshot.paused = run.control.paused;
        run.snapshot.revision += 1;
        Ok(run.snapshot.clone())
    }

    pub(super) fn set_playback_speed(&mut self, generation: u64, speed: &str) -> Result<Attempt> {
        let speed = bmux_playbook::PlaybackSpeed::parse(speed)
            .ok_or("invalid playback speed; expected instant, half, normal, double")?;
        let run = self.run(generation)?;
        run.refresh()?;
        if run.control.stop || (run.child.is_none() && run.snapshot.phase != "prepared") {
            return Err("attempt is stopped or settled".into());
        }
        run.control.playback_speed = speed.as_str().into();
        run.write_control()?;
        run.spec
            .playback_speed
            .clone_from(&run.control.playback_speed);
        run.snapshot
            .playback_speed
            .clone_from(&run.control.playback_speed);
        run.snapshot.revision += 1;
        Ok(run.snapshot.clone())
    }

    pub(super) fn restart(&mut self, generation: u64) -> Result<Attempt> {
        let run = self.run(generation)?;
        run.refresh()?;
        if run.child.is_some() || !run.snapshot.cleanup_acknowledged {
            return Err("restart requires runner exit AND sandbox cleanup acknowledgement".into());
        }
        run.cleanup_runtime()?;
        let mut spec = run.spec.clone();
        spec.request_id = uuid::Uuid::new_v4().to_string();
        let next = self.prepare(spec)?;
        self.start(next.generation)
    }

    fn run(&mut self, generation: u64) -> Result<&mut Run> {
        let run = self
            .runs
            .get_mut(&generation)
            .ok_or("unknown or stale attempt generation")?;
        if run.snapshot.generation != generation {
            return Err("stale attempt generation".into());
        }
        Ok(run)
    }
}

impl Run {
    fn cleanup_runtime(&self) -> Result<()> {
        let runtime = Path::new(&self.snapshot.runtime_dir);
        if !runtime.try_exists().map_err(message)? {
            return Ok(());
        }
        if fs::symlink_metadata(runtime)
            .map_err(message)?
            .file_type()
            .is_symlink()
        {
            return Err("managed runtime ownership changed to a symlink; cleanup refused".into());
        }
        let bytes = read_bounded(&runtime.join("playbook-owner.json"))?;
        let owner: RuntimeOwnership = serde_json::from_slice(&bytes).map_err(message)?;
        if owner.generation != self.snapshot.generation
            || owner.attempt_root != self.snapshot.root
            || owner.runtime_dir != self.snapshot.runtime_dir
        {
            return Err("managed runtime ownership mismatch; cleanup refused".into());
        }
        fs::remove_dir_all(runtime).map_err(message)?;
        if runtime.try_exists().map_err(message)? {
            return Err("managed runtime removal is unacknowledged".into());
        }
        Ok(())
    }

    fn spawn_runner(&self) -> std::io::Result<Child> {
        use std::io::Write;
        let mut diagnostics = File::create(Path::new(&self.snapshot.root).join("worker.stderr"))?;
        let mut child = self.command().stderr(Stdio::piped()).spawn()?;
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut remaining = 8192;
                let mut buffer = [0; 1024];
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            let retained = count.min(remaining);
                            let _ = diagnostics.write_all(&buffer[..retained]);
                            remaining -= retained;
                        }
                    }
                }
            });
        }
        Ok(child)
    }

    fn command(&self) -> ProcessCommand {
        let mut command = self.command_with_source(Path::new(&self.spec.source));
        command.envs(&self.application_environment);
        command
    }

    fn command_with_source(&self, source: &Path) -> ProcessCommand {
        let mut cmd = ProcessCommand::new(&self.spec.executable);
        cmd.env_clear();
        for key in ["PATH", "USER", "SHELL", "LANG", "TERM", "TMPDIR"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        let bootstrap = Path::new(&self.snapshot.root).join("worker");
        cmd.env("HOME", &self.snapshot.root);
        cmd.args(["playbook", "run"])
            .arg(source)
            .arg("--json")
            .env(
                "BMUX_PLAYBOOK_MANAGED_RUNTIME_DIR",
                &self.snapshot.runtime_dir,
            )
            .env("BMUX_PLAYBOOK_MANAGED_ROOT", &self.snapshot.root)
            .env(
                "BMUX_PLAYBOOK_MANAGED_GENERATION",
                self.snapshot.generation.to_string(),
            )
            .env("BMUX_PLAYBOOK_ALLOWED_ROOT", &self.spec.allowed_root)
            .env("BMUX_CONFIG_DIR", bootstrap.join("c"))
            .env("BMUX_RUNTIME_DIR", bootstrap.join("r"))
            .env("BMUX_DATA_DIR", bootstrap.join("d"))
            .env("BMUX_STATE_DIR", bootstrap.join("s"))
            .env("BMUX_LOG_DIR", bootstrap.join("logs"))
            .env("BMUX_NO_BASE_CONFIG", "1")
            .env_remove("BMUX_SLOT")
            .env_remove("BMUX_RUNTIME")
            .env_remove("BMUX_PLAYBOOK_PROTOCOL_PROBE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    fn write_control(&self) -> Result<()> {
        let root = Path::new(&self.snapshot.root);
        let bytes = serde_json::to_vec(&self.control).map_err(message)?;
        fs::write(root.join("control.pending"), bytes).map_err(message)?;
        fs::rename(root.join("control.pending"), root.join("control.json")).map_err(message)
    }

    fn refresh_probe(&mut self) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Err("probe ownership missing".into());
        };
        if let Some(status) = child.try_wait().map_err(message)? {
            self.child = None;
            self.probe_deadline = None;
            let probe = Path::new(&self.snapshot.root).join("probe.json");
            let compatible = status.success()
                && read_bounded(&probe).is_ok_and(|bytes| bytes == b"bmux-playbook-worker-v1");
            if compatible && !self.control.stop {
                self.snapshot.phase = "starting".into();
                match self.spawn_runner() {
                    Ok(child) => self.child = Some(child),
                    Err(error) => {
                        self.snapshot.phase = "failed".into();
                        self.snapshot.diagnostic = Some(error.to_string());
                        self.cleanup_runtime()?;
                        self.snapshot.cleanup_acknowledged = true;
                    }
                }
            } else {
                self.snapshot.phase = "failed".into();
                self.snapshot.diagnostic = Some(if self.control.stop {
                    "start cancelled before runtime launch".into()
                } else {
                    "selected executable does not support managed worker v1".into()
                });
                self.cleanup_runtime()?;
                self.snapshot.cleanup_acknowledged = true;
            }
            self.snapshot.revision += 1;
        } else if self.control.stop
            || self
                .probe_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            child.kill().map_err(message)?;
            // Do not wait under the owner mutex. A later poll must observe and
            // reap actual exit before cleanup can be acknowledged.
            self.control.stop = true;
            self.snapshot.phase = "stopping".into();
            self.snapshot.diagnostic = Some(
                "protocol probe cancelled or deadline exceeded; waiting for child exit".into(),
            );
            self.snapshot.revision += 1;
        }
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        if self.probe_deadline.is_some() {
            return self.refresh_probe();
        }
        let path = Path::new(&self.snapshot.root).join("runner.json");
        if path.exists() {
            let bytes = read_bounded(&path)?;
            if bytes != self.last_snapshot {
                let observed: RunnerSnapshot = serde_json::from_slice(&bytes).map_err(message)?;
                if observed.generation != self.snapshot.generation {
                    return Err("worker snapshot has stale generation".into());
                }
                self.snapshot.phase = observed.phase;
                self.snapshot.step = observed.step;
                if observed.typing_done > observed.typing_total {
                    return Err("invalid typing progress".into());
                }
                self.snapshot.typing_done = observed.typing_done;
                self.snapshot.typing_total = observed.typing_total;
                let expected_label = usize::try_from(observed.step)
                    .ok()
                    .and_then(|step| self.action_labels.get(step))
                    .copied()
                    .unwrap_or("");
                // Never project arbitrary worker text as an action label.
                self.snapshot.current_action = if observed.current_action == expected_label {
                    expected_label.into()
                } else {
                    String::new()
                };
                if observed.action_count != 0 && observed.action_count != self.snapshot.action_count
                {
                    return Err("worker action count differs from prepared deck".into());
                }
                self.snapshot.session_id = observed.session_id;
                self.snapshot.diagnostic = observed.diagnostic;
                // Cleanup ack is published by the runner only after child reaping.
                // It is not authoritative until the runner itself exits as well.
                self.snapshot.cleanup_acknowledged =
                    observed.cleanup_acknowledged && self.child.is_none();
                self.last_snapshot = bytes;
                self.snapshot.revision += 1;
            }
        }
        if let Some(child) = self.child.as_mut() {
            if let Some(status) = child.try_wait().map_err(message)? {
                self.child = None;
                let acknowledged = if self.last_snapshot.is_empty() {
                    false
                } else {
                    serde_json::from_slice::<RunnerSnapshot>(&self.last_snapshot)
                        .map_err(message)?
                        .cleanup_acknowledged
                };
                self.snapshot.cleanup_acknowledged = acknowledged;
                if acknowledged && let Err(error) = self.cleanup_runtime() {
                    self.snapshot.cleanup_acknowledged = false;
                    self.snapshot.phase = "cleanup-failed".into();
                    self.snapshot.diagnostic = Some(error);
                }
                if !acknowledged {
                    self.snapshot.phase = "cleanup-failed".into();
                    let diagnostic_path = Path::new(&self.snapshot.root).join("worker.stderr");
                    let excerpt = read_bounded(&diagnostic_path).unwrap_or_default();
                    let excerpt = String::from_utf8_lossy(&excerpt)
                        .chars()
                        .take(2048)
                        .collect::<String>();
                    self.snapshot.diagnostic = Some(format!(
                        "runner exited ({status}) without sandbox cleanup acknowledgement; diagnostics retained: {excerpt}"
                    ));
                }
                self.snapshot.revision += 1;
            } else if self
                .stop_requested
                .is_some_and(|at| at.elapsed() >= STOP_DEADLINE)
            {
                child.kill().map_err(message)?;
                child.wait().map_err(message)?;
                self.child = None;
                self.snapshot.phase = "cleanup-failed".into();
                self.snapshot.cleanup_acknowledged = false;
                self.snapshot.diagnostic = Some("emergency runner termination is NOT sandbox cleanup acknowledgement; restart blocked".into());
                self.snapshot.revision += 1;
            }
        }
        Ok(())
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.control.stop = true;
        let _ = self.write_control();
        if let Some(child) = self.child.as_mut() {
            let until = Instant::now() + STOP_DEADLINE;
            while Instant::now() < until {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
            // Never remove the attempt or claim cleanup after emergency kill.
        } else if self.snapshot.cleanup_acknowledged {
            let _ = self.cleanup_runtime();
        }
    }
}

fn canonical(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("paths must be explicit and absolute".into());
    }
    path.canonicalize().map_err(message)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(message)?;
    let mut bytes = Vec::new();
    file.take(MAX_CHANNEL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(message)?;
    if bytes.len() as u64 > MAX_CHANNEL_BYTES {
        return Err("worker channel exceeds size limit".into());
    }
    Ok(bytes)
}

fn message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_environment_is_explicit_bounded_and_not_a_runtime_selector() {
        let mut config = bmux_playbook::types::PlaybookConfig::default();
        config.env_from.insert("APP_TOKEN".into());
        let selected = config
            .application_environment(|name| (name == "APP_TOKEN").then(|| "private".into()))
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected["APP_TOKEN"], "private");
        for name in [
            "BMUX_CONFIG",
            "XDG_STATE_HOME",
            "HOME",
            "PATH",
            "LD_PRELOAD",
            "PYTHONPATH",
            "lowercase",
            "9INVALID",
        ] {
            config.env_from = [name.to_owned()].into();
            assert!(config.application_environment(|_| None).is_err());
        }
        config.env_from = ["BIG".to_owned()].into();
        assert!(
            config
                .application_environment(|_| Some("x".repeat(65537).into()))
                .is_err()
        );
    }

    #[test]
    fn application_inputs_reach_only_the_runner_and_never_serialized_evidence() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("deck.playbook"), "new-session\n").unwrap();
        let mut owner = Owner::default();
        let attempt = owner.prepare(specification(root.path())).unwrap();
        let run = owner.runs.get_mut(&attempt.generation).unwrap();
        run.application_environment
            .insert("APP_TOKEN".into(), "private-test-value".into());
        let command = run.command();
        assert!(command.get_envs().any(|(key, value)| key == "APP_TOKEN"
            && value == Some(std::ffi::OsStr::new("private-test-value"))));
        let probe = run.command_with_source(Path::new("missing-probe"));
        assert!(!probe.get_envs().any(|(key, _)| key == "APP_TOKEN"));
        for bytes in [
            serde_json::to_vec(&run.spec).unwrap(),
            serde_json::to_vec(&run.snapshot).unwrap(),
            run.original_request.clone(),
        ] {
            assert!(
                !String::from_utf8(bytes)
                    .unwrap()
                    .contains("private-test-value")
            );
        }
        run.cleanup_runtime().unwrap();
    }

    fn specification(root: &Path) -> Preparation {
        Preparation {
            request_id: uuid::Uuid::new_v4().to_string(),
            source: root.join("deck.playbook").to_string_lossy().into_owned(),
            allowed_root: root.to_string_lossy().into_owned(),
            executable: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            attempts_root: root.to_string_lossy().into_owned(),
            trusted_executable_input: true,
            playback_speed: "instant".into(),
            expected_closure_sha256: bmux_playbook::parser::source_closure_sha256(
                &root.join("deck.playbook"),
                root,
            )
            .unwrap(),
        }
    }

    #[test]
    fn preparation_identity_recovers_original_attempt_without_rereading_changed_source() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("deck.playbook"),
            "new-session name='approved'\n",
        )
        .unwrap();
        let mut owner = Owner::default();
        let request = specification(root.path());
        let admitted = owner.prepare(request.clone()).unwrap();
        fs::write(
            root.path().join("deck.playbook"),
            "new-session name='changed'\n",
        )
        .unwrap();
        let recovered = owner.prepare(request.clone()).unwrap();
        assert_eq!(recovered.generation, admitted.generation);
        assert_eq!(
            owner
                .lookup_preparation(&request.request_id)
                .unwrap()
                .generation,
            admitted.generation
        );
        let mut conflicting = request;
        conflicting.expected_closure_sha256 = "different".into();
        assert!(owner.prepare(conflicting).is_err());
        assert_eq!(owner.runs.len(), 1);
    }

    #[test]
    fn independent_attempts_do_not_replace_each_other_and_sources_are_pinned() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("deck.playbook"),
            "new-session name='one'\n",
        )
        .unwrap();
        let mut owner = Owner::default();
        let first = owner.prepare(specification(root.path())).unwrap();
        fs::write(
            root.path().join("deck.playbook"),
            "new-session name='two'\n",
        )
        .unwrap();
        let second = owner.prepare(specification(root.path())).unwrap();
        assert_ne!(first.generation, second.generation);
        assert_ne!(first.root, second.root);
        assert_eq!(owner.status(first.generation).unwrap().phase, "prepared");
        let command = owner.runs[&first.generation].command();
        let worker_state = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("BMUX_STATE_DIR"))
            .and_then(|(_, value)| value)
            .unwrap();
        assert_eq!(
            Path::new(worker_state),
            Path::new(&first.root).join("worker/s")
        );
        assert_ne!(Path::new(worker_state), Path::new(&first.state_dir));
        let frozen = &owner.runs[&first.generation].spec.source;
        assert!(fs::read_to_string(frozen).unwrap().contains("one"));
        assert!(owner.status(0).is_err());
    }

    #[test]
    fn deep_attempt_roots_use_a_separately_owned_short_runtime() {
        let temporary = tempfile::tempdir().unwrap();
        let deep = temporary.path().join("retained-state-root-".repeat(10));
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("deck.playbook"), "new-session name='one'\n").unwrap();
        let mut owner = Owner::default();
        let attempt = owner.prepare(specification(&deep)).unwrap();
        let runtime = Path::new(&attempt.runtime_dir);
        assert!(!runtime.starts_with(&deep));
        assert!(
            runtime
                .join("server.sock")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                < 104
        );
        let manifest: RuntimeOwnership = serde_json::from_slice(
            &fs::read(Path::new(&attempt.root).join("runtime.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.runtime_dir, attempt.runtime_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(runtime).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        owner.runs[&attempt.generation].cleanup_runtime().unwrap();
        assert!(!runtime.exists());
    }

    #[test]
    fn playback_speed_is_validated_fenced_retained_and_cannot_revive_stop() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("deck.playbook"), "new-session\n").unwrap();
        let mut owner = Owner::default();
        let mut spec = specification(root.path());
        spec.playback_speed = "fast".into();
        assert!(owner.prepare(spec).is_err());
        let attempt = owner.prepare(specification(root.path())).unwrap();
        assert_eq!(attempt.playback_speed, "instant");
        assert_eq!(attempt.action_count, 1);
        assert!(owner.set_playback_speed(0, "normal").is_err());
        assert!(
            owner
                .set_playback_speed(attempt.generation, "fast")
                .is_err()
        );
        for speed in ["half", "normal", "double", "instant"] {
            let updated = owner.set_playback_speed(attempt.generation, speed).unwrap();
            assert_eq!(updated.playback_speed, speed);
            let control: serde_json::Value = serde_json::from_slice(
                &fs::read(Path::new(&attempt.root).join("control.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(control["playback_speed"], speed);
            assert_eq!(control["generation"], attempt.generation);
        }
        owner.control(attempt.generation, Command::Stop).unwrap();
        assert!(
            owner
                .set_playback_speed(attempt.generation, "normal")
                .is_err()
        );
        owner.runs[&attempt.generation].cleanup_runtime().unwrap();
    }

    #[test]
    fn restart_cannot_bypass_missing_cleanup_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("deck.playbook"),
            "new-session name='one'\n",
        )
        .unwrap();
        let mut owner = Owner::default();
        let attempt = owner.prepare(specification(root.path())).unwrap();
        owner
            .runs
            .get_mut(&attempt.generation)
            .unwrap()
            .snapshot
            .cleanup_acknowledged = false;
        assert!(owner.restart(attempt.generation).is_err());
        let stopped = owner.control(attempt.generation, Command::Stop).unwrap();
        assert!(!stopped.cleanup_acknowledged);
        assert!(owner.control(attempt.generation, Command::Resume).is_err());
    }
}
