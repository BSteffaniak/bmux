//! Line-oriented benchmark driver; see docs/mermaid-wasm-performance.md.
//! No renderer output cache is used. Run only in a dedicated benchmark process.
use std::{
    error::Error,
    fmt::Write as _,
    io::{self, BufRead, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bmux_mermaid_render::{
    CapturedMermaidWorker, IsolatedMermaidRenderer, MermaidCancellationToken, MermaidRenderError,
    MermaidRenderRequest, MermaidRendered, MermaidRenderedOutput, render_mermaid,
};
use sha2::{Digest, Sha256};

#[cfg(feature = "wasm")]
use bmux_mermaid_render::WasmMermaidRenderer;

type RenderResult = Result<MermaidRendered, MermaidRenderError>;

// Exact compile-time corpus bytes, not caller-provided trust assertions. Do not add
// malformed, dense or limit cases here: native timeout does not stop its thread.
const NATIVE_TRUSTED: [&str; 6] = [
    include_str!("../bench-corpus/flowchart.mmd"),
    include_str!("../bench-corpus/sequence.mmd"),
    include_str!("../bench-corpus/class.mmd"),
    include_str!("../bench-corpus/state.mmd"),
    include_str!("../bench-corpus/er.mmd"),
    include_str!("../bench-corpus/pie.mmd"),
];

struct Driver {
    mode: String,
    capture: Option<CapturedMermaidWorker>,
    acquired: Option<IsolatedMermaidRenderer>,
    #[cfg(feature = "wasm")]
    wasm: Option<WasmMermaidRenderer>,
}

impl Driver {
    fn new(mode: String, worker: &Path, fuel: u64) -> Result<Self, Box<dyn Error>> {
        #[cfg(not(feature = "wasm"))]
        let _ = fuel;
        let mut driver = Self {
            mode,
            capture: None,
            acquired: None,
            #[cfg(feature = "wasm")]
            wasm: None,
        };
        match driver.mode.as_str() {
            "current" => driver.capture = Some(CapturedMermaidWorker::capture(worker)?),
            "worker" => {
                let mut capture = CapturedMermaidWorker::capture(worker)?;
                driver.acquired = Some(capture.acquire()?);
                driver.capture = Some(capture);
            }
            "native" => (),
            #[cfg(feature = "wasm")]
            "wasm-warm" => driver.wasm = Some(WasmMermaidRenderer::new()?),
            #[cfg(feature = "wasm")]
            "wasm-jit-warm" => driver.wasm = Some(WasmMermaidRenderer::new_jit()?),
            #[cfg(feature = "wasm")]
            "wasm-fuel" => driver.wasm = Some(WasmMermaidRenderer::new_with_fuel(fuel)?),
            #[cfg(feature = "wasm")]
            "wasm-cold" | "wasm-jit-cold" => (),
            _ => {
                return Err(format!(
                    "unsupported mode: {} (WASM requires feature wasm)",
                    driver.mode
                )
                .into());
            }
        }
        Ok(driver)
    }

    fn render(
        &mut self,
        request: &MermaidRenderRequest,
        token: &MermaidCancellationToken,
    ) -> (RenderResult, u128, u128) {
        let mut construction_ns = 0;
        #[cfg(feature = "wasm")]
        let cold = if matches!(self.mode.as_str(), "wasm-cold" | "wasm-jit-cold") {
            let started = Instant::now();
            let renderer = if self.mode == "wasm-cold" {
                WasmMermaidRenderer::new()
            } else {
                WasmMermaidRenderer::new_jit()
            };
            construction_ns = started.elapsed().as_nanos();
            match renderer {
                Ok(renderer) => Some(renderer),
                Err(error) => return (Err(error), construction_ns, 0),
            }
        } else {
            None
        };
        #[cfg(not(feature = "wasm"))]
        let _ = &mut construction_ns;
        let render_started = Instant::now();
        let result = match self.mode.as_str() {
            "current" => {
                // Presenter acquire -> loader pin -> render. Both snapshots drop
                // before the timer stops; capture persists like plugin activation.
                // Keep fallible snapshot setup inside the timed result closure.
                (|| {
                    let acquired = self
                        .capture
                        .as_mut()
                        .expect("initialized capture")
                        .acquire()?;
                    IsolatedMermaidRenderer::pin(acquired.executable())?.render(request, token)
                })()
            }
            "worker" => self
                .acquired
                .as_ref()
                .expect("initialized snapshot")
                .render(request, token),
            "native" => render_mermaid(request, token),
            #[cfg(feature = "wasm")]
            "wasm-warm" | "wasm-jit-warm" | "wasm-fuel" => self
                .wasm
                .as_ref()
                .expect("initialized module")
                .render(request, token),
            #[cfg(feature = "wasm")]
            "wasm-cold" | "wasm-jit-cold" => cold
                .as_ref()
                .expect("constructed cold renderer")
                .render(request, token),
            _ => unreachable!("validated mode"),
        };
        (result, construction_ns, render_started.elapsed().as_nanos())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    if !(3..=4).contains(&arguments.len()) {
        return Err("usage: compare MODE ABSOLUTE_WORKER_PATH [FUEL_BUDGET=1000000000]".into());
    }
    let fuel = arguments
        .get(3)
        .map_or(Ok(1_000_000_000_u64), |value| value.parse())?;
    if fuel == 0 {
        return Err("fuel budget must be positive".into());
    }
    let setup = Instant::now();
    let mut driver = Driver::new(arguments[1].clone(), Path::new(&arguments[2]), fuel)?;
    println!("READY\t{}", setup.elapsed().as_nanos());
    io::stdout().flush()?;
    // Input: absolute source path TAB cancellation delay ms or '-' TAB SVG path or '-'.
    for line in io::stdin().lock().lines() {
        let line = line?;
        let (path, options) = line.split_once('\t').ok_or("invalid input record")?;
        let (cancel, output) = options.split_once('\t').ok_or("invalid output option")?;
        if std::fs::metadata(path)?.len() > 65_537 {
            return Err("benchmark source exceeds largest corpus case".into());
        }
        let source = std::fs::read_to_string(path)?;
        if driver.mode == "native" && (cancel != "-" || !NATIVE_TRUSTED.contains(&source.as_str()))
        {
            return Err(
                "native diagnostic accepts only exact small trusted corpus; no cancellation".into(),
            );
        }
        let request = MermaidRenderRequest::svg(source, 800, 600);
        let token = MermaidCancellationToken::default();
        let cancelled_at = Arc::new(Mutex::new(None));
        let cancelling = if cancel == "-" {
            None
        } else {
            let delay = Duration::from_millis(cancel.parse()?);
            let token = token.clone();
            let cancelled_at = Arc::clone(&cancelled_at);
            Some(std::thread::spawn(move || {
                std::thread::sleep(delay);
                let mut at = cancelled_at.lock().expect("cancel timestamp lock");
                *at = Some(Instant::now());
                token.cancel();
            }))
        };
        let started = Instant::now();
        let (result, construction_ns, render_call_ns) = driver.render(&request, &token);
        let ended = Instant::now();
        if let Some(cancelling) = cancelling {
            cancelling
                .join()
                .map_err(|_| "cancellation thread panicked")?;
        }
        let cancel_ns = cancelled_at
            .lock()
            .map_err(|_| "cancel timestamp lock")?
            .filter(|at| *at <= ended)
            .map_or_else(
                || "-".to_owned(),
                |at| ended.duration_since(at).as_nanos().to_string(),
            );
        let (bytes, digest, status) = match &result {
            Ok(rendered) => {
                let MermaidRenderedOutput::Svg(svg) = &rendered.output;
                let mut digest = String::with_capacity(64);
                for byte in Sha256::digest(svg) {
                    write!(digest, "{byte:02x}")?;
                }
                (svg.len(), digest, "ok".to_owned())
            }
            Err(error) => (
                0,
                "-".to_owned(),
                format!("{error:?}").replace(['\t', '\n', '\r'], " "),
            ),
        };
        if output != "-"
            && let Ok(rendered) = &result
        {
            let MermaidRenderedOutput::Svg(svg) = &rendered.output;
            std::fs::write(output, svg)?;
        }
        println!(
            "RESULT\t{}\t{bytes}\t{digest}\t{status}\t{cancel_ns}\t{construction_ns}\t{render_call_ns}",
            ended.duration_since(started).as_nanos()
        );
        io::stdout().flush()?;
        // Never continue after a native timeout/error: its backend thread may
        // still be running. Process exit is the only hard-stop boundary here.
        if driver.mode == "native" && result.is_err() {
            std::process::exit(2);
        }
    }
    Ok(())
}
