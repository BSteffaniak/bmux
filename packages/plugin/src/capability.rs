use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    Commands,
    EventSubscription,
    KeyActions,
    StatusBarItems,
    PersistentStorage,
    Clipboard,
    PermissionRead,
    PermissionWrite,
    SessionRead,
    SessionWrite,
    WindowRead,
    WindowWrite,
    PaneRead,
    PaneWrite,
    FollowRead,
    FollowWrite,
    PersistenceRead,
    PersistenceWrite,
    AttachOverlay,
    TerminalProtocolObserve,
    TerminalInputIntercept,
    TerminalOutputIntercept,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginCapabilityTier {
    Automation,
    Integration,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRisk {
    Safe,
    Elevated,
    HotPath,
}

impl PluginCapability {
    #[must_use]
    pub const fn tier(self) -> PluginCapabilityTier {
        match self {
            Self::Commands
            | Self::EventSubscription
            | Self::KeyActions
            | Self::StatusBarItems
            | Self::PersistentStorage
            | Self::Clipboard => PluginCapabilityTier::Automation,
            Self::PermissionRead
            | Self::PermissionWrite
            | Self::FollowRead
            | Self::FollowWrite
            | Self::PersistenceRead
            | Self::PersistenceWrite
            | Self::SessionRead
            | Self::SessionWrite
            | Self::WindowRead
            | Self::WindowWrite
            | Self::PaneRead
            | Self::PaneWrite
            | Self::AttachOverlay => PluginCapabilityTier::Integration,
            Self::TerminalProtocolObserve
            | Self::TerminalInputIntercept
            | Self::TerminalOutputIntercept => PluginCapabilityTier::Runtime,
        }
    }

    #[must_use]
    pub const fn risk(self) -> PluginRisk {
        match self {
            Self::Commands
            | Self::EventSubscription
            | Self::KeyActions
            | Self::StatusBarItems
            | Self::PersistentStorage
            | Self::Clipboard
            | Self::PermissionRead
            | Self::SessionRead
            | Self::WindowRead
            | Self::PaneRead
            | Self::FollowRead
            | Self::PersistenceRead => PluginRisk::Safe,
            Self::PermissionWrite
            | Self::SessionWrite
            | Self::WindowWrite
            | Self::PaneWrite
            | Self::FollowWrite
            | Self::PersistenceWrite
            | Self::AttachOverlay
            | Self::TerminalProtocolObserve => PluginRisk::Elevated,
            Self::TerminalInputIntercept | Self::TerminalOutputIntercept => PluginRisk::HotPath,
        }
    }

    #[must_use]
    pub const fn is_hot_path(self) -> bool {
        matches!(self.risk(), PluginRisk::HotPath)
    }
}

impl fmt::Display for PluginCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Commands => "commands",
            Self::EventSubscription => "event_subscription",
            Self::KeyActions => "key_actions",
            Self::StatusBarItems => "status_bar_items",
            Self::PersistentStorage => "persistent_storage",
            Self::Clipboard => "clipboard",
            Self::PermissionRead => "permission_read",
            Self::PermissionWrite => "permission_write",
            Self::SessionRead => "session_read",
            Self::SessionWrite => "session_write",
            Self::WindowRead => "window_read",
            Self::WindowWrite => "window_write",
            Self::PaneRead => "pane_read",
            Self::PaneWrite => "pane_write",
            Self::FollowRead => "follow_read",
            Self::FollowWrite => "follow_write",
            Self::PersistenceRead => "persistence_read",
            Self::PersistenceWrite => "persistence_write",
            Self::AttachOverlay => "attach_overlay",
            Self::TerminalProtocolObserve => "terminal_protocol_observe",
            Self::TerminalInputIntercept => "terminal_input_intercept",
            Self::TerminalOutputIntercept => "terminal_output_intercept",
        };
        f.write_str(name)
    }
}
