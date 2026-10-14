#![cfg(feature = "wasm")]

use std::time::{Duration, Instant};

use bmux_mermaid_render::{
    MermaidCancellationToken, MermaidRenderError, MermaidRenderRequest, MermaidRenderedOutput,
    WasmMermaidRenderer,
};

fn guest(body: &str, response: &[u8]) -> WasmMermaidRenderer {
    let bytes = response
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let module = format!(
        r#"(module
        (memory (export "memory") 1)
        (data (i32.const 4096) "{bytes}")
        (global $calls (mut i32) (i32.const 0))
        (global $frees (mut i32) (i32.const 0))
        (func (export "alloc") (param i32) (result i32) i32.const 64)
        (func (export "dealloc") (param $ptr i32) (param i32)
            ;; Check output and host-owned input are released separately, in order.
            global.get $frees i32.eqz
            if
                local.get $ptr i32.const 4096 i32.ne if unreachable end
            else
                local.get $ptr i32.const 64 i32.ne if unreachable end
            end
            global.get $frees i32.const 1 i32.add global.set $frees)
        (func (export "render") (param $ptr i32) (param i32) (result i64)
            ;; Repeated renders must not reuse an earlier request's guest state.
            global.get $calls if unreachable end
            i32.const 1 global.set $calls
            {body}
            i64.const {}))"#,
        (4096_u64 << 32) | response.len() as u64
    );
    module_renderer(&module)
}

fn module_renderer(module: &str) -> WasmMermaidRenderer {
    WasmMermaidRenderer::from_module_bytes(&wat::parse_str(module).unwrap()).unwrap()
}

fn request() -> MermaidRenderRequest {
    MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600)
}

fn render(
    renderer: &WasmMermaidRenderer,
) -> Result<bmux_mermaid_render::MermaidRendered, MermaidRenderError> {
    renderer.render(&request(), &MermaidCancellationToken::default())
}

#[test]
fn synthetic_guest_abi_bounds_and_settlement() {
    let renderer = guest("", b"\0<svg width=\"12\" height=\"8\"></svg>");
    for _ in 0..20 {
        let rendered = render(&renderer).unwrap();
        assert_eq!(
            rendered.output,
            MermaidRenderedOutput::Svg(b"<svg width=\"12\" height=\"8\"></svg>".to_vec())
        );
        assert_eq!(rendered.cache_key, request().cache_key());
    }
    assert!(matches!(
        render(&guest("", b"\x01bad diagram")),
        Err(MermaidRenderError::InvalidDiagram { .. })
    ));
    for response in [b"".as_slice(), b"\x02<svg/>", b"\0\xff", b"\0not svg"] {
        assert!(matches!(
            render(&guest("", response)),
            Err(MermaidRenderError::InvalidWorkerResponse { .. })
        ));
    }
    let mut small = request();
    small.limits.max_output_bytes = 3;
    assert!(matches!(
        renderer.render(&small, &MermaidCancellationToken::default()),
        Err(MermaidRenderError::OutputTooLarge { .. })
    ));
    assert_eq!(
        render(&guest("", b"\0<svg width=\"900\" height=\"8\"></svg>")),
        Err(MermaidRenderError::OutputDimensionsExceeded)
    );

    for body in [
        "unreachable",
        "i32.const 8193 memory.grow drop",
        "i32.const 65536 i32.load drop",
    ] {
        assert!(matches!(
            render(&guest(body, b"\0<svg/>")),
            Err(MermaidRenderError::InvalidDiagram { .. })
        ));
        assert!(render(&renderer).is_ok());
    }
    for packed in [(64_u64 << 32) | 8, (65535_u64 << 32) | 8] {
        let invalid = guest(&format!("i64.const {packed} return"), b"\0<svg/>");
        assert!(matches!(
            render(&invalid),
            Err(MermaidRenderError::InvalidWorkerResponse { .. })
        ));
    }
    let missing = module_renderer("(module (memory (export \"memory\") 1))");
    assert!(matches!(
        render(&missing),
        Err(MermaidRenderError::InvalidWorkerResponse { .. })
    ));
    let imported = wat::parse_str("(module (import \"env\" \"host\" (func)))").unwrap();
    assert!(matches!(
        WasmMermaidRenderer::from_module_bytes(&imported),
        Err(MermaidRenderError::WorkerUnavailable { .. })
    ));

    cancellation_and_settlement(&renderer);
}

