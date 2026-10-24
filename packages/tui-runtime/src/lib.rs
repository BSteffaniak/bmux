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
#[cfg(feature = "crossterm")]
pub mod input_drain;
pub mod invalidation;
#[cfg(all(target_os = "linux", feature = "crossterm"))]
pub mod linux_capture;
mod mailbox;
pub mod presenter;
#[cfg(feature = "crossterm")]
pub mod private_input;
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
pub use image_terminal_presenter::{
    AsyncImageTerminalPresenter, ImagePresentationError, ImageTerminalPresenter,
};
#[cfg(feature = "crossterm")]
pub use input::{ManagedTerminalInput, TerminalInput};
pub use invalidation::InvalidationSignal;
pub use mailbox::{LatestSendError, LatestSendOutcome, LatestSender, SendError, TrySendError};
pub use presenter::{CancellableOutput, HeadlessPresenter, PresentReport, Presenter, ResetReason};
pub use program::{Invalidation, Lifecycle, Program, RuntimeEvent, Update};
pub use runtime::{Runtime, RuntimeError, RuntimeHandle, RuntimeOutput};
pub use stats::{
    DetailedRuntimeStats, DurationHistogram, PresentationSample, QueueSource, RuntimeStats,
    TerminalDispatchRange,
};
pub use subscription::Subscription;
pub use terminal_presenter::TerminalPresenter;

#[cfg(test)]
mod telemetry_bench;
#[cfg(test)]
mod tests;
