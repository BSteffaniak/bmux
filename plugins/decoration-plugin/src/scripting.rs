//! Lua scripting backend for the decoration plugin.
//!
//! Users can configure a `script = "decorations/name.lua"` path inside
//! their theme's `[plugins."bmux.decoration"]` section. When one or
//! more of the `scripting-*` cargo features is enabled, the
//! decoration plugin compiles the script via [`mlua`], caches the
//! compiled representation, and invokes a global `decorate(ctx)`
//! function per animation tick. The return value is a list of paint
//! commands the plugin inserts into its surface list before
//! publishing the next [`DecorationScene`].
//!
//! ## Backend selection
//!
//! The three `scripting-*` cargo features are mutually exclusive
//! (mlua enforces this at link time): `scripting-luajit` for maximum
//! performance on platforms where LuaJIT vendor-builds cleanly,
//! `scripting-luau` for a pure-Rust-friendlier build on
//! Windows-MSVC and other platforms where LuaJIT misbehaves, and
//! `scripting-lua54` for a middle-ground alternative. Consumers that
//! compile the decoration plugin without any scripting feature
//! receive a no-op backend — loading a script logs a warning and the
//! plugin proceeds with static decorations.
//!
//! ## Sandbox
//!
//! The backend strips `io`, `os`, `package`, `require`, `debug`, and
//! `dofile` from the Lua globals before script load. The only
//! external surface scripts can touch is the host-provided `print`
//! function (routed through the plugin log) plus the `bmux.*` helper
//! table injected at compile time.
//!
//! ## Performance monitoring
//!
//! Each `decorate()` invocation is wrapped with a wall-clock timer.
//! The backend tracks a rolling P95 over the last
//! [`PERF_WINDOW_FRAMES`] frames; when the P95 exceeds the
//! configurable `warn_script_ms` threshold (default
//! [`DEFAULT_WARN_MS`]), a `WARN`-level log is emitted at most once
//! per [`WARN_COOLDOWN`]. No hard budget or forced fallback — this
//! is user-owned performance, not a runtime kill switch.

#![allow(dead_code)] // Public surface is consumed by feature-gated call sites.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::missing_errors_doc,
    clippy::unnecessary_debug_formatting,
    clippy::doc_markdown
)] // Bounded numeric casts + trait-method doc noise are out of scope for this module's style.

use bmux_scene_protocol::scene_protocol::PaintCommand;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rolling window size for P95 perf sampling.
pub const PERF_WINDOW_FRAMES: usize = 60;

/// Default soft P95 threshold above which the backend logs a warning.
pub const DEFAULT_WARN_MS: f32 = 8.0;

/// Minimum spacing between consecutive perf warnings per script.
pub const WARN_COOLDOWN: Duration = Duration::from_mins(1);

/// Context passed to a script's `decorate(ctx)` function.
///
/// Fields are populated from the decoration plugin's live state at
/// the moment of invocation. Scripts receive a fresh context per
/// call; they cannot retain handles across frames (storing data
/// should go into Lua globals, not into the ctx object).
#[derive(Debug, Clone)]
pub struct DecorateContext {
    /// The attach-global cell coordinates of this pane.
    pub rect: (u16, u16, u16, u16),
    /// The interior rect (excluding decoration cells).
    pub content_rect: (u16, u16, u16, u16),
    /// Whether this pane currently has focus.
    pub focused: bool,
    /// Whether this pane is zoomed to fill the viewport.
    pub zoomed: bool,
    /// Whether a bell has been observed for this pane since the last
    /// clear. (Bell is reserved for a future wire; currently always
    /// false.)
    pub bell: bool,
    /// Milliseconds since the attach started. Scripts consume this to
    /// compute time-varying effects.
    pub time_ms: u64,
    /// Monotonic frame counter since the attach started.
    pub frame: u64,
}

