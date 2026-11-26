//! The attach client's connection banner: one row saying the connection to
//! the server is degraded, lost, or stalled, shown until that is resolved
//! (unlike a status message, which expires).
//!
//! Every state is declared once in [`ConnectionBanner::spec`]: its severity
//! (which sets its colors) and the keys it answers. A banner with keys is
//! *modal*: while it shows, keys go to it, never to a pane. Only states with
//! nothing to send input to are modal (the connection is lost, or the view is
//! known not to match the server and is no longer resynced).
//!
//! The banner reaches the screen two ways: as an attach-local overlay surface
//! when a frame is rendered ([`super::local_presentation::publish_banner`]),
//! or painted straight over whatever the terminal shows ([`paint`]) when no
//! frame can be rendered (no layout, or no connection to fetch one with).

use bmux_plugin::RenderStyle;
use bmux_terminal::cursor::{MoveTo, RestorePosition, SavePosition};
use bmux_terminal::queue;
use bmux_terminal::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use std::io::{self, Write};
use std::time::Duration;

/// What the connection banner says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionBanner {
    /// Requests keep timing out; the connection is still up.
    Degraded,
    /// The connection was lost; attempt `attempt` reconnects in `next_in`
    /// (`None`: it is running now).
    Reconnecting {
        attempt: u32,
        next_in: Option<Duration>,
    },
    /// The server refused to resync the view too many times in a row; no more
    /// are tried until the user asks.
    Stalled { reason: String },
    /// The connection came back, but the attached session no longer exists.
    Gone,
}

/// How serious a banner is; sets its colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerSeverity {
    Warning,
    Error,
}

impl BannerSeverity {
    /// Foreground and background palette indices, the toast stack's severity
    /// colors (yellow warning, red error) as a filled row.
    const fn palette(self) -> (u8, u8) {
        match self {
            Self::Warning => (0, 3),
            Self::Error => (15, 1),
        }
    }

    /// The overlay style of a banner of this severity.
    #[must_use]
    pub const fn style(self) -> RenderStyle {
        let (fg, bg) = self.palette();
        RenderStyle::new()
            .indexed_foreground(fg)
            .indexed_background(bg)
            .bold()
    }
}

/// What a key pressed while a modal banner shows does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerAction {
    /// Retry what stalled now.
    Retry,
    /// Leave the attach client.
    Quit,
    /// Leave the attach client without telling the server (it cannot be
    /// reached); the session keeps running there.
    DetachLocal,
}

/// One key a modal banner answers.
struct BannerKey {
    action: BannerAction,
    key: char,
    hint: &'static str,
}

const BANNER_KEYS: [BannerKey; 3] = [
    BannerKey {
        action: BannerAction::Retry,
        key: 'r',
        hint: "press r to retry",
    },
    BannerKey {
        action: BannerAction::Quit,
        key: 'q',
        hint: "q quit",
    },
    BannerKey {
        action: BannerAction::DetachLocal,
        key: 'd',
        hint: "d detach-local",
    },
];

/// How a banner state looks and which keys it answers.
struct BannerSpec {
    severity: BannerSeverity,
    actions: &'static [BannerAction],
}

impl ConnectionBanner {
    const fn spec(&self) -> BannerSpec {
        match self {
            Self::Degraded => BannerSpec {
                severity: BannerSeverity::Warning,
                actions: &[],
            },
            Self::Reconnecting { .. } => BannerSpec {
                severity: BannerSeverity::Error,
                actions: &[BannerAction::Quit, BannerAction::DetachLocal],
            },
            Self::Stalled { .. } => BannerSpec {
                severity: BannerSeverity::Error,
                actions: &[
                    BannerAction::Retry,
                    BannerAction::Quit,
                    BannerAction::DetachLocal,
                ],
            },
            Self::Gone => BannerSpec {
                severity: BannerSeverity::Error,
                actions: &[],
            },
        }
    }

    #[must_use]
    pub const fn severity(&self) -> BannerSeverity {
        self.spec().severity
    }

    /// Whether keys go to this banner instead of a pane while it shows.
    #[must_use]
    pub const fn modal(&self) -> bool {
        !self.spec().actions.is_empty()
    }

    /// What pressing `key` does while this banner shows: `None` for a key it
    /// does not answer (a modal banner drops it).
    #[must_use]
    pub fn action_for(&self, key: char) -> Option<BannerAction> {
        BANNER_KEYS
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.action)
            .filter(|action| self.spec().actions.contains(action))
    }

    /// The banner's text, with the hints for the keys it answers.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = match self {
            Self::Degraded => "server not responding — retrying…".to_owned(),
            Self::Reconnecting { attempt, next_in } => next_in.map_or_else(
                || format!("connection lost — reconnecting (attempt {attempt}…)"),
                |next_in| {
                    format!(
                        "connection lost — reconnecting (attempt {attempt}, next in {}s)",
                        next_in.as_millis().div_ceil(1000).max(1)
                    )
                },
            ),
            Self::Stalled { reason } => format!("view out of sync with the server: {reason}"),
            Self::Gone => GONE_MESSAGE.to_owned(),
        };
        for action in self.spec().actions {
            if let Some(entry) = BANNER_KEYS.iter().find(|entry| entry.action == *action) {
                text.push_str(" · ");
                text.push_str(entry.hint);
            }
        }
        text
    }
}

