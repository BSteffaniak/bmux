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
use bmux_tui::presentation::PreparedPresentation;
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

/// Default prepared-snapshot configuration preserving existing constructors.
pub struct NoopPreparedConfiguration;

/// Caller-owned configuration applied before output, never a focus authority.
pub trait PreparedConfiguration<P> {
    /// Stage metadata and validate admission before the backend writes anything.
    ///
    /// # Errors
    /// Returns a rejected configuration or admission error.
    fn configure(&mut self, program: &mut P, prepared: &mut PreparedPresentation)
    -> io::Result<()>;
}

impl<P> PreparedConfiguration<P> for NoopPreparedConfiguration {
    fn configure(
        &mut self,
        _program: &mut P,
        _prepared: &mut PreparedPresentation,
    ) -> io::Result<()> {
        Ok(())
    }
}

impl<P, F> PreparedConfiguration<P> for F
where
    F: FnMut(&mut P, &mut PreparedPresentation) -> io::Result<()>,
{
    fn configure(
        &mut self,
        program: &mut P,
        prepared: &mut PreparedPresentation,
    ) -> io::Result<()> {
        self(program, prepared)
    }
}

/// Presenter that commits cell and terminal-image output through one flush.
pub struct ImageTerminalPresenter<W, R, C = NoopPresentationCommit, Q = NoopPreparedConfiguration> {
    terminal: Terminal<W>,
    render: R,
    commit: C,
    configure: Q,
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

impl<W: Write, R, C, Q> ImageTerminalPresenter<W, R, C, Q> {
    /// Refresh pixel geometry without reading terminal input or changing protocols.
    pub fn refresh_cell_metrics(&mut self) {
        let (width, height) = bmux_image::host_caps::query_cell_pixel_size();
        if width != 0 && height != 0 {
            self.host_capabilities.cell_pixel_width = width;
            self.host_capabilities.cell_pixel_height = height;
        }
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
            configure: NoopPreparedConfiguration,
            compositor: TuiImageCompositor::new(),
            host_capabilities,
            image_config,
        }
    }
}

