//! Shell-scope lifecycle on the per-pane control socket.

use super::{CommandPhase, Entry, State, save};
use crate::execution::Execution;
use crate::process::{self, Identity};
use crate::recovery::{self, Command};
use serde::Deserialize;
use uuid::Uuid;

pub(super) const PROTOCOL: &str = "bmux-shell-invocation/1";

pub(super) struct NestedShell {
    identity: Identity,
    parent: Uuid,
    shell: String,
    pub execution: Option<Execution>,
    replay: bool,
    offered: bool,
    pub claim: Option<Identity>,
}

impl NestedShell {
    pub(super) fn owns_replay(&self, identity: Identity, invocation: Uuid) -> bool {
        self.identity == identity
            && self.replay
            && self.offered
            && self
                .execution
                .as_ref()
                .is_some_and(|execution| execution.committed.invocation == invocation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Boundary {
    pub protocol: String,
    operation: String,
    shell: String,
    shell_pid: u32,
    text: String,
    cwd: String,
}

pub(super) fn handle(
    entry: &Entry,
    peer: u32,
    boundary: Boundary,
) -> Result<Option<String>, String> {
    if boundary.protocol != PROTOCOL
        || !matches!(boundary.operation.as_str(), "prompt" | "start" | "location")
    {
        return Err("invalid shell lifecycle operation".into());
    }
    let callback = process::inspect(peer)?;
    let shell = process::ancestor(peer, boundary.shell_pid)?;
    let executable = process::executable(shell.identity.pid)?;
    let declared = std::fs::canonicalize(&boundary.shell).map_err(|error| error.to_string())?;
    if std::fs::canonicalize(executable).map_err(|error| error.to_string())? != declared
        || !process::current(shell.identity)
    {
        return Err("shell executable identity mismatch".into());
    }
    let foreground = u32::try_from(callback.foreground)
        .ok()
        .and_then(|pid| process::ancestor(pid, shell.identity.pid).ok());
    if callback.session != shell.session
        || callback.terminal != shell.terminal
        || callback.group <= 0
        || (callback.group != callback.foreground && foreground.is_none())
    {
        return Err("callback does not belong to shell terminal".into());
    }
    advance(entry, shell, boundary)
}

fn advance(
    entry: &Entry,
    shell: process::Process,
    boundary: Boundary,
) -> Result<Option<String>, String> {
    let mut state = entry.state.lock().map_err(|error| error.to_string())?;
    if state.identity == Some(shell.identity)
        && state.executable.as_deref() == Some(std::path::Path::new(&boundary.shell))
    {
        // The pane owner's existing authenticated OSC hooks own root boundaries.
        return Ok(None);
    }
    if entry.stopped.load(std::sync::atomic::Ordering::SeqCst)
        || !state.shell
        || state.command_phase != CommandPhase::Active
        || !state.identity.is_some_and(process::current)
        || !process::current(shell.identity)
    {
        return Err("shell execution unavailable".into());
    }
    verify_pane(&state, shell)?;
    // Dead shells cannot retain claims on a successor process with a recycled PID.
    state
        .nested
        .retain(|scope| process::current(scope.identity));
    let index = register(&mut state, shell, &boundary)?;
    persist_generation(entry, &mut state, index)?;
    if state.nested[index].shell != boundary.shell {
        return Err("shell scope executable changed".into());
    }
    if boundary.operation == "location" {
        if state.nested[index].replay {
            let parent = state.nested[index].parent;
            let command = recovery::find(&state.execution.committed, parent)
                .and_then(|checkpoint| checkpoint.commands.first())
                .ok_or("missing nested replay command")?;
            return Ok(Some(command.cwd.clone()));
        }
        return Ok(None);
    }
    if boundary.operation == "prompt" && state.nested[index].replay {
        // Offer the host-recorded command once at the recreated shell's first
        // prompt. Keep the descriptor until a durable application update.
        let parent = state.nested[index].parent;
        let command = recovery::find(&state.execution.committed, parent)
            .and_then(|checkpoint| checkpoint.commands.first())
            .ok_or("missing nested replay command")?;
        let text = command.text.clone();
        if state.pending_replay.is_some() || state.nested[index].offered {
            return Err("nested replay already offered".into());
        }
        let invocation = state.nested[index]
            .execution
            .as_ref()
            .ok_or("missing replay execution")?
            .committed
            .invocation;
        state.pending_replay = Some((shell.identity, invocation, text));
        state.nested[index].offered = true;
        return Ok(Some(String::new()));
    }
    if boundary.operation == "start" && state.nested[index].replay {
        let parent = state.nested[index].parent;
        let expected = recovery::find(&state.execution.committed, parent)
            .and_then(|checkpoint| checkpoint.commands.first())
            .ok_or("missing nested replay command")?;
        if expected.text != boundary.text || expected.cwd != boundary.cwd {
            return Err("nested replay command or working directory changed".into());
        }
        state.nested[index].replay = false;
        return Ok(None);
    }
    let parent = state.nested[index].parent;
    let execution = (boundary.operation == "start").then(Execution::manual);
    let mut checkpoint = state.execution.committed.clone();
    let parent_checkpoint =
        recovery::find_mut(&mut checkpoint, parent).ok_or("stale parent invocation")?;
    parent_checkpoint.version = 2;
    parent_checkpoint.commands.clear();
    if let Some(execution) = &execution {
        parent_checkpoint.commands.push(Command {
            shell: boundary.shell,
            text: boundary.text,
            cwd: boundary.cwd,
            checkpoint: Box::new(execution.committed.clone()),
        });
    }
    checkpoint.version = 2;
    checkpoint.validate().map_err(str::to_owned)?;
    if let Err(error) = save(&entry.file, &checkpoint) {
        entry
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        return Err(error);
    }
    state.install_checkpoint(checkpoint);
    state.pending_replay = None;
    state.nested.truncate(index + 1);
    state.nested[index].execution = execution;
    state.nested[index].claim = None;
    state.epoch = state
        .epoch
        .checked_add(1)
        .ok_or("execution epoch exhausted")?;
    Ok(None)
}

fn verify_pane(state: &State, shell: process::Process) -> Result<(), String> {
    let root = state.identity.ok_or("pane process identity unavailable")?;
    let root_process = process::inspect(root.pid)?;
    if root_process.identity != root
        || root_process.session != shell.session
        || root_process.terminal != shell.terminal
    {
        return Err("shell belongs to another pane terminal or session".into());
    }
    Ok(())
}

fn persist_generation(entry: &Entry, state: &mut State, index: usize) -> Result<(), String> {
    if let Some(execution) = &state.nested[index].execution {
        let mut durable = state.execution.committed.clone();
        let invocation = execution.committed.invocation;
        let saved =
            recovery::find_mut(&mut durable, invocation).ok_or("stale nested invocation")?;
        if saved.generation != execution.committed.generation {
            *saved = execution.committed.clone();
            if let Err(error) = save(&entry.file, &durable) {
                entry
                    .stopped
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Err(error);
            }
            state.install_checkpoint(durable);
        }
    }
    Ok(())
}

fn register(
    state: &mut State,
    shell: process::Process,
    boundary: &Boundary,
) -> Result<usize, String> {
    let index = if let Some(index) = state
        .nested
        .iter()
        .position(|scope| scope.identity == shell.identity && scope.shell == boundary.shell)
    {
        index
    } else {
        if !matches!(boundary.operation.as_str(), "prompt" | "location")
            || state.nested.len() >= recovery::MAX_DEPTH
        {
            return Err("shell scope must begin at a prompt".into());
        }
        let replaced = state
            .nested
            .iter()
            .rev()
            .find(|scope| scope.identity == shell.identity);
        let parent = if let Some(scope) = replaced {
            scope
                .execution
                .as_ref()
                .ok_or("exec shell has no active command")?
                .committed
                .invocation
        } else if state.identity == Some(shell.identity) {
            state.execution.committed.invocation
        } else {
            parent_invocation(state, shell.parent)?
        };
        if state.nested.iter().any(|scope| scope.parent == parent) {
            return Err("competing nested shells require an explicit recovery policy".into());
        }
        let restoring = execution_mut(state, parent)?.restoring();
        if !std::path::Path::new(&boundary.shell).is_absolute() || boundary.shell.len() > 4096 {
            return Err("shell executable must be absolute".into());
        }
        let saved = recovery::find(&state.execution.committed, parent)
            .ok_or("parent invocation unavailable")?
            .commands
            .first()
            .cloned();
        let (execution, replay) = match saved {
            Some(command) if restoring && command.shell == boundary.shell => (
                Some(Execution::resurrect(*command.checkpoint).map_err(str::to_owned)?),
                true,
            ),
            Some(_) => return Err("recreated nested shell does not match recovery path".into()),
            None => (None, false),
        };
        state.nested.push(NestedShell {
            identity: shell.identity,
            parent,
            shell: boundary.shell.clone(),
            execution,
            replay,
            offered: false,
            claim: None,
        });
        state.nested.len() - 1
    };
    Ok(index)
}

fn parent_invocation(state: &State, parent_pid: u32) -> Result<Uuid, String> {
    if state
        .identity
        .is_some_and(|identity| identity.pid == parent_pid)
    {
        // An application owner and a nested shell cannot both own one command.
        if state.claim.is_some() {
            return Err("parent invocation already owned".into());
        }
        return Ok(state.execution.committed.invocation);
    }
    let scope = state
        .nested
        .iter()
        .find(|scope| scope.identity.pid == parent_pid)
        .ok_or("nested shell parent is not registered")?;
    if scope.claim.is_some() {
        return Err("parent invocation already owned".into());
    }
    scope
        .execution
        .as_ref()
        .map(|execution| execution.committed.invocation)
        .ok_or_else(|| "parent shell has no active command".into())
}

pub(super) fn target(state: &State, peer: u32) -> Result<(Uuid, Identity), String> {
    let process = process::inspect(peer)?;
    for scope in state.nested.iter().rev() {
        if process.parent == scope.identity.pid || process.identity == scope.identity {
            let execution = scope
                .execution
                .as_ref()
                .ok_or("nested shell has no active command")?;
            if scope.claim.is_some() || scope.replay || !execution.committed.commands.is_empty() {
                return Err("nested invocation unavailable or already owned".into());
            }
            return Ok((execution.committed.invocation, scope.identity));
        }
    }
    if state.claim.is_some() || !state.execution.committed.commands.is_empty() {
        return Err("root invocation unavailable or delegated to nested shell".into());
    }
    Ok((
        state.execution.committed.invocation,
        state.identity.ok_or("unbound")?,
    ))
}

pub(super) fn execution_mut(state: &mut State, invocation: Uuid) -> Result<&mut Execution, String> {
    if state.execution.committed.invocation == invocation {
        return Ok(&mut state.execution);
    }
    state
        .nested
        .iter_mut()
        .filter_map(|scope| scope.execution.as_mut())
        .find(|execution| execution.committed.invocation == invocation)
        .ok_or_else(|| "stale nested invocation".into())
}

pub(super) fn claim(state: &mut State, invocation: Uuid, peer: Identity) -> Result<(), String> {
    if state.execution.committed.invocation == invocation {
        if state.claim.replace(peer.pid).is_some() {
            return Err("invocation already owned".into());
        }
        return Ok(());
    }
    let scope = state
        .nested
        .iter_mut()
        .find(|scope| {
            scope
                .execution
                .as_ref()
                .is_some_and(|execution| execution.committed.invocation == invocation)
        })
        .ok_or("stale nested invocation")?;
    if scope.claim.replace(peer).is_some() {
        return Err("invocation already owned".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;
    use std::process::{Child, Command as ProcessCommand};

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn child() -> ChildGuard {
        ChildGuard(ProcessCommand::new("sleep").arg("30").spawn().unwrap())
    }

    fn boundary(operation: &str, text: &str) -> Boundary {
        Boundary {
            protocol: PROTOCOL.into(),
            operation: operation.into(),
            shell: "/bin/test-shell".into(),
            shell_pid: 0,
            text: text.into(),
            cwd: "/workspace".into(),
        }
    }

    #[test]
    fn successive_nested_commands_are_distinct_and_old_claims_are_fenced() {
        let directory = tempfile::tempdir().unwrap();
        let host = Host::new(directory.path().to_owned()).unwrap();
        let key = Uuid::new_v4().to_string();
        let endpoint = host.prepare(&key, false, true, false).unwrap();
        host.bind(&key, &endpoint, std::process::id()).unwrap();
        host.boundary(&key, &endpoint, true).unwrap();
        host.boundary(&key, &endpoint, false).unwrap();
        let entry = host.entry(&key, &endpoint).unwrap();
        let child = child();
        let shell = process::inspect(child.0.id()).unwrap();
        advance(&entry, shell, boundary("location", "")).unwrap();
        advance(&entry, shell, boundary("prompt", "")).unwrap();
        advance(&entry, shell, boundary("start", "first")).unwrap();
        let first = entry.state.lock().unwrap().nested[0]
            .execution
            .as_ref()
            .unwrap()
            .committed
            .invocation;
        {
            let mut state = entry.state.lock().unwrap();
            let execution = execution_mut(&mut state, first).unwrap();
            let registration = execution.register(Uuid::new_v4()).unwrap();
            let checkpoint = execution
                .update(registration, 1, Some("{}".into()))
                .unwrap();
            let mut durable = state.execution.committed.clone();
            *recovery::find_mut(&mut durable, first).unwrap() = checkpoint;
            state.install_checkpoint(durable);
        }
        advance(&entry, shell, boundary("prompt", "")).unwrap();
        advance(&entry, shell, boundary("start", "second")).unwrap();
        let mut state = entry.state.lock().unwrap();
        let second = state.nested[0]
            .execution
            .as_ref()
            .unwrap()
            .committed
            .invocation;
        assert_ne!(first, second);
        assert!(execution_mut(&mut state, first).is_err());
        assert!(
            execution_mut(&mut state, second)
                .unwrap()
                .committed
                .descriptor
                .is_none()
        );
        let durable = super::super::load_checkpoint(&entry.file).unwrap().unwrap();
        assert_eq!(durable.commands[0].text, "second");
        assert!(durable.commands[0].checkpoint.descriptor.is_none());
    }

    #[test]
    fn nested_replay_is_once_only_and_retains_descriptor_until_application_update() {
        let directory = tempfile::tempdir().unwrap();
        let host = Host::new(directory.path().to_owned()).unwrap();
        let key = Uuid::new_v4().to_string();
        let endpoint = host.prepare(&key, false, true, false).unwrap();
        host.bind(&key, &endpoint, std::process::id()).unwrap();
        host.boundary(&key, &endpoint, true).unwrap();
        host.boundary(&key, &endpoint, false).unwrap();
        let entry = host.entry(&key, &endpoint).unwrap();
        let child = child();
        let shell = process::inspect(child.0.id()).unwrap();
        advance(&entry, shell, boundary("prompt", "")).unwrap();
        advance(&entry, shell, boundary("start", "application")).unwrap();
        {
            let mut state = entry.state.lock().unwrap();
            state.execution.committed.commands[0].checkpoint.descriptor = Some("{}".into());
            save(&entry.file, &state.execution.committed).unwrap();
        }
        let endpoint = host.prepare(&key, true, true, true).unwrap();
        host.bind(&key, &endpoint, std::process::id()).unwrap();
        host.boundary(&key, &endpoint, true).unwrap();
        host.boundary(&key, &endpoint, false).unwrap();
        let entry = host.entry(&key, &endpoint).unwrap();
        assert_eq!(
            advance(&entry, shell, boundary("location", ""))
                .unwrap()
                .as_deref(),
            Some("/workspace")
        );
        advance(&entry, shell, boundary("prompt", "")).unwrap();
        assert_eq!(
            host.take_replay(&key, &endpoint).unwrap().as_deref(),
            Some("application")
        );
        assert!(host.take_replay(&key, &endpoint).unwrap().is_none());
        assert!(advance(&entry, shell, boundary("prompt", "")).is_err());
        assert!(advance(&entry, shell, boundary("start", "other")).is_err());
        advance(&entry, shell, boundary("start", "application")).unwrap();
        let state = entry.state.lock().unwrap();
        let saved = &state.execution.committed.commands[0].checkpoint;
        assert_eq!(saved.generation, 2);
        assert_eq!(saved.descriptor.as_deref(), Some("{}"));
    }
}
