#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal handling and PTY management for bmux
//!
//! This package provides terminal abstraction and PTY (pseudo-terminal)
//! management for the bmux terminal multiplexer.

// Re-export models for easy access
pub use bmux_terminal_models as models;

// Re-export commonly used types
pub use models::{Pane, PaneLayout, PaneSize, SplitDirection, Window, WindowInfo};

use anyhow::Result;
use bmux_session::PaneId;
use std::collections::BTreeMap;
use tracing::{info, warn};

/// Terminal manager responsible for handling terminal operations
#[derive(Debug, Default)]
pub struct TerminalManager {
    active_terminals: BTreeMap<PaneId, TerminalInstance>,
}

/// Represents a single terminal instance
#[derive(Debug)]
pub struct TerminalInstance {
    #[allow(dead_code)]
    pane_id: PaneId,
    size: PaneSize,
    // TODO: Add PTY handle and other terminal state
}

impl TerminalInstance {
    /// Create a new terminal instance
    #[must_use]
    pub const fn new(pane_id: PaneId, size: PaneSize) -> Self {
        Self { pane_id, size }
    }
}

impl TerminalManager {
    /// Create a new terminal manager
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active_terminals: BTreeMap::new(),
        }
    }

    /// Create a new terminal instance
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal creation fails or if a terminal with the same pane ID already exists.
    pub fn create_terminal(&mut self, pane_id: PaneId, size: PaneSize) -> Result<()> {
        let terminal = TerminalInstance::new(pane_id, size);

        if self.active_terminals.insert(pane_id, terminal).is_some() {
            warn!("Terminal already exists for pane: {}", pane_id);
        }

        info!("Created terminal: {}", pane_id);
        Ok(())
    }

    /// Get a terminal instance by pane ID
    #[must_use]
    pub fn get_terminal(&self, pane_id: &PaneId) -> Option<&TerminalInstance> {
        self.active_terminals.get(pane_id)
    }

    /// Resize a terminal
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal doesn't exist or if the resize operation fails.
    pub fn resize_terminal(&mut self, pane_id: &PaneId, new_size: PaneSize) -> Result<()> {
        if let Some(terminal) = self.active_terminals.get_mut(pane_id) {
            terminal.size = new_size;
            info!("Resized terminal: {} to {:?}", pane_id, new_size);
            Ok(())
        } else {
            warn!("Attempted to resize non-existent terminal: {}", pane_id);
            Err(anyhow::anyhow!("Terminal not found"))
        }
    }

    /// Remove a terminal instance
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal doesn't exist.
    pub fn remove_terminal(&mut self, pane_id: &PaneId) -> Result<()> {
        if self.active_terminals.remove(pane_id).is_some() {
            info!("Removed terminal: {}", pane_id);
            Ok(())
        } else {
            warn!("Attempted to remove non-existent terminal: {}", pane_id);
            Err(anyhow::anyhow!("Terminal not found"))
        }
    }

    /// Get the number of active terminals
    #[must_use]
    pub fn terminal_count(&self) -> usize {
        self.active_terminals.len()
    }
}
