//! Winit is an implementation detail of this module. Window lifetime is main-thread owned.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::{LogicalSize, PhysicalRect, PhysicalSize, WindowMetrics};

/// Native window creation parameters. Sizes are logical pixels, never terminal cells.
#[derive(Clone, Debug)]
pub struct WindowOptions {
    pub title: String,
    pub initial_size: LogicalSize,
    /// Whether to request native titlebar/chrome (default: `true`).
    /// Applied only when creating the window; changing configuration does not
    /// update an existing window. This does not disable resizing or focus.
    pub decorations: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "Bterm".into(),
            decorations: true,
            initial_size: LogicalSize {
                width: 960.0,
                height: 640.0,
            },
        }
    }
}

impl WindowOptions {
    fn attributes(&self) -> winit::window::WindowAttributes {
        Window::default_attributes()
            // AccessKit must attach before the native window becomes visible.
            .with_visible(false)
            .with_title(&self.title)
            .with_decorations(self.decorations)
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.initial_size.width,
                self.initial_size.height,
            ))
    }
}

/// Owned lifecycle notifications. None acknowledge successful presentation.
#[derive(Clone, Debug, PartialEq)]
pub enum PlatformEvent {
    Resumed,
    Suspended,
    Resized(WindowMetrics),
    RedrawRequested,
    Focused(bool),
    CloseRequested,
    /// An OS-provided Paste action, not a synthesized shortcut binding.
    PasteRequested,
    Wake,
    Input(crate::input::InputEvent),
}

/// Host/consumer errors stop the loop rather than pretending a frame was displayed.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("native platform is unsupported on this operating system")]
    Unsupported,
    #[error("invalid initial logical window size")]
    InvalidSize,
    #[error("native event loop: {0}")]
    EventLoop(String),
    #[error("native window creation: {0}")]
    Window(String),
    #[error("native application: {0}")]
    Application(String),
}

#[derive(Default)]
struct WakeGate(AtomicBool);

impl WakeGate {
    fn try_queue(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }
    fn clear(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Thread-safe wake-only handle; carries no unbounded application payload queue.
#[derive(Clone)]
pub struct PlatformProxy {
    proxy: EventLoopProxy<()>,
    pending: Arc<WakeGate>,
}

impl PlatformProxy {
    pub fn wake(&self) -> Result<(), PlatformError> {
        if !self.pending.try_queue() {
            return Ok(());
        }
        self.proxy.send_event(()).map_err(|err| {
            self.pending.clear();
            PlatformError::EventLoop(err.to_string())
        })
    }
}

/// Opaque window lifetime token for the renderer's native surface adapter.
/// A token keeps its window alive, but does not authorize presentation after suspend.
#[derive(Clone)]
pub struct SurfaceTarget {
    // Retain the native window even when the renderer adapter feature is disabled.
    _window: Arc<Window>,
}

#[cfg(feature = "renderer-adapter")]
impl raw_window_handle::HasWindowHandle for SurfaceTarget {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        self._window.window_handle()
    }
}

#[cfg(feature = "renderer-adapter")]
impl raw_window_handle::HasDisplayHandle for SurfaceTarget {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        self._window.display_handle()
    }
}

/// Application callbacks execute serially on the event-loop thread. Consumers own
/// retained presentation, redraw scheduling, and successful-output acknowledgement.
pub trait WindowApplication {
    /// Keep the main window hidden until native initialization is ready.
    /// Do not require successful presentation: hidden surfaces may not acquire
    /// drawables. Visibility does not acknowledge output or grant scene authority.
    /// The default preserves immediate visibility for other native consumers.
    fn ready_to_show(&self) -> bool {
        true
    }

    fn event(
        &mut self,
        context: &mut WindowContext<'_>,
        event: PlatformEvent,
    ) -> Result<(), PlatformError>;
}

/// Borrowed access to the current live window. No layout or resource management APIs.
pub struct WindowContext<'a> {
    window: &'a Arc<Window>,
    event_loop: &'a ActiveEventLoop,
    proxy: &'a PlatformProxy,
    accessibility: &'a mut crate::accessibility::AccessibilityAdapter,
}

