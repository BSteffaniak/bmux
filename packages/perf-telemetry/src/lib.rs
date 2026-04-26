use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseChannel {
    Plugin,
    Attach,
    Service,
    Ipc,
    Storage,
}

impl PhaseChannel {
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Plugin => "[bmux-plugin-phase-json]",
            Self::Attach => "[bmux-attach-phase-json]",
            Self::Service => "[bmux-service-phase-json]",
            Self::Ipc => "[bmux-ipc-phase-json]",
            Self::Storage => "[bmux-storage-phase-json]",
        }
    }

    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::Plugin => "BMUX_PLUGIN_PHASE_TIMING",
            Self::Attach => "BMUX_ATTACH_PHASE_TIMING",
            Self::Service => "BMUX_SERVICE_PHASE_TIMING",
            Self::Ipc => "BMUX_IPC_PHASE_TIMING",
            Self::Storage => "BMUX_PLUGIN_STORAGE_PHASE_TIMING",
        }
    }

    #[must_use]
    pub fn enabled(self) -> bool {
        std::env::var_os(self.env_var()).is_some()
    }
}

pub fn emit(channel: PhaseChannel, payload: &Value) {
    if channel.enabled() {
        eprintln!("{}{payload}", channel.marker());
    }
}

#[derive(Debug, Clone)]
pub struct PhaseTimer {
    started_at: Instant,
}

impl PhaseTimer {
    #[must_use]
    pub fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    #[must_use]
    pub fn elapsed_us(&self) -> u128 {
        self.elapsed().as_micros()
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u128 {
        self.elapsed().as_millis()
    }
}

#[must_use]
pub fn elapsed_us_since(started_at: Instant) -> u128 {
    started_at.elapsed().as_micros()
}
