#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Event system for bmux terminal multiplexer
//!
//! This package provides the event system including event handling,
//! modal system management, and input processing.

// Re-export models for easy access
pub use bmux_event_models as models;

// Re-export commonly used types
pub use models::{
    ClientEvent, Event, InputEvent, KeyCode, KeyEvent, KeyModifiers, Mode, MouseEvent, PaneEvent,
    SessionEvent, SystemEvent, WindowEvent,
};

use anyhow::Result;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Event dispatcher responsible for routing events
#[derive(Debug)]
pub struct EventDispatcher {
    event_sender: broadcast::Sender<Event>,
    #[allow(dead_code)]
    event_receiver: broadcast::Receiver<Event>,
}

impl EventDispatcher {
    /// Create a new event dispatcher
    #[must_use]
    pub fn new() -> Self {
        let (event_sender, event_receiver) = broadcast::channel(1000);

        Self {
            event_sender,
            event_receiver,
        }
    }

    /// Send an event
    /// 
    /// # Errors
    /// 
    /// Returns an error if the event cannot be sent to subscribers.
    pub fn send_event(&self, event: Event) -> Result<()> {
        match self.event_sender.send(event) {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to send event: {}", e);
                Err(anyhow::anyhow!("Failed to send event: {}", e))
            }
        }
    }

    /// Subscribe to events
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_sender.subscribe()
    }

    /// Get the number of active subscribers
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.event_sender.receiver_count()
    }
}

impl Default for EventDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Modal system manager
#[derive(Debug, Default)]
pub struct ModalSystem {
    current_mode: Mode,
}

impl ModalSystem {
    /// Create a new modal system
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_mode: Mode::Normal,
        }
    }

    /// Get the current mode
    #[must_use]
    pub const fn current_mode(&self) -> Mode {
        self.current_mode
    }

    /// Attempt to transition to a new mode
    /// 
    /// # Errors
    /// 
    /// Returns an error if the mode transition is not valid.
    pub fn transition_to(&mut self, new_mode: Mode) -> Result<()> {
        if self.current_mode.can_transition_to(new_mode) {
            info!("Mode transition: {:?} -> {:?}", self.current_mode, new_mode);
            self.current_mode = new_mode;
            Ok(())
        } else {
            warn!(
                "Invalid mode transition: {:?} -> {:?}",
                self.current_mode, new_mode
            );
            Err(anyhow::anyhow!("Invalid mode transition"))
        }
    }

    /// Handle a key event in the current mode
    /// 
    /// # Errors
    /// 
    /// Returns an error if the key event handling fails or mode transition is invalid.
    pub fn handle_key_event(&mut self, key_event: &KeyEvent) -> Result<Option<Event>> {
        match self.current_mode {
            Mode::Normal => self.handle_normal_mode_key(key_event),
            Mode::Insert => self.handle_insert_mode_key(key_event),
            Mode::Visual => self.handle_visual_mode_key(key_event),
            Mode::Command => self.handle_command_mode_key(key_event),
        }
    }

    fn handle_normal_mode_key(&mut self, key_event: &KeyEvent) -> Result<Option<Event>> {
        // Handle Normal mode specific keys
        match key_event.code {
            KeyCode::Char('i') => {
                self.transition_to(Mode::Insert)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Insert))))
            }
            KeyCode::Char('v') => {
                self.transition_to(Mode::Visual)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Visual))))
            }
            KeyCode::Char(':') => {
                self.transition_to(Mode::Command)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Command))))
            }
            _ => Ok(None), // Pass through other keys
        }
    }

    fn handle_insert_mode_key(&mut self, key_event: &KeyEvent) -> Result<Option<Event>> {
        // Handle Insert mode specific keys
        match key_event.code {
            KeyCode::Escape => {
                self.transition_to(Mode::Normal)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Normal))))
            }
            _ => Ok(None), // Pass through other keys for terminal input
        }
    }

    fn handle_visual_mode_key(&mut self, key_event: &KeyEvent) -> Result<Option<Event>> {
        // Handle Visual mode specific keys
        match key_event.code {
            KeyCode::Escape => {
                self.transition_to(Mode::Normal)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Normal))))
            }
            _ => Ok(None), // Pass through other keys
        }
    }

    fn handle_command_mode_key(&mut self, key_event: &KeyEvent) -> Result<Option<Event>> {
        // Handle Command mode specific keys
        match key_event.code {
            KeyCode::Escape => {
                self.transition_to(Mode::Normal)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Normal))))
            }
            KeyCode::Enter => {
                // TODO: Execute command
                self.transition_to(Mode::Normal)?;
                Ok(Some(Event::System(SystemEvent::ModeChanged(Mode::Normal))))
            }
            _ => Ok(None), // Pass through other keys for command input
        }
    }
}
