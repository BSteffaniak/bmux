//! Server-process controller. One serialized relay worker; one pending full repair.
use crate::relay::Relay;
use bmux_broadcast_plugin_api::{broadcast_control_v1::Status, broadcast_output_v1::SourceState};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use uuid::Uuid;

const SOURCE_LEASE: Duration = Duration::from_secs(6);
const MAX_SOURCES: usize = 32;
struct Source {
    owner: Uuid,
    expires: Instant,
    last: Option<(u64, String, u64)>,
    terminal_stop: Option<tokio::sync::watch::Sender<bool>>,
}
struct Pending {
    refresh: u64,
    bytes: Vec<u8>,
    relay_revision: Option<u64>,
}
struct State {
    sources: BTreeMap<String, Source>,
    selected: Option<String>,
    refresh: u64,
    local: &'static str,
    remote: &'static str,
    error: Option<&'static str>,
    pending: Option<Pending>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            sources: BTreeMap::new(),
            selected: None,
            refresh: 1,
            local: "stopped",
            remote: "offline",
            error: None,
            pending: None,
        }
    }
}
impl State {
    fn status(&self) -> Status {
        Status {
            local: self.local.into(),
            remote: self.remote.into(),
            source_id: self.selected.clone(),
            error_code: self.error.map(str::to_owned),
        }
    }
    fn invalidate(&mut self) {
        self.refresh = self.refresh.saturating_add(1);
        self.pending = None;
    }
    fn available(&self) -> bool {
        self.selected
            .as_ref()
            .and_then(|id| self.sources.get(id))
            .is_some_and(|s| s.expires > Instant::now())
    }
    fn source_state(&self, id: &str) -> SourceState {
        SourceState {
            refresh: self.refresh,
            selected: self.local == "running"
                && self.selected.as_deref() == Some(id)
                && self.available(),
        }
    }
}
pub enum Control {
    Start(Relay, String),
    Pause,
    Resume,
    Stop,
    Source(String),
    Shutdown,
}
struct Request {
    control: Control,
    reply: std::sync::mpsc::SyncSender<Status>,
}
pub struct Controller {
    state: Arc<Mutex<State>>,
    commands: mpsc::Sender<Request>,
    gate: tokio::sync::watch::Sender<u64>,
    operation: Mutex<()>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    terminals: Mutex<Vec<std::thread::JoinHandle<()>>>,
}
impl Controller {
    pub fn new() -> Result<Self, &'static str> {
        let state = Arc::new(Mutex::new(State::default()));
        let (commands, receiver) = mpsc::channel(8);
        let worker_state = state.clone();
        let (gate, worker_gate) = tokio::sync::watch::channel(1);
        let worker = std::thread::Builder::new()
            .name("bmux-broadcast".into())
            .spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(worker(worker_state, receiver, worker_gate));
                }
            })
            .map_err(|_| "executor_unavailable")?;
        Ok(Self {
            state,
            commands,
            gate,
            operation: Mutex::new(()),
            worker: Mutex::new(Some(worker)),
            terminals: Mutex::new(Vec::new()),
        })
    }
    pub fn status(&self) -> Result<Status, &'static str> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "controller_unavailable")?
            .status())
    }
    pub fn control(&self, control: Control) -> Result<Status, &'static str> {
        let _operation = self
            .operation
            .lock()
            .map_err(|_| "controller_unavailable")?;
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        // Admission and the local gate transition are atomic. A full command queue
        // rejects the action without changing local state.
        let mut state = self.state.lock().map_err(|_| "controller_unavailable")?;
        let permit = self.commands.try_reserve().map_err(|_| "controller_busy")?;
        match &control {
            Control::Start(_, id) => {
                if state.local != "stopped"
                    || state
                        .sources
                        .get(id)
                        .is_none_or(|s| s.expires <= Instant::now())
                {
                    return Err("source_unavailable_or_already_started");
                }
                state.selected = Some(id.clone());
                state.local = "starting";
            }
            Control::Pause => {
                if state.local != "running" && state.local != "recovering" {
                    return Err("not_running");
                }
                state.local = "paused";
            }
            Control::Resume => {
                if state.local != "paused"
                    && state.local != "unavailable"
                    && state.local != "recovering"
                {
                    return Err("not_paused");
                }
                if !state.available() {
                    return Err("source_unavailable");
                }
                state.local = "starting";
            }
            Control::Stop | Control::Shutdown => {
                state.local = "stopped";
                for source in state.sources.values() {
                    if let Some(stop) = &source.terminal_stop {
                        stop.send_replace(true);
                    }
                }
            }
            Control::Source(id) => {
                if state
                    .sources
                    .get(id)
                    .is_none_or(|s| s.expires <= Instant::now())
                {
                    return Err("source_unavailable");
                }
                if state.local == "stopped" {
                    return Err("not_started");
                }
                state.selected = Some(id.clone());
                if state.local != "paused" {
                    state.local = "starting";
                }
            }
        }
        state.invalidate();
        self.gate.send_replace(state.refresh);
        permit.send(Request { control, reply });
        drop(state);
        response
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "controller_timeout")
    }
    pub fn source(
        &self,
        owner: Uuid,
        id: &str,
        register: bool,
    ) -> Result<SourceState, &'static str> {
        Uuid::parse_str(id).map_err(|_| "invalid_source")?;
        let mut state = self.state.lock().map_err(|_| "controller_unavailable")?;
        let now = Instant::now();
        if let Some(source) = state.sources.get(id) {
            if source.expires > now && source.owner != owner {
                return Err("source_not_owned");
            }
            if source.expires <= now && !register {
                return Err("source_expired");
            }
        } else if !register {
            return Err("source_not_registered");
        }
        let expired = state
            .sources
            .get(id)
            .is_some_and(|source| source.expires <= now);
        if expired {
            if let Some(stop) = state
                .sources
                .remove(id)
                .and_then(|source| source.terminal_stop)
            {
                stop.send_replace(true);
            }
            if state.selected.as_deref() == Some(id) && state.local != "stopped" {
                state.invalidate();
                state.local = "unavailable";
                self.gate.send_replace(state.refresh);
            }
        }
        if let Some(source) = state.sources.get_mut(id) {
            source.expires = now + SOURCE_LEASE;
        } else {
            state.sources.retain(|_, source| source.expires > now);
            if state.sources.len() >= MAX_SOURCES {
                return Err("source_limit");
            }
            state.sources.insert(
                id.into(),
                Source {
                    owner,
                    expires: now + SOURCE_LEASE,
                    last: None,
                    terminal_stop: None,
                },
            );
        }
        Ok(state.source_state(id))
    }
    pub fn unregister(&self, owner: Uuid, id: &str) -> Result<bool, &'static str> {
        let mut state = self.state.lock().map_err(|_| "controller_unavailable")?;
        if state.sources.get(id).is_none_or(|s| s.owner != owner) {
            return Err("source_not_owned");
        }
        if let Some(stop) = state
            .sources
            .remove(id)
            .and_then(|source| source.terminal_stop)
        {
            stop.send_replace(true);
        }
        if state.selected.as_deref() == Some(id) && state.local != "stopped" {
            state.invalidate();
            state.local = "unavailable";
            self.gate.send_replace(state.refresh);
        }
        Ok(true)
    }
    pub fn terminal(
        self: &Arc<Self>,
        owner: Uuid,
        id: String,
        selection: crate::terminal::RuntimeSelection,
        target: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<Status, &'static str> {
        let _operation = self
            .operation
            .lock()
            .map_err(|_| "controller_unavailable")?;
        // Verify the explicit executable before registering any source authority.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "executor_unavailable")?;
        runtime.block_on(selection.verify())?;
        self.source(owner, &id, true)?;
        let (stop, stopped) = tokio::sync::watch::channel(false);
        {
            let mut state = self.state.lock().map_err(|_| "controller_unavailable")?;
            let source = state.sources.get_mut(&id).ok_or("source_not_registered")?;
            if source.terminal_stop.is_some() {
                return Err("terminal_already_registered");
            }
            source.terminal_stop = Some(stop);
        }
        let controller = self.clone();
        let handle = std::thread::Builder::new()
            .name("bmux-broadcast-terminal".into())
            .spawn(move || {
                runtime.block_on(async {
                    let (frames, mut latest) = tokio::sync::watch::channel(None);
                    let observation =
                        crate::terminal::observe(selection, target, cols, rows, frames, stopped);
                    tokio::pin!(observation);
                    let mut tick = tokio::time::interval(Duration::from_secs(1));
                    let mut sent = None;
                    loop {
                        tokio::select! {
                            _ = &mut observation => break,
                            _ = tick.tick() => {},
                            result = latest.changed() => { if result.is_err() { break; } },
                        }
                        let Ok(source) = controller.source(owner, &id, false) else {
                            break;
                        };
                        if !source.selected {
                            continue;
                        }
                        let bytes: Option<Arc<[u8]>> = latest.borrow().clone();
                        if let Some(bytes) = bytes {
                            let identity = (source.refresh, Arc::as_ptr(&bytes));
                            if sent != Some(identity)
                                && controller
                                    .publish(owner, &id, source.refresh, bytes.to_vec())
                                    .is_ok_and(|accepted| accepted)
                            {
                                sent = Some(identity);
                            }
                        }
                    }
                    let _ = controller.unregister(owner, &id);
                });
            })
            .map_err(|_| "executor_unavailable")?;
        let mut terminals = self.terminals.lock().map_err(|_| "executor_unavailable")?;
        terminals.retain(|handle| !handle.is_finished());
        terminals.push(handle);
        self.status()
    }

    pub fn shutdown(&self) {
        let _ = self.control(Control::Shutdown);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
        if let Ok(mut terminals) = self.terminals.lock() {
            for terminal in terminals.drain(..) {
                let _ = terminal.join();
            }
        }
    }

    pub fn publish(
        &self,
        owner: Uuid,
        id: &str,
        refresh: u64,
        bytes: Vec<u8>,
    ) -> Result<bool, &'static str> {
        if bytes.is_empty() || bytes.len() > crate::relay::MAX_FRAME_BYTES {
            return Err("frame_bound");
        }
        // Validation happens off the render path, before touching pending authority.
        let frame = bmux_web_renderer::decode_binary(&bytes, None).map_err(|_| "invalid_frame")?;
        if frame.revision() == 0 {
            return Err("invalid_frame");
        }
        let mut state = self.state.lock().map_err(|_| "controller_unavailable")?;
        if !state.source_state(id).selected || state.refresh != refresh {
            return Ok(false);
        }
        let source = state.sources.get_mut(id).ok_or("source_not_registered")?;
        if source.owner != owner {
            return Err("source_not_owned");
        }
        if source
            .last
            .as_ref()
            .is_some_and(|(_, generation, _)| generation != frame.generation())
        {
            source.last = None;
            state.invalidate();
            self.gate.send_replace(state.refresh);
            return Ok(false);
        }
        if source
            .last
            .as_ref()
            .is_some_and(|(old_refresh, _generation, revision)| {
                *old_refresh == refresh && *revision >= frame.revision()
            })
        {
            return Err("stale_frame");
        }
        source.last = Some((refresh, frame.generation().into(), frame.revision()));
        state.pending = Some(Pending {
            refresh,
            bytes,
            relay_revision: None,
        });
        Ok(true)
    }
}