impl WindowContext<'_> {
    /// Publish semantics only for the retained frame that successfully reached output.
    pub fn commit_accessibility(
        &mut self,
        generation: u64,
        scene: &bmux_tui::semantic::SemanticScene,
        geometry: crate::accessibility::AccessibilityGeometry,
    ) -> Result<(), crate::accessibility::AccessibilityError> {
        self.accessibility.commit(generation, scene, geometry)
    }

    pub fn invalidate_accessibility(&mut self) {
        self.accessibility.invalidate();
    }

    pub fn take_accessibility_actions(
        &mut self,
    ) -> Result<
        Vec<crate::accessibility::AccessibilityAction>,
        crate::accessibility::AccessibilityError,
    > {
        self.accessibility.take_actions()
    }

    /// Current monitor's nominal refresh in millihertz, if the platform reports it.
    /// This is not measured presentation feedback and may differ under variable refresh.
    #[must_use]
    pub fn nominal_refresh_millihertz(&self) -> Option<u32> {
        self.window.current_monitor()?.refresh_rate_millihertz()
    }

    #[must_use]
    pub fn metrics(&self) -> WindowMetrics {
        metrics(self.window)
    }

    #[must_use]
    pub fn surface_target(&self) -> SurfaceTarget {
        SurfaceTarget {
            _window: Arc::clone(self.window),
        }
    }

    #[must_use]
    pub fn proxy(&self) -> PlatformProxy {
        self.proxy.clone()
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }
    pub fn exit(&self) {
        self.event_loop.exit();
    }
    /// Apply pointer intent only after the corresponding presentation succeeds.
    /// None hides the pointer; Some restores visibility and sets its shape.
    pub fn set_pointer_icon(&self, icon: Option<winit::window::CursorIcon>) {
        self.window.set_cursor_visible(icon.is_some());
        if let Some(icon) = icon {
            self.window.set_cursor(icon);
        }
    }

    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }
    pub fn set_ime_enabled(&self, enabled: bool) {
        self.window.set_ime_allowed(enabled);
    }
    pub fn set_ime_cursor_area(&self, rect: PhysicalRect) {
        self.window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(rect.x, rect.y),
            winit::dpi::PhysicalSize::new(rect.width, rect.height),
        );
    }
}

fn is_paste_action(key: &winit::keyboard::Key) -> bool {
    matches!(
        key,
        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Paste)
    )
}

fn metrics(window: &Window) -> WindowMetrics {
    let size = window.inner_size();
    WindowMetrics {
        physical_size: PhysicalSize {
            width: size.width,
            height: size.height,
        },
        scale_factor: window.scale_factor(),
    }
}

struct Host<A> {
    options: WindowOptions,
    application: A,
    // The accessibility adapter must be destroyed before its native window.
    accessibility: Option<crate::accessibility::AccessibilityAdapter>,
    window: Option<Arc<Window>>,
    proxy: PlatformProxy,
    active: bool,
    shown: bool,
    input: crate::input::InputAdapter,
    error: Option<PlatformError>,
}

