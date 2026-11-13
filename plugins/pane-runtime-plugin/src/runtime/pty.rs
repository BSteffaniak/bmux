//! Process handles for pane terminals.
//!
//! Startup wraps the process spawned on a terminal (to tie it to supervision, or to observe the
//! native spawn), and the wrappers must stand in for the process itself; these traits are what
//! every such handle provides. `bmux_pty` supplies the terminals and the processes.

use std::io;

use bmux_pty::{ChildExit, PtyChild, PtyChildKiller, PtySlave, SpawnCommand};

/// Ends a process from wherever the handle was cloned to.
pub(crate) trait ChildKiller: std::fmt::Debug + Send {
    /// End the process. A process that has already exited counts as ended.
    fn kill(&mut self) -> io::Result<()>;

    /// Another handle ending the same process.
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync>;
}

/// A process started on a pane terminal.
pub(crate) trait Child: ChildKiller {
    /// How the process ended, if it has, without waiting.
    // Only Windows startup polls its helper; Unix startup negotiates over a socket instead.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn try_wait(&mut self) -> io::Result<Option<ChildExit>>;

    /// Wait for the process to end.
    fn wait(&mut self) -> io::Result<ChildExit>;

    /// Operating system process id, when known.
    fn process_id(&self) -> Option<u32>;
}

/// Where a process can be started on a terminal: the terminal's slave, or a wrapper observing
/// the spawn.
pub(crate) trait Slave {
    fn spawn_command(&self, command: SpawnCommand) -> io::Result<Box<dyn Child + Send + Sync>>;
}

impl Slave for PtySlave {
    fn spawn_command(&self, command: SpawnCommand) -> io::Result<Box<dyn Child + Send + Sync>> {
        Ok(Box::new(self.spawn(&command)?))
    }
}

impl ChildKiller for PtyChild {
    fn kill(&mut self) -> io::Result<()> {
        Self::kill(self)
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(Killer(self.killer().map_err(|error| error.to_string())))
    }
}

impl Child for PtyChild {
    fn try_wait(&mut self) -> io::Result<Option<ChildExit>> {
        Self::try_wait(self)
    }

    fn wait(&mut self) -> io::Result<ChildExit> {
        Self::wait(self)
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.id())
    }
}

/// A cloned killer, or why the process handle could not be duplicated for one (reported when
/// it is used).
#[derive(Debug, Clone)]
struct Killer(Result<PtyChildKiller, String>);

impl ChildKiller for Killer {
    fn kill(&mut self) -> io::Result<()> {
        match &self.0 {
            Ok(killer) => killer.kill(),
            Err(error) => Err(io::Error::other(format!(
                "process handle unavailable for killing: {error}"
            ))),
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn spawned_processes_wait_kill_and_clone_killers() {
        let pair = bmux_pty::PtyPair::open(bmux_pty::PtySize { cols: 80, rows: 24 }).unwrap();
        let mut command = SpawnCommand::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        assert!(child.process_id().is_some());
        assert!(child.try_wait().unwrap().is_none());
        let mut killer = child.clone_killer();
        killer.kill().unwrap();
        let exit = child.wait().unwrap();
        assert!(!exit.success());
        // Both handles accept ending a process that is gone.
        killer.kill().unwrap();
        child.kill().unwrap();
        let mut copy = killer.clone_killer();
        copy.kill().unwrap();
    }
}
