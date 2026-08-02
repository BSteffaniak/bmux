//! Program update and presentation contracts.

use bmux_tui::event::Event;

use crate::command::Command;
use crate::ids::{SubscriptionKey, TimerId};
use crate::subscription::Subscription;

/// Event delivered serially to an application program.
#[derive(Debug)]
pub enum RuntimeEvent<M> {
    /// Terminal input or lifecycle event.
    Terminal(Event),
    /// Application-owned message.
    Message(M),
    /// One-shot timer became due.
    Timer(TimerId),
}

/// Presentation intent returned by one program update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Invalidation {
    /// No presentation is needed.
    #[default]
    None,
    /// Present the current application state at the configured cadence.
    Redraw,
    /// Reset backend retained state before presenting the current application state.
    Reset,
}

/// Runtime lifecycle intent returned by one program update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lifecycle {
    /// Continue processing events.
    #[default]
    Continue,
    /// Exit after presenting pending dirty state when cadence permits.
    Exit,
    /// Exit without waiting for another presentation.
    Abort,
}

/// Complete result of one serialized program update.
pub struct Update<M> {
    /// Presentation intent.
    pub invalidation: Invalidation,
    /// Runtime lifecycle intent.
    pub lifecycle: Lifecycle,
    /// Commands to schedule after the update returns.
    pub commands: Vec<Command<M>>,
    /// Long-lived subscriptions to start or replace after the update returns.
    pub subscriptions: Vec<Subscription<M>>,
    /// Long-lived subscription keys to cancel after the update returns.
    pub cancelled_subscriptions: Vec<SubscriptionKey>,
}

impl<M> Update<M> {
    /// Continue without presentation or commands.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            invalidation: Invalidation::None,
            lifecycle: Lifecycle::Continue,
            commands: Vec::new(),
            subscriptions: Vec::new(),
            cancelled_subscriptions: Vec::new(),
        }
    }

    /// Continue and request presentation.
    #[must_use]
    pub const fn redraw() -> Self {
        Self {
            invalidation: Invalidation::Redraw,
            lifecycle: Lifecycle::Continue,
            commands: Vec::new(),
            subscriptions: Vec::new(),
            cancelled_subscriptions: Vec::new(),
        }
    }

    /// Continue and reset retained backend state before presentation.
    #[must_use]
    pub const fn reset() -> Self {
        Self {
            invalidation: Invalidation::Reset,
            lifecycle: Lifecycle::Continue,
            commands: Vec::new(),
            subscriptions: Vec::new(),
            cancelled_subscriptions: Vec::new(),
        }
    }

    /// Request graceful exit.
    #[must_use]
    pub const fn exit() -> Self {
        Self {
            invalidation: Invalidation::None,
            lifecycle: Lifecycle::Exit,
            commands: Vec::new(),
            subscriptions: Vec::new(),
            cancelled_subscriptions: Vec::new(),
        }
    }

    /// Request immediate abort.
    #[must_use]
    pub const fn abort() -> Self {
        Self {
            invalidation: Invalidation::None,
            lifecycle: Lifecycle::Abort,
            commands: Vec::new(),
            subscriptions: Vec::new(),
            cancelled_subscriptions: Vec::new(),
        }
    }

    /// Add one command to schedule after the update.
    #[must_use]
    pub fn with_command(mut self, command: Command<M>) -> Self {
        self.commands.push(command);
        self
    }

    /// Add one subscription to start or replace after the update.
    #[must_use]
    pub fn with_subscription(mut self, subscription: Subscription<M>) -> Self {
        self.subscriptions.push(subscription);
        self
    }

    /// Cancel one active subscription after the update.
    #[must_use]
    pub fn cancel_subscription(mut self, key: SubscriptionKey) -> Self {
        self.cancelled_subscriptions.push(key);
        self
    }

    /// Merge another update into this update.
    #[must_use]
    pub fn merge(mut self, mut other: Self) -> Self {
        self.invalidation = self.invalidation.max(other.invalidation);
        self.lifecycle = match (self.lifecycle, other.lifecycle) {
            (Lifecycle::Abort, _) | (_, Lifecycle::Abort) => Lifecycle::Abort,
            (Lifecycle::Exit, _) | (_, Lifecycle::Exit) => Lifecycle::Exit,
            (Lifecycle::Continue, Lifecycle::Continue) => Lifecycle::Continue,
        };
        self.commands.append(&mut other.commands);
        self.subscriptions.append(&mut other.subscriptions);
        self.cancelled_subscriptions
            .append(&mut other.cancelled_subscriptions);
        self
    }
}

impl<M> Default for Update<M> {
    fn default() -> Self {
        Self::none()
    }
}

/// Application state machine driven by the runtime.
pub trait Program {
    /// Typed application message.
    type Message: Send + 'static;
    /// Application update error.
    type Error;

    /// Apply one event to application state.
    ///
    /// # Errors
    ///
    /// Returns an application-defined error when the event cannot be applied. The runtime stops
    /// without presenting state produced after the failing update.
    fn update(
        &mut self,
        event: RuntimeEvent<Self::Message>,
    ) -> Result<Update<Self::Message>, Self::Error>;
}
