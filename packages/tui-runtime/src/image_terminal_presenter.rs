//! Image-capable [`bmux_tui::terminal::Terminal`] presenter adapter.

use std::io::{self, Write};

use bmux_image::ImageConfig;
use bmux_image::compositor::PaneRect;
use bmux_image::host_caps::HostImageCapabilities;
use bmux_image::tui::{TuiImageCompositor, TuiImageError};
use bmux_tui::focus::FocusTrap;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::SelectionScene;
use bmux_tui::terminal::Terminal;

use crate::presenter::{PresentReport, Presenter, ResetReason};
use crate::terminal_presenter::NoopPresentationCommit;

/// Presentation error produced by an image-capable terminal presenter.
#[derive(Debug)]
pub enum ImagePresentationError {
    /// Cell or terminal output failed.
    Io(io::Error),
    /// A protocol-neutral image payload was invalid.
    Image(TuiImageError),
}

impl std::fmt::Display for ImagePresentationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Image(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ImagePresentationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Image(error) => Some(error),
        }
    }
}

impl From<io::Error> for ImagePresentationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Presenter that commits cell and terminal-image output through one flush.
pub struct ImageTerminalPresenter<W, R, C = NoopPresentationCommit> {
    terminal: Terminal<W>,
    render: R,
    commit: C,
    compositor: TuiImageCompositor,
    host_capabilities: HostImageCapabilities,
    image_config: ImageConfig,
}

impl<W: Write, R> ImageTerminalPresenter<W, R, NoopPresentationCommit> {
    /// Create an image-capable presenter using safe environment-only capability detection.
    ///
    /// This constructor performs no terminal I/O and therefore cannot consume
    /// input intended for the runtime. Callers that need active terminal
    /// queries should detect capabilities while raw mode is active, before
    /// starting runtime input admission, and pass them to [`Self::new`].
    #[must_use]
    pub fn detect(terminal: Terminal<W>, render: R, image_config: ImageConfig) -> Self {
        Self::new(
            terminal,
            render,
            bmux_image::host_caps::detect_from_env(),
            image_config,
        )
    }

    /// Create an image-capable presenter with pre-detected capabilities.
    #[must_use]
    pub fn new(
        terminal: Terminal<W>,
        render: R,
        host_capabilities: HostImageCapabilities,
        image_config: ImageConfig,
    ) -> Self {
        Self::with_commit(
            terminal,
            render,
            NoopPresentationCommit,
            host_capabilities,
            image_config,
        )
    }
}

impl<W: Write, R, C> ImageTerminalPresenter<W, R, C> {
    /// Create an image-capable presenter with a successful-commit observer.
    #[must_use]
    pub fn with_commit(
        terminal: Terminal<W>,
        render: R,
        commit: C,
        host_capabilities: HostImageCapabilities,
        image_config: ImageConfig,
    ) -> Self {
        Self {
            terminal,
            render,
            commit,
            compositor: TuiImageCompositor::new(),
            host_capabilities,
            image_config,
        }
    }

    /// Return the underlying terminal.
    #[must_use]
    pub const fn terminal(&self) -> &Terminal<W> {
        &self.terminal
    }

    /// Return the underlying terminal mutably.
    pub const fn terminal_mut(&mut self) -> &mut Terminal<W> {
        &mut self.terminal
    }

    /// Return the last committed interaction scene.
    #[must_use]
    pub const fn interactions(&self) -> &HitMap {
        self.terminal.hits()
    }

    /// Return the last committed selection scene.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        self.terminal.selection()
    }

    /// Return the last committed focus state.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        self.terminal.focus()
    }

    /// Return detected host image capabilities.
    #[must_use]
    pub const fn host_capabilities(&self) -> &HostImageCapabilities {
        &self.host_capabilities
    }

    /// Reset retained terminal and image presentation state.
    ///
    /// Existing host images are removed before the next frame is presented.
    /// If cleanup output fails, the presenter retains the pending removals and
    /// retries them during the next presentation.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when protocol cleanup cannot be written or flushed.
    pub fn reset_presentation(&mut self) -> Result<(), ImagePresentationError> {
        self.cleanup_images()?;
        self.terminal.reset();
        Ok(())
    }

    /// Remove all committed terminal images from the host.
    ///
    /// Call this before returning terminal ownership, including graceful exit,
    /// suspension, and recovery from an application error.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when protocol cleanup cannot be written or flushed.
    pub fn cleanup_images(&mut self) -> Result<(), ImagePresentationError> {
        self.compositor
            .cleanup(self.terminal.writer_mut(), &self.host_capabilities)?;
        self.terminal.writer_mut().flush()?;
        self.terminal.reset();
        Ok(())
    }

    /// Consume the presenter after cleaning host image resources and return its terminal.
    ///
    /// # Errors
    ///
    /// Returns an error when protocol cleanup cannot be written or flushed.
    pub fn into_clean_terminal(mut self) -> Result<Terminal<W>, ImagePresentationError> {
        self.cleanup_images()?;
        Ok(self.terminal)
    }
}

