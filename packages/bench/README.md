# BMUX benchmark library

BMUX-owned deterministic terminal workloads, bounded measurements, versioned
artifacts,
and uncertainty-aware comparisons. No BMUX server or domain plugins are
required.

```rust,no_run
use bmux_bench::{analysis, artifact, config, runner, terminal};
use std::sync::atomic::AtomicBool;

let config = config::resolve(None, "quick")?;
let mut terminal = terminal::TerminalBackend::open()?;
let record = runner::run(&mut terminal, config, "my-terminal".into(),
    &AtomicBool::new(false), |_| {})?;
artifact::write_bundle(std::path::Path::new("results/my-run"), &record)?;
let summary = analysis::summarize(&record)?;
# Ok::<(), String>(())
```

The optional `internal` feature enables parser/grid, snapshot, neutral TUI
paint,
and ANSI encoding diagnostics without terminal I/O. Portable write acceptance
and
protocol round trips never establish visible rendering completion.

See [the frontend documentation](../bench-cli/README.md) for commands, profiles,
artifact contracts, interpretation, and limitations. All bundled workloads are
BMUX-authored synthetic generators; no network corpus download is required.

Methodology references: [vtebench](https://github.com/alacritty/vtebench),
[TermBench](https://github.com/cmuratori/termbench), and
[Kitty performance](https://sw.kovidgoyal.net/kitty/performance/).
These inform workload categories and measurement boundaries, not borrowed scores
or redistributed corpora. The ANSI scrolling scenario also draws on the owner's
RC benchmark, replacing its shell loop with deterministic Rust generation.