/// Errors produced by script compile / invoke paths.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// Script source failed to compile.
    #[error("failed to compile decoration script {path:?}: {message}")]
    Compile { path: PathBuf, message: String },
    /// Script raised an error during `decorate(ctx)`.
    #[error("decoration script {path:?} runtime error: {message}")]
    Runtime { path: PathBuf, message: String },
    /// The bundled backend is a no-op stub (no `scripting-*` feature
    /// compiled in).
    #[error(
        "decoration scripting is not compiled into this build; enable one of the scripting-* features"
    )]
    NotAvailable,
    /// Filesystem error reading the script file.
    #[error("failed to read decoration script {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Result of a `decorate(ctx)` invocation. `commands` is the list of
/// paint commands the script wants applied; `duration` is the
/// wall-clock time the invocation consumed (used for perf tracking).
#[derive(Debug)]
pub struct DecorateOutcome {
    pub commands: Vec<PaintCommand>,
    pub duration: Duration,
}

/// Interface a scripting backend exposes to the decoration plugin.
///
/// Backends hold a compiled Lua state internally and re-invoke it
/// per tick. The trait is intentionally minimal so alternative
/// backends (a pure-Rust expression language, a WASM runtime) can
/// be dropped in without churn.
pub trait ScriptBackend: Send + Sync {
    /// Load + compile `source` (tagged with `path` for error
    /// reporting). On success the backend stores the compiled
    /// script; subsequent [`Self::invoke`] calls run it.
    fn compile(&self, path: &Path, source: &str) -> Result<(), ScriptError>;
    /// Invoke the last-compiled script's global `decorate(ctx)`.
    fn invoke(&self, ctx: &DecorateContext) -> Result<DecorateOutcome, ScriptError>;
    /// Short backend name ("luajit", "luau", "lua54", "stub").
    fn name(&self) -> &'static str;
    /// Whether this backend is a functional Lua runtime (as opposed
    /// to the stub emitted when no scripting feature is compiled in).
    fn is_functional(&self) -> bool;
}

/// Rolling perf sampler for [`ScriptBackend::invoke`] durations.
///
/// Tracks the last [`PERF_WINDOW_FRAMES`] durations and emits a
/// `WARN` log when the P95 exceeds `warn_ms` — at most once every
/// [`WARN_COOLDOWN`]. No hard budget; users who care about speed
/// act on the warning.
#[derive(Debug)]
pub struct PerfTracker {
    samples: Mutex<PerfInner>,
    warn_ms: f32,
    script_path: PathBuf,
}

#[derive(Debug)]
struct PerfInner {
    durations_micros: Vec<u32>,
    cursor: usize,
    last_warn_at: Option<Instant>,
}

impl PerfTracker {
    #[must_use]
    pub fn new(script_path: impl Into<PathBuf>, warn_ms: f32) -> Self {
        Self {
            samples: Mutex::new(PerfInner {
                durations_micros: Vec::with_capacity(PERF_WINDOW_FRAMES),
                cursor: 0,
                last_warn_at: None,
            }),
            warn_ms,
            script_path: script_path.into(),
        }
    }

    /// Record a `decorate()` invocation's duration. Returns the
    /// warning text when a perf threshold was crossed this sample
    /// (caller logs via `tracing::warn!`), or `None` otherwise.
    pub fn record(&self, duration: Duration) -> Option<String> {
        let Ok(mut inner) = self.samples.lock() else {
            return None;
        };
        let micros = u32::try_from(duration.as_micros()).unwrap_or(u32::MAX);
        if inner.durations_micros.len() < PERF_WINDOW_FRAMES {
            inner.durations_micros.push(micros);
        } else {
            let i = inner.cursor;
            inner.durations_micros[i] = micros;
            inner.cursor = (i + 1) % PERF_WINDOW_FRAMES;
        }
        // Need a full window before emitting any warning; saves us
        // from false positives during warmup.
        if inner.durations_micros.len() < PERF_WINDOW_FRAMES {
            return None;
        }
        let p95_micros = p95_of(&inner.durations_micros);
        let p95_ms = p95_micros as f32 / 1000.0;
        if p95_ms < self.warn_ms {
            return None;
        }
        if let Some(last) = inner.last_warn_at
            && last.elapsed() < WARN_COOLDOWN
        {
            return None;
        }
        inner.last_warn_at = Some(Instant::now());
        Some(format!(
            "decoration script {:?} P95={p95_ms:.2}ms exceeds warn threshold {:.2}ms — consider optimizing",
            self.script_path, self.warn_ms,
        ))
    }
}

