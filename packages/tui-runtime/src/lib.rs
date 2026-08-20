#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bounded scheduling and presentation runtime for BMUX terminal user interfaces.
//!
//! The runtime serializes application updates while independently bounding reliable terminal and
//! application admission, keyed latest-value updates, timers, commands, and cadence-limited
//! presentation. Product state and semantics remain application-owned. After each successful
//! presentation, the application receives one neutral commit callback; any update returned by that
//! callback is scheduled through the same bounded cadence and lifecycle machinery as an ordinary
//! event update.

pub mod command;
mod command_scheduler;
pub mod config;
pub mod ids;
#[cfg(feature = "images")]
pub mod image_terminal_presenter;
#[cfg(feature = "crossterm")]
pub mod input;
pub mod invalidation;
mod mailbox;
pub mod presenter;
pub mod program;
pub mod runtime;
pub mod stats;
pub mod subscription;
mod subscription_scheduler;
pub mod terminal_presenter;

pub use command::{Command, CommandPolicy};
pub use config::RuntimeConfig;
pub use ids::{CommandKey, MessageKey, SubscriptionKey, TimerId};
#[cfg(feature = "images")]
pub use image_terminal_presenter::{ImagePresentationError, ImageTerminalPresenter};
#[cfg(feature = "crossterm")]
pub use input::{ManagedTerminalInput, TerminalInput};
pub use invalidation::InvalidationSignal;
pub use mailbox::{LatestSendError, LatestSendOutcome, LatestSender, SendError, TrySendError};
pub use presenter::{HeadlessPresenter, PresentReport, Presenter, ResetReason};
pub use program::{Invalidation, Lifecycle, Program, RuntimeEvent, Update};
pub use runtime::{Runtime, RuntimeError, RuntimeHandle, RuntimeOutput};
pub use stats::RuntimeStats;
pub use subscription::Subscription;
pub use terminal_presenter::TerminalPresenter;

#[cfg(test)]
mod tests;
