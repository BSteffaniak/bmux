//! Identical build-time and runtime compiler settings for embedded guests.

pub fn config(target: &str, fuel: bool) -> wasmtime::Result<wasmtime::Config> {
    let mut config = wasmtime::Config::new();
    // An explicit target disables host CPU feature inference. Artifacts use the
    // destination baseline, not the machine that happens to build the package.
    config.target(target)?;
    config
        .strategy(wasmtime::Strategy::Cranelift)
        .cranelift_opt_level(wasmtime::OptLevel::Speed)
        .epoch_interruption(true)
        .max_wasm_stack(2 * 1024 * 1024)
        .consume_fuel(fuel);
    Ok(config)
}