/// Compute the 95th percentile of a sample slice. Copies into a
/// scratch buffer because the slice is expected to be a fixed-size
/// rolling window.
fn p95_of(samples: &[u32]) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    let mut scratch = samples.to_vec();
    scratch.sort_unstable();
    let idx = ((scratch.len() as f32) * 0.95).floor() as usize;
    let idx = idx.min(scratch.len() - 1);
    scratch[idx]
}

/// Construct the active backend for this build. When exactly one
/// scripting feature is enabled this returns the corresponding real
/// backend; otherwise it returns a stub whose `invoke` / `compile`
/// both surface [`ScriptError::NotAvailable`].
#[must_use]
pub fn make_backend() -> Box<dyn ScriptBackend> {
    #[cfg(any(
        feature = "scripting-luajit",
        feature = "scripting-luau",
        feature = "scripting-lua54"
    ))]
    {
        Box::new(lua_backend::LuaScriptBackend::new())
    }
    #[cfg(not(any(
        feature = "scripting-luajit",
        feature = "scripting-luau",
        feature = "scripting-lua54"
    )))]
    {
        Box::new(StubBackend)
    }
}

/// No-op backend used when no `scripting-*` feature is compiled in.
pub struct StubBackend;

impl ScriptBackend for StubBackend {
    fn compile(&self, _path: &Path, _source: &str) -> Result<(), ScriptError> {
        Err(ScriptError::NotAvailable)
    }

    fn invoke(&self, _ctx: &DecorateContext) -> Result<DecorateOutcome, ScriptError> {
        Err(ScriptError::NotAvailable)
    }

    fn name(&self) -> &'static str {
        "stub"
    }

    fn is_functional(&self) -> bool {
        false
    }
}

#[cfg(any(
    feature = "scripting-luajit",
    feature = "scripting-luau",
    feature = "scripting-lua54"
))]
mod lua_backend {
    //! Concrete `mlua`-backed implementation shared by all three
    //! scripting features. The backend holds a single Lua state
    //! across the life of the plugin; scripts are re-compiled on
    //! theme / script file changes.

    use super::{DecorateContext, DecorateOutcome, ScriptBackend, ScriptError};
    use bmux_scene_protocol::scene_protocol::{
        Color, GradientAxis, NamedColor, PaintCommand, Rect, Style,
    };
    use mlua::{Function, Lua, LuaOptions, StdLib, Table, Value};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Instant;

