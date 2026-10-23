# bterm_native_platform

Native main-thread window/event-loop host for Bterm. Linux (X11/Wayland) and
macOS use Winit privately; the initial Windows entry explicitly returns
`PlatformError::Unsupported`.

`run(WindowOptions, WindowApplication)` owns the event loop and window.
Callbacks receive owned lifecycle/input events and a borrowed `WindowContext`.
Workers may retain a coalescing wake proxy, not native windows. `Resized` carries
physical pixels and scale together; these are not terminal execution dimensions.
Suspension and zero drawable extents suppress redraw delivery. Callback errors
terminate the loop and are returned to the caller.

`WindowOptions::decorations` defaults to `true`. Set it to `false` before `run`
to request a window without native titlebar/chrome. The option is passed to
Winit's native window creation on macOS and Linux (X11/Wayland), not a renderer
or terminal layout toggle. Native decoration requests remain subject to the
window manager/compositor. Resizability and focus options, input routing,
clipboard ownership, and accessibility attachment are unchanged; hiding chrome
also removes its titlebar buttons and drag affordance. This is a creation-time
option: configuration changes do not restyle an existing window; apply them to
new windows. There is no live-decoration setter in `WindowContext`.

The renderer receives only an opaque `SurfaceTarget` lifetime token. Its raw
handle trait implementations, enabled only by the `renderer-adapter` feature,
form the quarantined adapter seam; consumers must not build a
second window, compositor, scene, hit map, or interaction authority. A window
redraw request is not presentation success. The retained presentation consumer
alone acknowledges successful output and publishes its corresponding metadata.

`input::InputEvent` reexports backend-neutral events. Logical keys, physical
keys, and text remain distinct: resolve bindings first, then send text or use
the key encoder, never both. Preedit is presentation-only; IME commits are not
paste events. `ime::ImeCursorArea` supplies a physical candidate rectangle only
for the matching committed presentation revision.

Accessibility exports committed visible semantics using physical cell geometry;
generation-fenced actions return through normal interaction routing. Clipboard
ownership is application-lived, separate from selection and terminal query
replies; platform failures remain explicit.

Input/IME translation and accessibility/clipboard integration live in their
respective modules. This crate does not start, select, replace, or stop BMUX
runtimes and does not own plugin resources or PTYs.
