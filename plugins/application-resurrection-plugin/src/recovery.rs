//! Durable nested-shell command paths. This is recovery intent, not a process snapshot.

use crate::execution::Checkpoint;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) const MAX_DEPTH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub shell: String,
    pub text: String,
    pub cwd: String,
    pub checkpoint: Box<Checkpoint>,
}

pub(super) fn validate(
    checkpoint: &Checkpoint,
    depth: usize,
    identities: &mut std::collections::BTreeSet<Uuid>,
) -> Result<(), &'static str> {
    let commands = &checkpoint.commands;
    if !identities.insert(checkpoint.invocation) {
        return Err("duplicate recovery invocation");
    }
    if checkpoint.descriptor.is_some() && !commands.is_empty() {
        return Err("invocation cannot own both an application and a nested shell");
    }
    // A foreground recovery path has one owner at each level. Parallel shell
    // branches require an explicit recovery policy, not first-writer selection.
    if commands.len() > 1 || depth > MAX_DEPTH {
        return Err("ambiguous or excessively deep recovery path");
    }
    for command in commands {
        if command.shell.is_empty()
            || command.shell.len() > 4096
            || command.text.trim().is_empty()
            || command.text.len() > 8192
            || command.cwd.is_empty()
            || command.cwd.len() > 4096
            || !std::path::Path::new(&command.cwd).is_absolute()
            || !std::path::Path::new(&command.shell).is_absolute()
        {
            return Err("invalid nested recovery command");
        }
        command.checkpoint.validate_header()?;
        validate(&command.checkpoint, depth + 1, identities)?;
    }
    Ok(())
}

pub(super) fn find(checkpoint: &Checkpoint, invocation: Uuid) -> Option<&Checkpoint> {
    if checkpoint.invocation == invocation {
        return Some(checkpoint);
    }
    checkpoint
        .commands
        .first()
        .and_then(|command| find(&command.checkpoint, invocation))
}

pub(super) fn find_mut(checkpoint: &mut Checkpoint, invocation: Uuid) -> Option<&mut Checkpoint> {
    if checkpoint.invocation == invocation {
        return Some(checkpoint);
    }
    checkpoint
        .commands
        .first_mut()
        .and_then(|command| find_mut(&mut command.checkpoint, invocation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Execution;

    fn path() -> Checkpoint {
        let mut root = Execution::manual().committed;
        root.version = 2;
        root.commands.push(Command {
            shell: "/bin/nu".into(),
            text: "bcode".into(),
            cwd: "/workspace".into(),
            checkpoint: Box::new(Execution::manual().committed),
        });
        root
    }

    #[test]
    fn nested_checkpoint_round_trips_and_finds_exact_invocation() {
        let root = path();
        let child_id = root.commands[0].checkpoint.invocation;
        root.validate().unwrap();
        let restored: Checkpoint =
            serde_json::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        assert_eq!(restored, root);
        assert_eq!(find(&restored, child_id).unwrap().invocation, child_id);
        assert!(find(&restored, Uuid::new_v4()).is_none());
        let mut updated = restored;
        find_mut(&mut updated, child_id).unwrap().descriptor =
            Some("{\"application\":\"test\"}".into());
        assert!(updated.descriptor.is_none());
        assert!(updated.commands[0].checkpoint.descriptor.is_some());
    }

    #[test]
    fn invalid_or_ambiguous_paths_cannot_be_recovered() {
        let mut root = path();
        root.commands[0].checkpoint.invocation = root.invocation;
        assert!(root.validate().is_err());
        let mut root = path();
        root.descriptor = Some("{}".into());
        assert!(root.validate().is_err());
        let mut root = path();
        root.commands.push(root.commands[0].clone());
        assert!(root.validate().is_err());
        let mut root = path();
        root.version = 1;
        assert!(root.validate().is_err());
        let mut root = path();
        root.commands[0].cwd = "relative".into();
        assert!(root.validate().is_err());
        let mut root = path();
        root.commands[0].checkpoint.generation = 0;
        assert!(root.validate().is_err());
    }

    #[test]
    fn excessive_depth_is_rejected() {
        let mut root = Execution::manual().committed;
        for _ in 0..=MAX_DEPTH {
            let mut parent = path();
            *parent.commands[0].checkpoint = root;
            root = parent;
        }
        assert!(root.validate().is_err());
    }
}