    const BACKEND_NAME: &str = {
        #[cfg(feature = "scripting-luajit")]
        {
            "luajit"
        }
        #[cfg(all(feature = "scripting-luau", not(feature = "scripting-luajit")))]
        {
            "luau"
        }
        #[cfg(all(
            feature = "scripting-lua54",
            not(feature = "scripting-luajit"),
            not(feature = "scripting-luau")
        ))]
        {
            "lua54"
        }
    };

    /// Thread-safe mlua-backed backend.
    pub struct LuaScriptBackend {
        inner: Mutex<LuaInner>,
    }

    struct LuaInner {
        lua: Lua,
        current_path: Option<PathBuf>,
        /// Compiled `decorate` function; refreshed on every
        /// `compile()` call. Kept as a registry key so the Lua state
        /// owns the actual function value.
        registry_key: Option<mlua::RegistryKey>,
    }

    impl LuaScriptBackend {
        #[must_use]
        pub fn new() -> Self {
            // Build the sandboxed standard library: disable `io`,
            // `os`, `package`, `debug`, `ffi`. Keep `string`,
            // `math`, `table`, `utf8`, `coroutine`.
            let std_libs =
                StdLib::STRING | StdLib::MATH | StdLib::TABLE | StdLib::UTF8 | StdLib::COROUTINE;
            let lua = Lua::new_with(std_libs, LuaOptions::new())
                .expect("constructing sandboxed mlua Lua state must succeed");
            // Remove the standard `print` function; scripts can log
            // via `bmux.log(...)` below. `dofile` / `loadfile` /
            // `loadstring` aren't exposed by the chosen StdLib set.
            let _ = lua.globals().set("print", Value::Nil);
            // Install the `bmux.*` helper table scripts use to
            // construct paint commands.
            install_bmux_helpers(&lua).expect("installing bmux helpers");
            Self {
                inner: Mutex::new(LuaInner {
                    lua,
                    current_path: None,
                    registry_key: None,
                }),
            }
        }
    }

    impl ScriptBackend for LuaScriptBackend {
        fn compile(&self, path: &Path, source: &str) -> Result<(), ScriptError> {
            let mut inner = self.inner.lock().map_err(|_| ScriptError::Runtime {
                path: path.to_path_buf(),
                message: "script state mutex poisoned".into(),
            })?;
            // Evaluate the chunk — running the top-level source
            // registers the `decorate` function as a global.
            let chunk = inner.lua.load(source).set_name(
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("decoration"),
            );
            chunk.exec().map_err(|e| ScriptError::Compile {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
            let decorate: Function =
                inner
                    .lua
                    .globals()
                    .get("decorate")
                    .map_err(|e| ScriptError::Compile {
                        path: path.to_path_buf(),
                        message: format!("script did not define global `decorate` function: {e}"),
                    })?;
            let key =
                inner
                    .lua
                    .create_registry_value(decorate)
                    .map_err(|e| ScriptError::Compile {
                        path: path.to_path_buf(),
                        message: format!("registry write failed: {e}"),
                    })?;
            // Drop the previous registry key first.
            if let Some(prev) = inner.registry_key.take() {
                let _ = inner.lua.remove_registry_value(prev);
            }
            inner.registry_key = Some(key);
            inner.current_path = Some(path.to_path_buf());
            Ok(())
        }

        fn invoke(&self, ctx: &DecorateContext) -> Result<DecorateOutcome, ScriptError> {
            let inner = self.inner.lock().map_err(|_| ScriptError::Runtime {
                path: PathBuf::new(),
                message: "script state mutex poisoned".into(),
            })?;
            let path = inner
                .current_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("<no script>"));
            let Some(key) = inner.registry_key.as_ref() else {
                return Err(ScriptError::Runtime {
                    path,
                    message: "no script compiled".into(),
                });
            };
            let decorate: Function =
                inner
                    .lua
                    .registry_value(key)
                    .map_err(|e| ScriptError::Runtime {
                        path: path.clone(),
                        message: format!("registry read failed: {e}"),
                    })?;
            let ctx_table =
                context_to_lua_table(&inner.lua, ctx).map_err(|e| ScriptError::Runtime {
                    path: path.clone(),
                    message: format!("building ctx table: {e}"),
                })?;
            let started_at = Instant::now();
            let result: Value = decorate.call(ctx_table).map_err(|e| ScriptError::Runtime {
                path: path.clone(),
                message: e.to_string(),
            })?;
            let duration = started_at.elapsed();
            let commands = match result {
                Value::Nil => Vec::new(),
                Value::Table(t) => {
                    table_to_paint_commands(&t).map_err(|e| ScriptError::Runtime {
                        path: path.clone(),
                        message: format!("converting result: {e}"),
                    })?
                }
                other => {
                    return Err(ScriptError::Runtime {
                        path,
                        message: format!("expected table of paint commands, got {other:?}"),
                    });
                }
            };
            Ok(DecorateOutcome { commands, duration })
        }

        fn name(&self) -> &'static str {
            BACKEND_NAME
        }

        fn is_functional(&self) -> bool {
            true
        }
    }

    fn context_to_lua_table(lua: &Lua, ctx: &DecorateContext) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        t.set("rect", rect_tuple_to_table(lua, ctx.rect)?)?;
        t.set("content_rect", rect_tuple_to_table(lua, ctx.content_rect)?)?;
        t.set("focused", ctx.focused)?;
        t.set("zoomed", ctx.zoomed)?;
        t.set("bell", ctx.bell)?;
        t.set("time_ms", ctx.time_ms)?;
        t.set("frame", ctx.frame)?;
        Ok(t)
    }

    fn rect_tuple_to_table(lua: &Lua, rect: (u16, u16, u16, u16)) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        t.set("x", rect.0)?;
        t.set("y", rect.1)?;
        t.set("w", rect.2)?;
        t.set("h", rect.3)?;
        Ok(t)
    }

    /// Install the `bmux` helper table into Lua globals. This is the
    /// only scripting surface the sandbox exposes beyond the reduced
    /// stdlib.
    fn install_bmux_helpers(lua: &Lua) -> mlua::Result<()> {
        let bmux = lua.create_table()?;
        // `bmux.log(level, msg)` — routes into tracing via the host
        // plugin log. Level is one of "info"/"warn"/"error"; anything
        // else defaults to "info".
        let log_fn = lua.create_function(|_, (level, message): (String, String)| {
            match level.as_str() {
                "warn" => tracing::warn!(target: "decoration.script", "{message}"),
                "error" => tracing::error!(target: "decoration.script", "{message}"),
                _ => tracing::info!(target: "decoration.script", "{message}"),
            }
            Ok(())
        })?;
        bmux.set("log", log_fn)?;
        // `bmux.rgb(r, g, b) -> table` — returns a table shaped like
        // our Rust `Color::Rgb` variant. Scripts never interact with
        // the raw serde layout; they use these helpers.
        let rgb_fn = lua.create_function(|lua, (r, g, b): (u8, u8, u8)| {
            let t = lua.create_table()?;
            t.set("kind", "rgb")?;
            t.set("r", r)?;
            t.set("g", g)?;
            t.set("b", b)?;
            Ok(t)
        })?;
        bmux.set("rgb", rgb_fn)?;
        // `bmux.named(name) -> table` — named-color helper.
        let named_fn = lua.create_function(|lua, name: String| {
            let t = lua.create_table()?;
            t.set("kind", "named")?;
            t.set("name", name)?;
            Ok(t)
        })?;
        bmux.set("named", named_fn)?;
        // `bmux.hsl_to_rgb(h, s, l) -> (r, g, b)` — scripts pass the
        // returned triple to `bmux.rgb()` if they want to construct a
        // truecolor directly.
        let hsl_fn = lua.create_function(|_, (h, s, l): (f32, f32, f32)| {
            let (r, g, b) = hsl_to_rgb(h, s, l);
            Ok((r, g, b))
        })?;
        bmux.set("hsl_to_rgb", hsl_fn)?;
        lua.globals().set("bmux", bmux)?;
        Ok(())
    }

    /// HSL → RGB. `h` in [0,360), `s`/`l` in [0,1].
    // The single-letter names mirror the canonical HSL→RGB formula
    // (hue/saturation/lightness plus the derived chroma `c`, intermediate
    // `x`, and match lightness `m`); renaming them hurts readability vs.
    // the standard pseudocode.
    #[allow(clippy::many_single_char_names)]
    fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
        let h = (h.rem_euclid(360.0)) / 60.0;
        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
        let (r1, g1, b1) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = l - c / 2.0;
        let clamp = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        (clamp(r1), clamp(g1), clamp(b1))
    }

    /// Convert a Lua-side table of paint-command descriptors into our
    /// typed `PaintCommand` enum. Scripts construct entries as plain
    /// tables with a `kind` string field plus the variant fields;
    /// unknown kinds are skipped with a warning log.
    fn table_to_paint_commands(t: &Table) -> mlua::Result<Vec<PaintCommand>> {
        let mut out = Vec::new();
        for pair in t.sequence_values::<Table>() {
            let entry = pair?;
            let kind: String = entry.get("kind").unwrap_or_default();
            let z: i16 = entry.get("z").unwrap_or(0);
            match kind.as_str() {
                "text" => {
                    let col: u16 = entry.get("col")?;
                    let row: u16 = entry.get("row")?;
                    let text: String = entry.get("text")?;
                    let style = style_from_table(entry.get("style").ok());
                    out.push(PaintCommand::Text {
                        col,
                        row,
                        z,
                        text,
                        style,
                    });
                }
                "filled_rect" => {
                    let rect_tbl: Table = entry.get("rect")?;
                    let rect = rect_from_table(&rect_tbl)?;
                    let glyph: String = entry.get("glyph")?;
                    let style = style_from_table(entry.get("style").ok());
                    out.push(PaintCommand::FilledRect {
                        rect,
                        z,
                        glyph,
                        style,
                    });
                }
                "gradient_run" => {
                    let col: u16 = entry.get("col")?;
                    let row: u16 = entry.get("row")?;
                    let text: String = entry.get("text")?;
                    let from_style = style_from_table(entry.get("from_style").ok());
                    let to_style = style_from_table(entry.get("to_style").ok());
                    let axis_str: String = entry
                        .get("axis")
                        .unwrap_or_else(|_| "horizontal".to_string());
                    let axis = if axis_str == "vertical" {
                        GradientAxis::Vertical
                    } else {
                        GradientAxis::Horizontal
                    };
                    out.push(PaintCommand::GradientRun {
                        col,
                        row,
                        z,
                        text,
                        axis,
                        from_style,
                        to_style,
                    });
                }
                "box_border" => {
                    let rect_tbl: Table = entry.get("rect")?;
                    let rect = rect_from_table(&rect_tbl)?;
                    let glyphs_name: String = entry
                        .get("glyphs")
                        .unwrap_or_else(|_| "single_line".to_string());
                    let glyphs = crate::glyphs::parse_border_glyphs(&glyphs_name);
                    let style = style_from_table(entry.get("style").ok());
                    out.push(PaintCommand::BoxBorder {
                        rect,
                        z,
                        glyphs,
                        style,
                    });
                }
                other => {
                    tracing::warn!(
                        target: "decoration.script",
                        "unknown paint-command kind {other:?}; skipping",
                    );
                }
            }
        }
        Ok(out)
    }

    fn rect_from_table(t: &Table) -> mlua::Result<Rect> {
        Ok(Rect {
            x: t.get("x")?,
            y: t.get("y")?,
            w: t.get("w")?,
            h: t.get("h")?,
        })
    }

    fn style_from_table(t: Option<Table>) -> Style {
        let mut style = Style {
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            reverse: false,
            dim: false,
            blink: false,
            strikethrough: false,
        };
        let Some(t) = t else {
            return style;
        };
        if let Ok(fg) = t.get::<Option<Table>>("fg") {
            style.fg = fg.and_then(|tbl| color_from_table(&tbl));
        }
        if let Ok(bg) = t.get::<Option<Table>>("bg") {
            style.bg = bg.and_then(|tbl| color_from_table(&tbl));
        }
        style.bold = t.get("bold").unwrap_or(false);
        style.underline = t.get("underline").unwrap_or(false);
        style.italic = t.get("italic").unwrap_or(false);
        style.reverse = t.get("reverse").unwrap_or(false);
        style.dim = t.get("dim").unwrap_or(false);
        style.blink = t.get("blink").unwrap_or(false);
        style.strikethrough = t.get("strikethrough").unwrap_or(false);
        style
    }

    fn color_from_table(t: &Table) -> Option<Color> {
        let kind: String = t.get("kind").ok()?;
        match kind.as_str() {
            "rgb" => {
                let r: u8 = t.get("r").ok()?;
                let g: u8 = t.get("g").ok()?;
                let b: u8 = t.get("b").ok()?;
                Some(Color::Rgb { r, g, b })
            }
            "named" => {
                let name: String = t.get("name").ok()?;
                Some(Color::Named {
                    name: parse_named_color(&name),
                })
            }
            "indexed" => {
                let index: u8 = t.get("index").ok()?;
                Some(Color::Indexed { index })
            }
            _ => None,
        }
    }

    fn parse_named_color(name: &str) -> NamedColor {
        match name {
            "black" => NamedColor::Black,
            "red" => NamedColor::Red,
            "green" => NamedColor::Green,
            "yellow" => NamedColor::Yellow,
            "blue" => NamedColor::Blue,
            "magenta" => NamedColor::Magenta,
            "cyan" => NamedColor::Cyan,
            "bright_black" => NamedColor::BrightBlack,
            "bright_red" => NamedColor::BrightRed,
            "bright_green" => NamedColor::BrightGreen,
            "bright_yellow" => NamedColor::BrightYellow,
            "bright_blue" => NamedColor::BrightBlue,
            "bright_magenta" => NamedColor::BrightMagenta,
            "bright_cyan" => NamedColor::BrightCyan,
            "bright_white" => NamedColor::BrightWhite,
            _ => NamedColor::White,
        }
    }
}

