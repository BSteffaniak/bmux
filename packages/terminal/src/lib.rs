#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal control for bmux: raw mode, size queries, ANSI output commands,
//! and decoded keyboard, mouse, focus, paste, and resize input.
//!
//! Output is expressed as [`Command`] values written with [`queue!`] (buffer
//! only) or [`execute!`] (buffer and flush). Each command writes its escape
//! sequence straight into the destination as bytes.
//!
//! Input is read from one process-wide reader through [`event::poll`],
//! [`event::read`], or the asynchronous [`event::EventStream`].
//!
//! On Windows, commands that need console API calls (mouse capture) use them,
//! and every other command writes virtual-terminal sequences once VT output
//! processing has been enabled on the console.

pub mod command;
pub mod cursor;
pub mod event;
pub mod keyboard;
pub mod style;
pub mod terminal;

mod sys;

#[cfg(unix)]
pub mod unix;

pub use command::{Command, ExecutableCommand, QueueableCommand};

#[cfg(windows)]
pub use sys::windows::supports_ansi;
