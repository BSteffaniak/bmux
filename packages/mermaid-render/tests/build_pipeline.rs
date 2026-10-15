//! The artifact is embedded at compile time; no installed runtime file is read.

#[path = "../wasm_config.rs"]
mod wasm_config;

#[test]
fn aot_configuration_uses_explicit_destination_and_rejects_unknown_targets() {
    let target = env!("BMUX_MERMAID_WASM_TARGET");
    assert!(cranelift_codegen::isa::lookup(target.parse().unwrap()).is_ok());
    let engine = wasmtime::Engine::new(&wasm_config::config(target, false).unwrap()).unwrap();
    let tiny = wat::parse_str("(module (func (export \"run\")))").unwrap();
    assert!(!engine.precompile_module(&tiny).unwrap().is_empty());
    assert!(wasm_config::config("unsupported-renderer-target", false).is_err());
}

#[test]
fn cross_target_aot_compiles_for_release_destination() {
    let native = env!("BMUX_MERMAID_WASM_TARGET");
    let destination = if native.starts_with("x86_64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    let config = wasm_config::config(destination, false).unwrap();
    let engine = wasmtime::Engine::new(&config).expect("release destination compiler backend");
    let tiny = wat::parse_str("(module (func (export \"run\")))").unwrap();
    assert!(!engine.precompile_module(&tiny).unwrap().is_empty());
}

const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mermaid.wasm"));

#[test]
fn ordinary_build_embeds_a_portable_wasm_module() {
    assert!(GUEST.starts_with(b"\0asm\x01\0\0\0"));
    assert!(GUEST.len() > 8);
}

mod abi {
    use wasmtime::{Engine, Instance, Module, Store};

    #[test]
    fn guest_has_no_imports_and_owns_independent_output_buffers() {
        let engine = Engine::default();
        let module = Module::new(&engine, super::GUEST).expect("portable module compiles");
        assert_eq!(module.imports().len(), 0, "guest must not require host IO");
        let mut store = Store::new(&engine, ());
        let instance =
            Instance::new(&mut store, &module, &[]).expect("instantiate without imports");
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("memory export");
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut store, "alloc")
            .expect("alloc ABI");
        let dealloc = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "dealloc")
            .expect("dealloc ABI");
        let render = instance
            .get_typed_func::<(u32, u32), u64>(&mut store, "render")
            .expect("render ABI");
        let cases: &[(&[u8], u8)] = &[
            (b"flowchart LR\n A[Hello] --> B[World]", 0),
            (b"sequenceDiagram\n Alice->>Bob: Hello", 0),
            (b"not a mermaid diagram", 1),
            (&[0xff, 0xfe], 1),
        ];
        // Reuse the same instance to exercise allocation/deallocation ownership.
        for _ in 0..3 {
            for &(input, status) in cases {
                let len = u32::try_from(input.len()).expect("small fixture");
                let ptr = alloc.call(&mut store, len).expect("allocate input");
                memory
                    .write(&mut store, ptr as usize, input)
                    .expect("write input");
                let packed = render
                    .call(&mut store, (ptr, len))
                    .expect("render does not trap");
                let output_ptr = u32::try_from(packed >> 32).expect("high pointer");
                let output_len = u32::try_from(packed & u64::from(u32::MAX)).expect("low length");
                assert_ne!(ptr, output_ptr, "input and output must not alias");
                // Output remains live after the host frees its input.
                dealloc.call(&mut store, (ptr, len)).expect("free input");
                let mut output = vec![0; output_len as usize];
                memory
                    .read(&store, output_ptr as usize, &mut output)
                    .expect("read output");
                assert_eq!(output[0], status);
                let text = std::str::from_utf8(&output[1..]).expect("UTF-8 output");
                assert!(!text.is_empty());
                if status == 0 {
                    assert!(text.contains("<svg"));
                    assert!(text.contains("</svg>"));
                }
                dealloc
                    .call(&mut store, (output_ptr, output_len))
                    .expect("free output");
            }
        }
    }
}
