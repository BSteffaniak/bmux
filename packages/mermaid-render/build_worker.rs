//! Native stack ownership for build-time Wasmtime/Cranelift compilation.

use std::{io, panic, thread};

// Windows executable main stacks can be smaller than Cranelift's compilation
// needs, especially for the large embedded guest in unoptimized build scripts.
// This is a native compiler stack, unrelated to Config::max_wasm_stack (guest
// execution). Own it explicitly rather than relying on linker or environment
// defaults. The worker is joined before its borrowed inputs can be released.
const COMPILER_STACK_BYTES: usize = 16 * 1024 * 1024;

pub fn run<T: Send>(compile: impl FnOnce() -> T + Send) -> io::Result<T> {
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("mermaid-aot-build".into())
            .stack_size(COMPILER_STACK_BYTES)
            .spawn_scoped(scope, compile)?;
        match worker.join() {
            Ok(result) => Ok(result),
            // Do not silently turn compiler panics into success or a JIT fallback.
            Err(payload) => panic::resume_unwind(payload),
        }
    })
}
