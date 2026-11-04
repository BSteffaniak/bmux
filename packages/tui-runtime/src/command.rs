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

    /// Scheduling policy of this command.
    #[must_use]
    pub const fn policy(&self) -> &CommandPolicy {
        &self.policy
    }

    /// Rewrite the key this command is scheduled under, keeping its policy.
    ///
    /// Useful for namespacing keyed work, so that commands built by the same
    /// code for independent owners do not replace or cancel each other.
    #[must_use]
    pub fn map_key(mut self, map: impl FnOnce(CommandKey) -> CommandKey) -> Self {
        self.policy = match self.policy {
            CommandPolicy::Concurrent => CommandPolicy::Concurrent,
            CommandPolicy::StartIfIdle(key) => CommandPolicy::StartIfIdle(map(key)),
            CommandPolicy::Replace(key) => CommandPolicy::Replace(map(key)),
            CommandPolicy::QueueLatest(key) => CommandPolicy::QueueLatest(map(key)),
            CommandPolicy::Cancel(key) => CommandPolicy::Cancel(map(key)),
        };
        self
    }
}

impl<M: Send + 'static> Command<M> {
    /// Transform the message this command produces, keeping its policy.
    ///
    /// Lets a component's commands be embedded in a program whose message
    /// type wraps the component's.
    #[must_use]
    pub fn map<N: Send + 'static>(self, map: impl FnOnce(M) -> N + Send + 'static) -> Command<N> {
        Command {
            policy: self.policy,
            future: self.future.map(|future| -> CommandFuture<N> {
                Box::pin(async move { future.await.map(map) })
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn map_transforms_the_message_and_keeps_the_policy() {
        let command = Command::replace(CommandKey::new("work"), async { Some(2_u8) })
            .map(|value| format!("got {value}"));
        assert_eq!(
            command.policy(),
            &CommandPolicy::Replace(CommandKey::new("work"))
        );
        let output = command.future.expect("future").await;
        assert_eq!(output.as_deref(), Some("got 2"));
    }

    #[test]
    fn map_key_rewrites_keyed_policies_only() {
        let scoped = |key: CommandKey| CommandKey::new(format!("owner:{}", key.as_str()));
        let keyed = Command::<u8>::cancel(CommandKey::new("work")).map_key(scoped);
        assert_eq!(
            keyed.policy(),
            &CommandPolicy::Cancel(CommandKey::new("owner:work"))
        );
        let free = Command::concurrent(async { Some(1_u8) }).map_key(scoped);
        assert_eq!(free.policy(), &CommandPolicy::Concurrent);
    }
}
