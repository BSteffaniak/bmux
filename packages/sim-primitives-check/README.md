# bmux_sim_primitives_check

A dev-only checker that scans a crate's `src/` for primitives a deterministic simulation cannot
control: host threads, locks and clocks, host files, the environment, tokio's runtime, timers
and `select!`, tokio's `watch`, `HashMap`/`HashSet`, process-wide statics, address ordering and
operating system names.

Each crate whose code runs in a simulation calls it from a test of its own, stating its
exemptions (native backends, the modules implementing the controlled versions, test-only code).
A line may opt out with a `// simulation-safe: <reason>` comment on it or on the line above.

```rust
#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR"))
        .native(&["native/"])
        .assert_clean();
}
```