impl<A: WindowApplication> Host<A> {
    fn dispatch(&mut self, event_loop: &ActiveEventLoop, event: PlatformEvent) {
        if self.error.is_some() || event_loop.exiting() {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let Some(accessibility) = self.accessibility.as_mut() else {
            return;
        };
        let mut context = WindowContext {
            window,
            event_loop,
            proxy: &self.proxy,
            accessibility,
        };
        if let Err(err) = self.application.event(&mut context, event) {
            self.error = Some(err);
            event_loop.exit();
        } else if self.active
            && !self.shown
            && !event_loop.exiting()
            && self.application.ready_to_show()
        {
            window.set_visible(true);
            self.shown = true;
        }
    }
}

impl<A: WindowApplication> ApplicationHandler<()> for Host<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active {
            return;
        }
        if self.window.is_none() {
            match event_loop.create_window(self.options.attributes()) {
                Ok(window) => self.window = Some(Arc::new(window)),
                Err(err) => {
                    self.error = Some(PlatformError::Window(err.to_string()));
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.accessibility.is_none()
            && let Some(window) = self.window.as_ref()
        {
            let proxy = self.proxy.clone();
            self.accessibility = Some(crate::accessibility::AccessibilityAdapter::new(
                event_loop,
                window,
                Arc::new(move || {
                    let _ = proxy.wake();
                }),
            ));
        }
        self.active = true;
        self.dispatch(event_loop, PlatformEvent::Resumed);
        if self.error.is_none()
            && let Some(window) = &self.window
        {
            let size = metrics(window);
            self.dispatch(event_loop, PlatformEvent::Resized(size));
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if self.active {
            self.active = false;
            self.input = crate::input::InputAdapter::default();
            if let Some(adapter) = self.accessibility.as_mut() {
                adapter.invalidate();
            }
            self.dispatch(event_loop, PlatformEvent::Suspended);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, (): ()) {
        self.proxy.pending.clear();
        if self.active {
            self.dispatch(event_loop, PlatformEvent::Wake);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = &self.window else {
            return;
        };
        if window.id() != window_id || !self.active || self.error.is_some() {
            return;
        }
        if let Some(adapter) = self.accessibility.as_mut() {
            adapter.process_event(window, &event);
            if matches!(
                event,
                WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
                    | WindowEvent::Focused(false)
                    | WindowEvent::CloseRequested
            ) {
                adapter.invalidate();
            }
        }
        if let WindowEvent::KeyboardInput { event, .. } = &event
            && is_paste_action(&event.logical_key)
        {
            if event.state == winit::event::ElementState::Pressed && !event.repeat {
                self.dispatch(event_loop, PlatformEvent::PasteRequested);
            }
            // Consume the OS action including repeats/releases: it must not
            // also reach the terminal as a second keyboard input.
            return;
        }
        if let Some(input) = self.input.window_event(&event) {
            self.dispatch(event_loop, PlatformEvent::Input(input));
            if self.error.is_some() {
                return;
            }
        }
        let Some(window) = &self.window else {
            return;
        };
        let event = match event {
            WindowEvent::CloseRequested => PlatformEvent::CloseRequested,
            WindowEvent::Focused(focused) => PlatformEvent::Focused(focused),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                PlatformEvent::Resized(metrics(window))
            }
            WindowEvent::RedrawRequested => {
                if metrics(window).physical_size.is_empty() {
                    return;
                }
                PlatformEvent::RedrawRequested
            }
            _ => return,
        };
        self.dispatch(event_loop, event);
    }
}

/// Run one native window on the calling (main) thread. Window close is delivered
/// to the consumer; it must call `context.exit()` after its shutdown bookkeeping.
/// Linux and macOS are supported; Windows explicitly rejects this initial backend.
/// A verified native display connection, prepared before runtime side effects.
pub struct NativeEventLoop(EventLoop<()>);

impl NativeEventLoop {
    pub fn prepare() -> Result<Self, PlatformError> {
        EventLoop::<()>::with_user_event()
            .build()
            .map(Self)
            .map_err(|err| PlatformError::EventLoop(err.to_string()))
    }

    pub fn run(
        self,
        options: WindowOptions,
        application: impl WindowApplication,
    ) -> Result<(), PlatformError> {
        run_prepared(self.0, options, application)
    }
}

pub fn run(
    options: WindowOptions,
    application: impl WindowApplication,
) -> Result<(), PlatformError> {
    NativeEventLoop::prepare()?.run(options, application)
}

fn run_prepared(
    event_loop: EventLoop<()>,
    options: WindowOptions,
    application: impl WindowApplication,
) -> Result<(), PlatformError> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(PlatformError::Unsupported);
    }
    if !valid_initial_size(options.initial_size) {
        return Err(PlatformError::InvalidSize);
    }
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = PlatformProxy {
        proxy: event_loop.create_proxy(),
        pending: Arc::new(WakeGate::default()),
    };
    let mut host = Host {
        options,
        application,
        window: None,
        proxy,
        active: false,
        shown: false,
        input: crate::input::InputAdapter::default(),
        accessibility: None,
        error: None,
    };
    event_loop
        .run_app(&mut host)
        .map_err(|err| PlatformError::EventLoop(err.to_string()))?;
    host.error.map_or(Ok(()), Err)
}

fn valid_initial_size(size: LogicalSize) -> bool {
    size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_decorations_are_enabled_by_default() {
        let options = WindowOptions::default();
        assert!(options.decorations);
        assert!(options.attributes().decorations);
    }

    #[test]
    fn decoration_choice_reaches_native_creation_without_changing_interaction_options() {
        let native_defaults = Window::default_attributes();
        for decorations in [false, true] {
            let options = WindowOptions {
                title: "Decoration test".into(),
                initial_size: LogicalSize {
                    width: 800.0,
                    height: 600.0,
                },
                decorations,
            };
            let attributes = options.attributes();
            assert_eq!(attributes.decorations, decorations);
            assert_eq!(attributes.title, options.title);
            assert_eq!(
                attributes.inner_size,
                Some(winit::dpi::Size::Logical(winit::dpi::LogicalSize::new(
                    800.0, 600.0,
                )))
            );
            assert_eq!(attributes.resizable, native_defaults.resizable);
            assert_eq!(attributes.active, native_defaults.active);
            assert_eq!(attributes.enabled_buttons, native_defaults.enabled_buttons);
            // Keep the AccessKit-before-show lifecycle for both choices.
            assert!(!attributes.visible);
        }
    }

    #[test]
    fn application_visibility_default_remains_opt_out() {
        struct ImmediateApplication;
        impl WindowApplication for ImmediateApplication {
            fn event(
                &mut self,
                _context: &mut WindowContext<'_>,
                _event: PlatformEvent,
            ) -> Result<(), PlatformError> {
                Ok(())
            }
        }
        assert!(ImmediateApplication.ready_to_show());
    }

    #[test]
    fn wake_storm_retains_only_one_pending_notification() {
        let gate = WakeGate::default();
        assert!(gate.try_queue());
        for _ in 0..1000 {
            assert!(!gate.try_queue());
        }
        gate.clear();
        assert!(gate.try_queue());
        assert!(!gate.try_queue());
    }

    #[test]
    fn os_paste_action_is_not_a_shortcut_map() {
        use winit::keyboard::{Key, NamedKey};
        assert!(super::is_paste_action(&Key::Named(NamedKey::Paste)));
        assert!(!super::is_paste_action(&Key::Character("v".into())));
        assert!(!super::is_paste_action(&Key::Named(NamedKey::Copy)));
        assert!(!super::is_paste_action(&Key::Named(NamedKey::Enter)));
    }

    #[test]
    fn invalid_sizes_are_rejected_before_event_loop_creation() {
        for size in [
            LogicalSize {
                width: 0.0,
                height: 10.0,
            },
            LogicalSize {
                width: f64::NAN,
                height: 1.0,
            },
            LogicalSize {
                width: 1.0,
                height: f64::INFINITY,
            },
        ] {
            assert!(!valid_initial_size(size));
        }
        assert!(valid_initial_size(WindowOptions::default().initial_size));
    }
}
