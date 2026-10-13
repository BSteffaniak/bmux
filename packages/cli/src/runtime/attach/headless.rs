//! Output-only attach lifecycle and bounded external viewport updates.
use anyhow::{Result, bail};
use crossterm::event::Event;
use tokio::sync::watch;

use super::input::TerminalGeometry;
use super::runtime::{AttachTerminal, AttachTerminalEventFuture};

type ViewportGuard = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;
#[derive(Clone)]
struct ViewportUpdate {
    geometry: TerminalGeometry,
    guard: Option<ViewportGuard>,
    closed: bool,
}

/// A latest-only viewport sender. It cannot inject terminal input.
#[derive(Clone)]
pub struct HeadlessViewportSender(watch::Sender<ViewportUpdate>);

impl HeadlessViewportSender {
    /// Request orderly detach even while other sender clones are retained.
    pub fn close(&self) {
        self.0.send_modify(|update| update.closed = true);
    }
    /// Update the bounded latest viewport.
    ///
    /// # Errors
    /// Returns an error for invalid geometry or a closed view.
    #[cfg(test)]
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.resize_if(cols, rows, || true)
    }
    /// The guard is checked on consumption and immediately before native sizing.
    ///
    /// # Errors
    /// Returns an error for invalid geometry or a closed view.
    pub fn resize_if(
        &self,
        cols: u16,
        rows: u16,
        guard: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<()> {
        validate(TerminalGeometry { cols, rows })?;
        if self.0.borrow().closed {
            bail!("headless viewport channel closed");
        }
        self.0.send(ViewportUpdate {
            geometry: TerminalGeometry { cols, rows },
            guard: Some(std::sync::Arc::new(guard)),
            closed: false,
        })?;
        Ok(())
    }
}

/// Per-view output-only lifecycle; deliberately has no terminal resources.
pub struct HeadlessAttachDriver {
    viewport: watch::Receiver<ViewportUpdate>,
    accepted: TerminalGeometry,
    guard: std::sync::Arc<std::sync::Mutex<Option<ViewportGuard>>>,
}

/// Create an output-only view with a bounded initial cell viewport.
///
/// # Errors
/// Returns an error for an empty or over-budget viewport.
pub fn headless_attach_channel(
    initial: TerminalGeometry,
) -> Result<(HeadlessAttachDriver, HeadlessViewportSender)> {
    validate(initial)?;
    let (sender, viewport) = watch::channel(ViewportUpdate {
        geometry: initial,
        guard: None,
        closed: false,
    });
    Ok((
        HeadlessAttachDriver {
            viewport,
            accepted: initial,
            guard: std::sync::Arc::default(),
        },
        HeadlessViewportSender(sender),
    ))
}

fn validate(geometry: TerminalGeometry) -> Result<()> {
    if geometry.cols == 0
        || geometry.rows == 0
        || usize::from(geometry.cols) * usize::from(geometry.rows) > 262_144
    {
        bail!("headless viewport is empty or exceeds cell budget");
    }
    Ok(())
}

// This legacy lifecycle trait still requires Write. Headless composition never
// calls it: reject accidental terminal encoding rather than swallow fake stdout.
impl std::io::Write for HeadlessAttachDriver {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(
            "terminal output forbidden in headless attach",
        ))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "terminal flush forbidden in headless attach",
        ))
    }
}

impl AttachTerminal for HeadlessAttachDriver {
    fn presentation_only(&self) -> bool {
        true
    }
    fn geometry(&self) -> TerminalGeometry {
        self.accepted
    }
    fn viewport_guard(&self) -> Option<ViewportGuard> {
        let current = self.guard.clone();
        Some(std::sync::Arc::new(move || {
            current
                .lock()
                .is_ok_and(|guard| guard.as_ref().is_none_or(|guard| guard()))
        }))
    }
    fn enter_attach_mode(&mut self, _: bool, _: bool, _: bool) -> Result<bool> {
        Ok(false)
    }
    fn restore_after_attach_ui(&mut self) -> Result<()> {
        Ok(())
    }
    fn next_event(&mut self) -> AttachTerminalEventFuture<'_> {
        Box::pin(async move {
            loop {
                // A select! may cancel this future after a viewport update was
                // consumed. Closure is persistent state, not a one-shot event.
                if self.viewport.borrow().closed {
                    return Ok(None);
                }
                if self.viewport.changed().await.is_err() {
                    return Ok(None);
                }
                let update = self.viewport.borrow_and_update().clone();
                if update.closed {
                    return Ok(None);
                }
                if update.guard.as_ref().is_some_and(|guard| !guard()) {
                    continue;
                }
                self.accepted = update.geometry;
                *self
                    .guard
                    .lock()
                    .map_err(|_| anyhow::anyhow!("viewport guard poisoned"))? = update.guard;
                return Ok(Some(Event::Resize(self.accepted.cols, self.accepted.rows)));
            }
        })
    }
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    fn detect_image_capabilities(&mut self) -> bmux_image::host_caps::HostImageCapabilities {
        bmux_image::host_caps::HostImageCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime::AttachTerminal;
    use super::*;

    #[tokio::test]
    async fn viewport_is_latest_only_guarded_and_orderly_closed() {
        let (mut driver, sender) =
            headless_attach_channel(TerminalGeometry { cols: 80, rows: 24 }).unwrap();
        sender.resize(90, 25).unwrap();
        sender.resize(100, 30).unwrap();
        assert_eq!(
            driver.next_event().await.unwrap(),
            Some(Event::Resize(100, 30))
        );
        assert_eq!(driver.geometry().cols, 100);
        sender.resize_if(110, 31, || false).unwrap();
        sender.close();
        assert!(driver.next_event().await.unwrap().is_none());
        // Closure survives consumption/cancellation by the surrounding select!.
        assert!(driver.next_event().await.unwrap().is_none());
        assert_eq!(driver.geometry().cols, 100);
        assert!(sender.resize(80, 24).is_err());
    }

    #[test]
    fn rejects_invalid_viewport_and_terminal_output() {
        assert!(headless_attach_channel(TerminalGeometry { cols: 0, rows: 24 }).is_err());
        let (mut driver, _) =
            headless_attach_channel(TerminalGeometry { cols: 80, rows: 24 }).unwrap();
        assert!(std::io::Write::write(&mut driver, b"ANSI").is_err());
        assert!(driver.enter_attach_mode(true, true, true).is_ok());
        assert!(driver.restore_after_attach_ui().is_ok());
    }
}