fn cancellation_and_settlement(renderer: &WasmMermaidRenderer) {
    let cancelled = MermaidCancellationToken::default();
    cancelled.cancel();
    assert_eq!(
        renderer.render(&request(), &cancelled),
        Err(MermaidRenderError::Cancelled)
    );
    let looping = guest("(loop $forever br $forever)", b"\0<svg/>");
    for _ in 0..5 {
        let mut bounded = request();
        bounded.limits.timeout = Duration::from_millis(40);
        let started = Instant::now();
        assert_eq!(
            looping.render(&bounded, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(render(renderer).is_ok());
    }

    // Concurrent cancellation isolation uses actual epoch readiness in the
    // runtime's async unit test, rather than assuming a sleeping host has started.
    assert!(render(renderer).is_ok());
}

#[test]
fn embedded_aot_and_jit_have_equivalent_results() {
    let aot = WasmMermaidRenderer::new().unwrap();
    let jit = WasmMermaidRenderer::new_jit().unwrap();
    let fuel_aot = WasmMermaidRenderer::new_with_fuel(u64::MAX).unwrap();
    let fuel_jit = WasmMermaidRenderer::new_jit_with_fuel(u64::MAX).unwrap();
    for source in [
        "flowchart LR\nA --> B",
        "sequenceDiagram\nAlice->>Bob: Hello",
        "not a valid Mermaid diagram",
    ] {
        let mut request = MermaidRenderRequest::svg(source, 4096, 4096);
        request.limits.timeout = Duration::from_secs(15);
        let token = MermaidCancellationToken::default();
        let expected = jit.render(&request, &token);
        if source.starts_with("flowchart") || source.starts_with("sequenceDiagram") {
            assert!(expected.is_ok(), "valid guest corpus failed: {expected:?}");
        } else {
            assert!(
                expected.is_err(),
                "invalid guest corpus unexpectedly succeeded"
            );
        }
        assert_eq!(aot.render(&request, &token), expected);
        assert_eq!(fuel_aot.render(&request, &token), expected);
        assert_eq!(fuel_jit.render(&request, &token), expected);
    }
    assert!(matches!(
        WasmMermaidRenderer::new_with_fuel(0),
        Err(MermaidRenderError::InvalidExecutionLimits)
    ));
    assert!(matches!(
        WasmMermaidRenderer::new_jit_with_fuel(0),
        Err(MermaidRenderError::InvalidExecutionLimits)
    ));
}

#[test]
fn fuel_budget_is_opt_in_and_settles_exhaustion() {
    let module = wat::parse_str(
        r#"(module
        (memory (export "memory") 1)
        (func (export "alloc") (param i32) (result i32) i32.const 64)
        (func (export "dealloc") (param i32 i32))
        (func (export "render") (param i32 i32) (result i64)
            (loop $forever br $forever) i64.const 0))"#,
    )
    .unwrap();
    assert!(matches!(
        WasmMermaidRenderer::from_module_bytes_with_fuel(&module, 0),
        Err(MermaidRenderError::InvalidExecutionLimits)
    ));
    let renderer = WasmMermaidRenderer::from_module_bytes_with_fuel(&module, 10_000).unwrap();
    for _ in 0..3 {
        assert!(matches!(
            render(&renderer),
            Err(MermaidRenderError::InvalidDiagram { .. })
        ));
    }
}

#[test]
fn initialization_and_cleanup_guest_code_are_also_interruptible() {
    for export in ["alloc", "render", "dealloc"] {
        let signatures = match export {
            "alloc" => (
                "(loop $forever br $forever) i32.const 64",
                "i64.const 17592186044423",
                "",
            ),
            "render" => (
                "i32.const 64",
                "(loop $forever br $forever) i64.const 17592186044423",
                "",
            ),
            _ => (
                "i32.const 64",
                "i64.const 17592186044423",
                "(loop $forever br $forever)",
            ),
        };
        let module = format!(
            r#"(module
            (memory (export "memory") 1)
            (data (i32.const 4096) "\00<svg/>")
            (func (export "alloc") (param i32) (result i32) {})
            (func (export "render") (param i32 i32) (result i64) {})
            (func (export "dealloc") (param i32 i32) {}))"#,
            signatures.0, signatures.1, signatures.2
        );
        let renderer = module_renderer(&module);
        let mut bounded = request();
        bounded.limits.timeout = Duration::from_millis(40);
        assert_eq!(
            renderer.render(&bounded, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::TimedOut),
            "{export}"
        );
    }
    let start =
        module_renderer("(module (func $start (loop $forever br $forever)) (start $start))");
    let mut bounded = request();
    bounded.limits.timeout = Duration::from_millis(40);
    assert_eq!(
        start.render(&bounded, &MermaidCancellationToken::default()),
        Err(MermaidRenderError::TimedOut)
    );
}