impl<P, W, R> Presenter<P> for ImageTerminalPresenter<W, R, NoopPresentationCommit>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
{
    type Error = ImagePresentationError;

    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.compositor.clear();
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        present_image_frame(
            &mut self.terminal,
            &mut self.render,
            &mut self.compositor,
            &self.host_capabilities,
            &self.image_config,
            program,
        )
    }
}

impl<P, W, R, C> Presenter<P> for ImageTerminalPresenter<W, R, C>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    C: FnMut(&mut P, &HitMap, &FocusTrap),
{
    type Error = ImagePresentationError;

    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.compositor.clear();
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        let report = present_image_frame(
            &mut self.terminal,
            &mut self.render,
            &mut self.compositor,
            &self.host_capabilities,
            &self.image_config,
            program,
        )?;
        (self.commit)(program, self.terminal.hits(), self.terminal.focus());
        Ok(report)
    }
}

fn present_image_frame<P, W, R>(
    terminal: &mut Terminal<W>,
    render: &mut R,
    compositor: &mut TuiImageCompositor,
    capabilities: &HostImageCapabilities,
    config: &ImageConfig,
    program: &mut P,
) -> Result<PresentReport, ImagePresentationError>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
{
    let terminal_area = terminal.area();
    let pane = PaneRect {
        x: terminal_area.x,
        y: terminal_area.y,
        w: terminal_area.width,
        h: terminal_area.height,
    };
    let mut staged_compositor = compositor.clone();
    let stats = terminal.draw_with_overlay(
        |cx| render(program, cx),
        |writer, scene, delta| {
            staged_compositor.apply_delta(delta);
            staged_compositor
                .render(writer, scene, pane, capabilities, config)
                .map_err(|error| match error {
                    TuiImageError::Io(error) => error,
                    error @ TuiImageError::InvalidPayload { .. } => {
                        io::Error::new(io::ErrorKind::InvalidData, error)
                    }
                })
        },
    )?;
    *compositor = staged_compositor;
    Ok(PresentReport {
        changed_cells: stats.changed_cells,
        full_repaint: stats.full_repaint,
    })
}

#[cfg(all(test, feature = "image-kitty"))]
mod tests {
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };

    use super::ImageTerminalPresenter;
    use crate::presenter::{Presenter, ResetReason};

    fn capabilities() -> bmux_image::HostImageCapabilities {
        bmux_image::HostImageCapabilities {
            kitty_graphics: true,
            ..bmux_image::HostImageCapabilities::default()
        }
    }

    #[test]
    fn reset_resize_and_cleanup_cover_image_lifecycle_handoffs() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");
        presenter.resize(bmux_tui::geometry::Size::new(8, 4));
        presenter.reset(ResetReason::Resize);
        presenter.present(&mut ()).expect("resize presentation");
        let before = presenter.terminal().writer().len();

        presenter.reset_presentation().expect("full reset cleanup");

        let output = String::from_utf8(presenter.terminal().writer()[before..].to_vec())
            .expect("terminal output");
        assert!(output.contains("a=d"));
        assert_eq!(presenter.terminal().area(), Rect::new(0, 0, 8, 4));
    }

    #[test]
    fn consuming_presenter_cleans_images_before_returning_terminal() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");

        let terminal = presenter.into_clean_terminal().expect("clean handoff");
        let output = String::from_utf8(terminal.writer().clone()).expect("terminal output");
        assert!(output.contains("a=d"));
    }

    #[test]
    fn cleanup_removes_host_images_before_terminal_handoff() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");
        let before = presenter.terminal().writer().len();

        presenter.cleanup_images().expect("image cleanup");

        let output = String::from_utf8(presenter.terminal().writer()[before..].to_vec())
            .expect("terminal output");
        assert!(output.contains("a=d"));
    }

    #[test]
    fn reset_removes_old_host_image_before_retransmission() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");
        let before = presenter.terminal().writer().len();

        presenter.reset(ResetReason::Application);
        presenter.present(&mut ()).expect("reset presentation");

        let output = String::from_utf8(presenter.terminal().writer()[before..].to_vec())
            .expect("terminal output");
        assert!(output.contains("a=d"));
        assert!(output.contains("a=t"));
    }
}
