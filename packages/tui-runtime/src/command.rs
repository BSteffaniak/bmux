//! Application command futures and scheduling policy.

use std::future::Future;
use std::pin::Pin;

use crate::ids::CommandKey;

/// Boxed runtime command future returning zero or one application message.
pub type CommandFuture<M> = Pin<Box<dyn Future<Output = Option<M>> + Send + 'static>>;

/// Scheduling policy for an application command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPolicy {
    /// Start independently of other commands.
    Concurrent,
    /// Start only when no command with this key is active or queued.
    StartIfIdle(CommandKey),
    /// Abort active work and replace queued work for this key.
    Replace(CommandKey),
    /// Run after active work for this key, retaining only the latest queued command.
    QueueLatest(CommandKey),
    /// Cancel active and queued work for this key without starting a future.
    Cancel(CommandKey),
}

/// Application-supplied asynchronous work.
pub struct Command<M> {
    pub(crate) policy: CommandPolicy,
    pub(crate) future: Option<CommandFuture<M>>,
}

impl<M> Command<M> {
    /// Create an independent command.
    #[must_use]
    pub fn concurrent(future: impl Future<Output = Option<M>> + Send + 'static) -> Self {
        Self {
            policy: CommandPolicy::Concurrent,
            future: Some(Box::pin(future)),
        }
    }

    /// Create a keyed command that starts only when idle.
    #[must_use]
    pub fn start_if_idle(
        key: CommandKey,
        future: impl Future<Output = Option<M>> + Send + 'static,
    ) -> Self {
        Self {
            policy: CommandPolicy::StartIfIdle(key),
            future: Some(Box::pin(future)),
        }
    }

    /// Create a keyed replacement command.
    #[must_use]
    pub fn replace(
        key: CommandKey,
        future: impl Future<Output = Option<M>> + Send + 'static,
    ) -> Self {
        Self {
            policy: CommandPolicy::Replace(key),
            future: Some(Box::pin(future)),
        }
    }

    /// Create a keyed queue-latest command.
    #[must_use]
    pub fn queue_latest(
        key: CommandKey,
        future: impl Future<Output = Option<M>> + Send + 'static,
    ) -> Self {
        Self {
            policy: CommandPolicy::QueueLatest(key),
            future: Some(Box::pin(future)),
        }
    }

    /// Cancel active and queued work for a key.
    #[must_use]
    pub const fn cancel(key: CommandKey) -> Self {
        Self {
            policy: CommandPolicy::Cancel(key),
            future: None,
        }
    }
}