/// Bundled Lua sample scripts shipped with the decoration plugin.
/// Each entry is `(name, source)` — `name` is the file stem users
/// reference in their theme's `script = "decorations/{name}.lua"`.
///
/// The list is empty when the `bundled-decoration-scripts` cargo
/// feature is disabled. A `scripting-*` feature is *not* required
/// for the list itself — the scripts are bundled as plain strings.
#[must_use]
pub fn bundled_decoration_scripts() -> &'static [(&'static str, &'static str)] {
    #[cfg(feature = "bundled-decoration-scripts")]
    {
        &[
            ("pulse", include_str!("../assets/decorations/pulse.lua")),
            (
                "rainbow_snake",
                include_str!("../assets/decorations/rainbow_snake.lua"),
            ),
        ]
    }
    #[cfg(not(feature = "bundled-decoration-scripts"))]
    {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_of_returns_highest_for_small_windows() {
        assert_eq!(p95_of(&[1, 2, 3, 4, 5]), 5);
    }

    #[test]
    fn p95_of_handles_full_window() {
        let mut v = Vec::with_capacity(PERF_WINDOW_FRAMES);
        for i in 0..PERF_WINDOW_FRAMES as u32 {
            v.push(i);
        }
        // 95th percentile of 0..60 ≈ index 57.
        assert_eq!(p95_of(&v), 57);
    }

    #[test]
    fn perf_tracker_only_warns_after_full_window() {
        let tracker = PerfTracker::new("/tmp/x.lua", 1.0);
        for _ in 0..(PERF_WINDOW_FRAMES - 1) {
            let warn = tracker.record(Duration::from_millis(50));
            assert!(warn.is_none(), "no warn before the window fills");
        }
        // The filling sample should trigger a warn.
        let warn = tracker.record(Duration::from_millis(50));
        assert!(warn.is_some(), "should warn once window is full");
    }

    #[test]
    fn perf_tracker_cooldown_suppresses_followups() {
        let tracker = PerfTracker::new("/tmp/x.lua", 1.0);
        // Fill + trip.
        for _ in 0..PERF_WINDOW_FRAMES {
            tracker.record(Duration::from_millis(50));
        }
        // Immediate follow-up must NOT warn again (cooldown).
        let next = tracker.record(Duration::from_millis(50));
        assert!(next.is_none());
    }

    #[test]
    fn stub_backend_is_not_functional() {
        let backend = StubBackend;
        assert!(!backend.is_functional());
        assert_eq!(backend.name(), "stub");
        let err = backend.compile(Path::new("x.lua"), "").unwrap_err();
        assert!(matches!(err, ScriptError::NotAvailable));
    }

    #[test]
    fn make_backend_returns_some_backend() {
        let b = make_backend();
        #[cfg(any(
            feature = "scripting-luajit",
            feature = "scripting-luau",
            feature = "scripting-lua54"
        ))]
        assert!(b.is_functional());
        #[cfg(not(any(
            feature = "scripting-luajit",
            feature = "scripting-luau",
            feature = "scripting-lua54"
        )))]
        assert!(!b.is_functional());
    }

    // Real-Lua integration tests. Gated on a scripting feature being
    // enabled; the CI matrix picks whichever backend builds on each
    // target and inherits the coverage.
    #[cfg(any(
        feature = "scripting-luajit",
        feature = "scripting-luau",
        feature = "scripting-lua54"
    ))]
    mod lua_integration {
        use super::super::{DecorateContext, make_backend};
        use std::path::Path;

        fn ctx() -> DecorateContext {
            DecorateContext {
                rect: (0, 0, 20, 5),
                content_rect: (1, 1, 18, 3),
                focused: true,
                zoomed: false,
                bell: false,
                time_ms: 500,
                frame: 10,
            }
        }

        #[test]
        fn minimal_script_returns_single_text_paint_command() {
            let backend = make_backend();
            let source = r#"
                function decorate(ctx)
                    return {
                        {
                            kind = "text",
                            col = 0,
                            row = 0,
                            z = 0,
                            text = "hello",
                            style = { fg = bmux.rgb(255, 0, 0), bold = true },
                        },
                    }
                end
            "#;
            backend
                .compile(Path::new("<test>"), source)
                .expect("compile");
            let outcome = backend.invoke(&ctx()).expect("invoke");
            assert_eq!(outcome.commands.len(), 1);
        }

        #[test]
        fn script_with_syntax_error_returns_compile_error() {
            let backend = make_backend();
            let err = backend
                .compile(Path::new("<test>"), "function decorate(ctx return {}")
                .expect_err("syntax error must surface");
            match err {
                super::super::ScriptError::Compile { .. } => {}
                other => panic!("expected Compile error, got {other:?}"),
            }
        }

        #[test]
        fn script_reading_host_io_is_sandboxed_away() {
            let backend = make_backend();
            // `io` and `os` should not be reachable. We expect the
            // script to throw a runtime error when it tries to read.
            let source = r#"
                function decorate(ctx)
                    local f = io.open("/etc/passwd", "r")
                    return {}
                end
            "#;
            backend
                .compile(Path::new("<test>"), source)
                .expect("compile");
            let err = backend
                .invoke(&ctx())
                .expect_err("io.open must not be reachable in sandbox");
            match err {
                super::super::ScriptError::Runtime { .. } => {}
                other => panic!("expected Runtime error, got {other:?}"),
            }
        }

        #[test]
        fn context_fields_are_observable_from_lua() {
            let backend = make_backend();
            let source = r#"
                function decorate(ctx)
                    assert(ctx.focused == true, "focused should be true")
                    assert(ctx.rect.w == 20, "rect.w should be 20")
                    return {}
                end
            "#;
            backend
                .compile(Path::new("<test>"), source)
                .expect("compile");
            backend.invoke(&ctx()).expect("invoke");
        }
    }
}