impl<W: Write, R, C, Q> ImageTerminalPresenter<W, R, C, Q> {
    /// Install caller-owned prepared metadata configuration before output.
    /// The callback receives `(&mut P, &mut PreparedPresentation)` and returns
    /// `io::Result<()>`. Stage scopes first, then call `validate_limits` with
    /// the application's admission budget; rejection performs no output.
    #[must_use]
    pub fn configure_prepared<F>(self, configure: F) -> ImageTerminalPresenter<W, R, C, F> {
        ImageTerminalPresenter {
            terminal: self.terminal,
            render: self.render,
            commit: self.commit,
            configure,
            compositor: self.compositor,
            host_capabilities: self.host_capabilities,
            image_config: self.image_config,
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
        let mut candidate = self.compositor.clone();
        let result = candidate
            .cleanup(self.terminal.writer_mut(), &self.host_capabilities)
            .and_then(|()| self.terminal.writer_mut().flush());
        self.terminal.reset();
        if let Err(error) = result {
            self.compositor.invalidate_after_failed_output(&candidate);
            return Err(error.into());
        }
        self.compositor = candidate;
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

impl<P, W, R, Q> Presenter<P> for ImageTerminalPresenter<W, R, NoopPresentationCommit, Q>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    Q: PreparedConfiguration<P>,
{
    type Error = ImagePresentationError;

    fn resize(&mut self, size: Size) {
        self.refresh_cell_metrics();
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
            &mut self.configure,
            &mut self.compositor,
            &self.host_capabilities,
            &self.image_config,
            program,
        )
    }
}

impl<P, W, R, C, Q> Presenter<P> for ImageTerminalPresenter<W, R, C, Q>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    C: FnMut(&mut P, &HitMap, &FocusTrap),
    Q: PreparedConfiguration<P>,
{
    type Error = ImagePresentationError;

    fn resize(&mut self, size: Size) {
        self.refresh_cell_metrics();
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
            &mut self.configure,
            &mut self.compositor,
            &self.host_capabilities,
            &self.image_config,
            program,
        )?;
        (self.commit)(program, self.terminal.hits(), self.terminal.focus());
        Ok(report)
    }
}

fn present_image_frame<P, W, R, Q>(
    terminal: &mut Terminal<W>,
    render: &mut R,
    configure: &mut Q,
    compositor: &mut TuiImageCompositor,
    capabilities: &HostImageCapabilities,
    config: &ImageConfig,
    program: &mut P,
) -> Result<PresentReport, ImagePresentationError>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    Q: PreparedConfiguration<P>,
{
    let terminal_area = terminal.area();
    let pane = PaneRect {
        x: terminal_area.x,
        y: terminal_area.y,
        w: terminal_area.width,
        h: terminal_area.height,
    };
    let staged_compositor = std::cell::RefCell::new(compositor.clone());
    let program = std::cell::RefCell::new(program);
    let stats = terminal.draw_damage_with_prepared_output_hooks(
        bmux_tui::damage::Damage::Full,
        |cx| render(&mut program.borrow_mut(), cx),
        |prepared| configure.configure(&mut program.borrow_mut(), prepared),
        |writer| staged_compositor.borrow_mut().prepare_output(writer),
        |writer, scene, delta| {
            let mut staged_compositor = staged_compositor.borrow_mut();
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
    );
    let stats = match stats {
        Ok(stats) => stats,
        Err(error) => {
            compositor.invalidate_after_failed_output(&staged_compositor.borrow());
            return Err(error.into());
        }
    };
    *compositor = staged_compositor.into_inner();
    Ok(PresentReport {
        changed_cells: stats.changed_cells,
        full_repaint: stats.full_repaint,
    })
}

#[cfg(test)]
mod prepared_tests {
    use super::ImageTerminalPresenter;
    use crate::presenter::Presenter;
    use bmux_tui::focus::{FocusScopeId, FocusTrap};
    use bmux_tui::geometry::Rect;
    use bmux_tui::hit::{HitMap, HitRegion};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::presentation::{PreparedPresentation, PresentationLimits};
    use bmux_tui::terminal::Terminal;
    use std::io::{self, Write};

    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        fail_flush: bool,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }
    #[derive(Default)]
    struct Program {
        scopes: Vec<FocusScopeId>,
        commits: usize,
        reject: bool,
    }
    fn render(program: &mut Program, cx: &mut PaintCx<'_, '_>) {
        cx.push_hit(HitRegion::new("background", Rect::new(0, 0, 4, 2)).focusable(true));
        for scope in &program.scopes {
            cx.set_focus_scope(Some(scope.clone()));
            cx.push_hit(HitRegion::new(scope.clone(), Rect::new(1, 0, 2, 1)).focusable(true));
        }
    }
    fn configure(program: &mut Program, prepared: &mut PreparedPresentation) -> io::Result<()> {
        if program.reject || !prepared.set_modal_scopes(&program.scopes) {
            return Err(io::Error::other("configuration rejected"));
        }
        prepared
            .validate_limits(PresentationLimits {
                max_cells: 8,
                max_entries: 128,
                max_bytes: 8192,
            })
            .map_err(io::Error::other)
    }
    fn commit(program: &mut Program, hits: &HitMap, focus: &FocusTrap) {
        program.commits += 1;
        assert_eq!(focus.active_scope(), program.scopes.last());
        if let Some(scope) = program.scopes.last() {
            assert_eq!(focus.active(), Some(scope));
            assert!(hits.focus_targets(None).is_empty());
        }
    }
    #[test]
    fn configured_scopes_commit_only_after_successful_flush() {
        let mut presenter = ImageTerminalPresenter::with_commit(
            Terminal::new(Writer::default(), Rect::new(0, 0, 4, 2)),
            render,
            commit,
            bmux_image::HostImageCapabilities::default(),
            bmux_image::ImageConfig::default(),
        )
        .configure_prepared(configure);
        let mut program = Program::default();
        presenter.present(&mut program).unwrap();
        program.scopes.push(FocusScopeId::new("modal"));
        presenter.terminal_mut().writer_mut().fail_flush = true;
        assert!(presenter.present(&mut program).is_err());
        assert_eq!(program.commits, 1);
        assert!(presenter.terminal().focus().active_scope().is_none());
        presenter.terminal_mut().writer_mut().fail_flush = false;
        presenter.present(&mut program).unwrap();
        assert_eq!(program.commits, 2);
        let written = presenter.terminal().writer().bytes.len();
        program.reject = true;
        assert!(presenter.present(&mut program).is_err());
        assert_eq!(presenter.terminal().writer().bytes.len(), written);
        assert_eq!(program.commits, 2);
        assert_eq!(
            presenter.terminal().focus().active_scope(),
            program.scopes.last()
        );
    }
    #[test]
    fn prepared_configuration_also_supports_noop_commit_constructor() {
        let mut presenter = ImageTerminalPresenter::new(
            Terminal::new(Writer::default(), Rect::new(0, 0, 4, 2)),
            render,
            bmux_image::HostImageCapabilities::default(),
            bmux_image::ImageConfig::default(),
        )
        .configure_prepared(configure);
        let mut program = Program {
            scopes: vec![FocusScopeId::new("modal")],
            ..Program::default()
        };
        presenter.present(&mut program).unwrap();
        assert_eq!(
            presenter.terminal().focus().active_scope(),
            program.scopes.last()
        );
    }
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

    #[cfg(all(feature = "image-sixel", feature = "image-iterm2"))]
    #[test]
    fn raster_lifecycle_clears_before_complete_repaint() {
        use bmux_image::HostImageCapabilities;
        use bmux_tui::paint::PaintCx;
        use bmux_tui::terminal::Terminal;
        for caps in [
            HostImageCapabilities {
                sixel: true,
                ..Default::default()
            },
            HostImageCapabilities {
                iterm2_inline: true,
                ..Default::default()
            },
        ] {
            let terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
            let render = |visible: &mut bool, frame: &mut PaintCx<'_, '_>| {
                if *visible {
                    frame.push_image(ImageContribution::Present(ImagePlacement {
                        fit: bmux_tui::image::ImageFit::Stretch,
                        allocation: None,
                        key: ImageKey::new("raster"),
                        payload: ImagePayload::Pixels {
                            bytes: vec![255, 0, 0, 255],
                            width: 1,
                            height: 1,
                            format: ImagePixelFormat::Rgba8,
                        },
                        destination: Rect::new(0, 0, 1, 1),
                        clip: frame.clip(),
                        lifecycle: ImageLifecycle::Frame,
                    }));
                }
            };
            let mut presenter = ImageTerminalPresenter::new(
                terminal,
                render,
                caps,
                bmux_image::ImageConfig::default(),
            );
            let mut visible = true;
            presenter.present(&mut visible).unwrap();
            for next in [true, false, true, false, true] {
                let before = presenter.terminal().writer().len();
                let had_image = visible;
                visible = next;
                let report = presenter.present(&mut visible).unwrap();
                let output = &presenter.terminal().writer()[before..];
                assert_eq!(
                    output.windows(4).any(|bytes| bytes == b"\x1b[2J"),
                    had_image
                );
                if had_image {
                    assert_eq!(report.changed_cells, 8);
                }
            }
            let before = presenter.terminal().writer().len();
            presenter.cleanup_images().unwrap();
            assert!(
                presenter.terminal().writer()[before..]
                    .windows(4)
                    .any(|bytes| bytes == b"\x1b[2J")
            );
        }
    }

    #[test]
    fn reset_resize_and_cleanup_cover_image_lifecycle_handoffs() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
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
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
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
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
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
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
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