async fn worker(
    state: Arc<Mutex<State>>,
    mut commands: mpsc::Receiver<Request>,
    mut gate: tokio::sync::watch::Receiver<u64>,
) {
    let mut relay: Option<Relay> = None;
    let mut revision = 0_u64;
    let mut heartbeat = Instant::now();
    let mut source_paused = false;
    let mut retry_after = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            request = commands.recv() => {
                let Some(request) = request else { break; };
                let shutdown = matches!(&request.control, Control::Shutdown);
                let result = match request.control {
                    Control::Start(mut next, _) => { let result = next.start().await; relay = Some(next); if result.is_ok() { revision = 0; source_paused = false; } result }
                    Control::Pause => match &relay { Some(relay) => relay.mutation("pause", None).await, None => Err("not_started") },
                    Control::Stop | Control::Shutdown => match &relay { Some(relay) => relay.mutation("stop", None).await, None => Ok(()) },
                    Control::Resume | Control::Source(_) => match &relay {
                        Some(relay) => {
                            let remain_paused = state.lock().is_ok_and(|state| state.local == "paused");
                            let paused = relay.mutation("pause", None).await;
                            if paused.is_ok() && !remain_paused { relay.mutation("resume", None).await } else { paused }
                        },
                        None => Err("not_started")
                    },
                };
                if let Ok(mut state) = state.lock() {
                    state.error = result.err();
                    match state.local {
                        "starting" if result.is_ok() => { state.local = "running"; state.remote = "starting"; source_paused = false; }
                        "starting" => { state.local = "unavailable"; state.remote = "unknown"; }
                        "paused" => state.remote = if result.is_ok() { "paused" } else { "unknown" },
                        "stopped" => { state.remote = if result.is_ok() { "offline" } else { "stop_unacknowledged" }; if result.is_ok() { relay = None; } }
                        _ => {}
                    }
                    let _ = request.reply.send(state.status());
                }
                if shutdown { break; }
                gate.borrow_and_update();
                heartbeat = Instant::now();
            }
            _ = tick.tick() => {
                let (pending, should_pause) = {
                    let Ok(mut state) = state.lock() else { break; };
                    let expired = matches!(state.local, "running" | "recovering") && !state.available();
                    if expired { state.invalidate(); state.local = "unavailable"; state.error = Some("source_expired"); }
                    let pause = state.local == "unavailable" && !source_paused;
                    gate.borrow_and_update();
                    let pending = if state.local == "running" && Instant::now() >= retry_after { state.pending.take().filter(|p| p.refresh == state.refresh) } else { None };
                    (pending, pause)
                };
                let Some(remote) = relay.as_ref() else { continue; };
                if should_pause {
                    let result = remote.mutation("pause", None).await;
                    if let Ok(mut state) = state.lock() { state.remote = if result.is_ok() { "paused" } else { "unknown" }; }
                    source_paused = result.is_ok();
                }
                if let Some(pending) = pending {
                    let Some(next) = pending.relay_revision.or_else(|| revision.checked_add(1)) else { continue; };
                    let lease_guard = async {
                        loop {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            let Ok(state) = state.lock() else { return "controller_unavailable"; };
                            if state.refresh != pending.refresh || state.local != "running" { return "upload_invalidated"; }
                            if !state.available() { return "source_expired"; }
                        }
                    };
                    let result = tokio::select! {
                        result = remote.mutation("frame", Some((next, &pending.bytes))) => result,
                        _ = gate.changed() => Err("upload_invalidated"),
                        code = lease_guard => Err(code),
                    };
                    revision = next;
                    if let Ok(mut state) = state.lock()
                        && state.refresh == pending.refresh && state.local == "running" {
                            state.error = result.err();
                            state.remote = if result.is_ok() { "live" } else { "unknown" };
                            if matches!(result, Err("relay_unreachable" | "relay_transient")) {
                                retry_after = Instant::now() + Duration::from_secs(1);
                                if state.pending.is_none() { state.pending = Some(Pending { refresh: pending.refresh, bytes: pending.bytes, relay_revision: Some(next) }); }
                            }
                            if result == Err("relay_epoch_lost") { state.invalidate(); state.local = "recovering"; retry_after = Instant::now() + Duration::from_secs(1); }
                            if result == Err("source_expired") { state.invalidate(); state.local = "unavailable"; }
                    }
                }
                if heartbeat.elapsed() >= Duration::from_secs(3) {
                    let operation = match state.lock().ok().map(|s| (s.local, s.remote)) {
                        Some(("stopped", "stop_unacknowledged")) => "stop",
                        Some(("stopped", _)) => continue,
                        Some(("paused" | "unavailable", remote)) if remote != "paused" => "pause",
                        _ => "heartbeat",
                    };
                    let result = remote.mutation(operation, None).await;
                    if let Ok(mut state) = state.lock() {
                        if let Err(code) = result {
                            state.error = Some(code);
                            if state.local != "stopped" { state.remote = "unknown"; }
                            if code == "relay_epoch_lost" && state.local == "running" { state.invalidate(); state.local = "recovering"; retry_after = Instant::now() + Duration::from_secs(1); }
                        }
                        else if operation == "stop" { state.remote = "offline"; state.error = None; }
                        else if operation == "pause" { state.remote = "paused"; }
                    }
                    heartbeat = Instant::now();
                }
                let recover = state.lock().is_ok_and(|state| state.local == "recovering" && state.available()) && Instant::now() >= retry_after;
                if recover {
                    let Some(remote) = relay.as_mut() else { continue; };
                    gate.borrow_and_update();
                    let result = tokio::select! {
                        result = remote.start() => result,
                        _ = gate.changed() => Err("upload_invalidated"),
                    };
                    retry_after = Instant::now() + Duration::from_secs(1);
                    if let Ok(mut state) = state.lock()
                        && state.local == "recovering" && state.available() {
                            if result.is_ok() { state.invalidate(); state.local = "running"; state.remote = "starting"; state.error = None; revision = 0; source_paused = false; }
                            else { state.error = result.err(); }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "controller_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_ownership_refresh_and_bounds() {
        let controller = Controller::new().unwrap();
        let owner = Uuid::new_v4();
        let id = Uuid::new_v4().to_string();
        assert!(!controller.source(owner, &id, true).unwrap().selected);
        assert!(controller.source(Uuid::new_v4(), &id, false).is_err());
        {
            let mut state = controller.state.lock().unwrap();
            state.selected = Some(id.clone());
            state.local = "running";
        }
        let before = controller.source(owner, &id, false).unwrap();
        assert!(before.selected);
        {
            let mut state = controller.state.lock().unwrap();
            state.invalidate();
            state.local = "paused";
        }
        let after = controller.source(owner, &id, false).unwrap();
        assert!(after.refresh > before.refresh);
        assert!(!after.selected);
        assert!(
            controller
                .publish(owner, &id, before.refresh, vec![0; 8 * 1024 * 1024 + 1])
                .is_err()
        );
        assert!(controller.unregister(owner, &id).unwrap());
        assert!(controller.source(owner, &id, false).is_err());
        controller.shutdown();
    }
}
