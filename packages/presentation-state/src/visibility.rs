//! Runtime visibility of a retained presentation, and its optional local memory.
//!
//! A presentation that can be shown or hidden while running keeps one
//! [`PresentationVisibility`] per runtime installation. Commands change only
//! that installation. When remembering is enabled, the last state is written
//! to a file in a directory the host chose, and the next installation that
//! remembers starts from it instead of the configured default.

use std::path::{Path, PathBuf};

/// File name of the remembered state inside a presentation's state directory.
const FILE_NAME: &str = "visibility";
const SHOWN: &str = "shown";
const HIDDEN: &str = "hidden";

/// A request to change whether a presentation is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityCommand {
    Toggle,
    Show,
    Hide,
}

impl VisibilityCommand {
    /// Command names, in the order presentations declare them.
    pub const NAMES: [&'static str; 3] = ["toggle", "show", "hide"];

    /// The command named `name`, if it is one of [`Self::NAMES`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "toggle" => Some(Self::Toggle),
            "show" => Some(Self::Show),
            "hide" => Some(Self::Hide),
            _ => None,
        }
    }

    /// The visibility that results from applying this command to `visible`.
    #[must_use]
    pub const fn apply(self, visible: bool) -> bool {
        match self {
            Self::Toggle => !visible,
            Self::Show => true,
            Self::Hide => false,
        }
    }
}

/// A presentation's visibility settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibilitySettings {
    /// Whether the presentation starts shown when nothing remembered says
    /// otherwise.
    pub visible: bool,
    /// Start from the last state remembered on this machine (on unless
    /// turned off): what you last showed or hid comes back.
    pub remember: bool,
}

impl Default for VisibilitySettings {
    fn default() -> Self {
        Self {
            visible: true,
            remember: true,
        }
    }
}

/// Whether one installation is shown, and where it remembers that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationVisibility {
    visible: bool,
    memory: Option<PathBuf>,
}

impl PresentationVisibility {
    /// Start from the remembered state when `settings.remember` is set and
    /// `directory` holds one, otherwise from `settings.visible`.
    ///
    /// Without a directory nothing is remembered, even when remembering is set.
    #[must_use]
    pub fn new(settings: VisibilitySettings, directory: Option<PathBuf>) -> Self {
        let memory = directory
            .filter(|_| settings.remember)
            .map(|directory| directory.join(FILE_NAME));
        let visible = memory
            .as_deref()
            .and_then(read_remembered)
            .unwrap_or(settings.visible);
        Self { visible, memory }
    }

    #[must_use]
    pub const fn visible(&self) -> bool {
        self.visible
    }

    /// Apply `command`, remembering the result when this installation
    /// remembers. Returns whether visibility changed.
    ///
    /// # Errors
    /// Returns a description of a failure to remember the new state. The new
    /// state still applies to this installation.
    pub fn apply(&mut self, command: VisibilityCommand) -> Result<bool, String> {
        let visible = command.apply(self.visible);
        let changed = visible != self.visible;
        self.visible = visible;
        if let Some(file) = &self.memory {
            write_remembered(file, visible).map_err(|error| {
                format!("remembering visibility in {}: {error}", file.display())
            })?;
        }
        Ok(changed)
    }
}

fn read_remembered(file: &Path) -> Option<bool> {
    match std::fs::read_to_string(file).ok()?.trim() {
        SHOWN => Some(true),
        HIDDEN => Some(false),
        _ => None,
    }
}

fn write_remembered(file: &Path, visible: bool) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Replace the file whole so a concurrent reader never sees a partial write.
    let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, if visible { SHOWN } else { HIDDEN })?;
    std::fs::rename(&temporary, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "bmux-presentation-visibility-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn commands_toggle_show_and_hide() {
        assert_eq!(
            VisibilityCommand::NAMES.map(VisibilityCommand::from_name),
            [
                Some(VisibilityCommand::Toggle),
                Some(VisibilityCommand::Show),
                Some(VisibilityCommand::Hide)
            ]
        );
        assert_eq!(VisibilityCommand::from_name("flip"), None);
        assert!(!VisibilityCommand::Toggle.apply(true));
        assert!(VisibilityCommand::Toggle.apply(false));
        assert!(VisibilityCommand::Show.apply(false));
        assert!(!VisibilityCommand::Hide.apply(true));
    }

    #[test]
    fn apply_reports_change_without_memory() {
        let mut visibility = PresentationVisibility::new(
            VisibilitySettings {
                visible: true,
                remember: true,
            },
            None,
        );
        assert!(visibility.visible());
        assert_eq!(visibility.apply(VisibilityCommand::Show), Ok(false));
        assert_eq!(visibility.apply(VisibilityCommand::Toggle), Ok(true));
        assert!(!visibility.visible());
    }

    #[test]
    fn remembered_state_overrides_configured_default_only_when_remembering() {
        let directory = scratch("remember");
        let mut first = PresentationVisibility::new(
            VisibilitySettings {
                visible: true,
                remember: true,
            },
            Some(directory.clone()),
        );
        assert!(first.visible());
        first.apply(VisibilityCommand::Hide).unwrap();

        let restored = PresentationVisibility::new(
            VisibilitySettings {
                visible: true,
                remember: true,
            },
            Some(directory.clone()),
        );
        assert!(!restored.visible());
        let forgetful = PresentationVisibility::new(
            VisibilitySettings {
                visible: true,
                remember: false,
            },
            Some(directory.clone()),
        );
        assert!(forgetful.visible());

        // A forgetful installation never writes, so the memory is untouched.
        let mut forgetful = forgetful;
        forgetful.apply(VisibilityCommand::Show).unwrap();
        assert!(
            !PresentationVisibility::new(
                VisibilitySettings {
                    visible: true,
                    remember: true
                },
                Some(directory.clone())
            )
            .visible()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn unreadable_memory_falls_back_to_configured_default() {
        let directory = scratch("corrupt");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(FILE_NAME), "sideways").unwrap();
        assert!(
            !PresentationVisibility::new(
                VisibilitySettings {
                    visible: false,
                    remember: true
                },
                Some(directory.clone())
            )
            .visible()
        );
        assert!(
            PresentationVisibility::new(
                VisibilitySettings {
                    visible: true,
                    remember: true
                },
                Some(directory.clone())
            )
            .visible()
        );
        let _ = std::fs::remove_dir_all(directory);
    }
}