/// What the attach client says when it ends because its session is gone.
pub const GONE_MESSAGE: &str = "session no longer exists — attach ended";

/// `text` cut or padded to exactly `cols` cells (one per character: the
/// banner's own text is narrow).
fn fit(text: &str, cols: u16) -> String {
    let cols = usize::from(cols);
    let mut row: String = text.chars().take(cols).collect();
    let used = row.chars().count();
    row.extend(std::iter::repeat_n(' ', cols.saturating_sub(used)));
    row
}

/// The banner's text as one row of `cols` cells: what is painted, and what
/// the overlay surface draws.
#[must_use]
pub fn row_text(banner: &ConnectionBanner, cols: u16) -> String {
    fit(&format!(" {}", banner.text()), cols)
}

/// Paint `banner` on row `row` straight over what the terminal shows, leaving
/// the cursor where it was.
///
/// # Errors
/// Returns the terminal write failure.
pub fn paint(
    output: &mut dyn Write,
    banner: &ConnectionBanner,
    row: u16,
    cols: u16,
) -> io::Result<()> {
    paint_row(
        output,
        row,
        &row_text(banner, cols),
        Some(banner.severity().palette()),
    )
}

/// Paint `text` (already fitted to the row) on row `row` straight over what
/// the terminal shows, bold, in `palette` colors (default colors without),
/// leaving the cursor where it was.
///
/// # Errors
/// Returns the terminal write failure.
pub fn paint_row(
    output: &mut dyn Write,
    row: u16,
    text: &str,
    palette: Option<(u8, u8)>,
) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut output = output;
    queue!(
        &mut output,
        SavePosition,
        MoveTo(0, row),
        SetAttribute(Attribute::Reset)
    )?;
    if let Some((fg, bg)) = palette {
        queue!(
            &mut output,
            SetForegroundColor(Color::AnsiValue(fg)),
            SetBackgroundColor(Color::AnsiValue(bg)),
        )?;
    }
    queue!(
        &mut output,
        SetAttribute(Attribute::Bold),
        Print(text),
        SetAttribute(Attribute::Reset),
        RestorePosition,
    )?;
    output.flush()
}

/// `text` as one row of `cols` cells (cut or padded).
#[must_use]
pub fn fit_row(text: &str, cols: u16) -> String {
    fit(text, cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_banners_are_the_ones_with_nothing_to_send_input_to() {
        assert!(!ConnectionBanner::Degraded.modal());
        assert!(!ConnectionBanner::Gone.modal());
        let reconnecting = ConnectionBanner::Reconnecting {
            attempt: 1,
            next_in: None,
        };
        assert!(reconnecting.modal());
        assert_eq!(reconnecting.action_for('q'), Some(BannerAction::Quit));
        assert_eq!(
            reconnecting.action_for('d'),
            Some(BannerAction::DetachLocal)
        );
        assert_eq!(reconnecting.action_for('r'), None);
        assert_eq!(reconnecting.action_for('x'), None);
        let stalled = ConnectionBanner::Stalled {
            reason: "denied".into(),
        };
        assert_eq!(stalled.action_for('r'), Some(BannerAction::Retry));
    }

    #[test]
    fn texts_name_the_state_and_the_keys_it_answers() {
        assert_eq!(
            ConnectionBanner::Degraded.text(),
            "server not responding — retrying…"
        );
        assert_eq!(
            ConnectionBanner::Reconnecting {
                attempt: 3,
                next_in: Some(Duration::from_millis(2_300)),
            }
            .text(),
            "connection lost — reconnecting (attempt 3, next in 3s) · q quit · d detach-local"
        );
        assert_eq!(
            ConnectionBanner::Stalled {
                reason: "resync failed: denied".into()
            }
            .text(),
            "view out of sync with the server: resync failed: denied · press r to retry · q quit · d detach-local"
        );
    }

    #[test]
    fn painting_fills_one_row_and_keeps_the_cursor() {
        let mut out = Vec::new();
        paint(&mut out, &ConnectionBanner::Degraded, 0, 10).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b7"), "{text:?}");
        assert!(text.ends_with("\x1b8"), "{text:?}");
        assert!(text.contains(" server no"), "{text:?}");
        assert_eq!(
            row_text(&ConnectionBanner::Degraded, 50).chars().count(),
            50
        );
    }
}
