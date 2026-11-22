//! Model checks the owned consensus engine; see the library documentation.
//!
//! `cargo sim consensus` runs an exhaustive breadth-first search over small bounds,
//! then seeded random walks over deeper ones (with learners, membership change,
//! duplication and more crashes). `SIMULATOR_SEED` reproduces the walks and
//! `SIMULATOR_EXPLORE_*` adjusts the checker. `--walks-only` skips the search.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use consensus::{Bounds, Raft};
use simvar::explore::Checker;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags: Vec<String> = std::env::args().skip(1).collect();
    if let Some(unknown) = flags.iter().find(|flag| *flag != "--walks-only") {
        return Err(format!("unknown option {unknown}; expected --walks-only").into());
    }
    let mut failed = false;
    if flags.is_empty() {
        for (name, bounds) in [
            ("fault-free", Bounds::exhaustive_fault_free()),
            ("crash and loss", Bounds::exhaustive()),
        ] {
            let report = Checker::from_env().bfs_parallel(&Raft { bounds });
            println!("exhaustive search ({name}):\n{report}");
            failed |= !report.is_success();
        }
    }
    let report = Checker::new()
        .walks(2_000)
        .max_depth(400)
        .env_overrides()
        .random_walk(&Raft {
            bounds: Bounds::walk(),
        });
    println!("random walks:\n{report}");
    failed |= !report.is_success();
    if failed {
        return Err("consensus model check failed".into());
    }
    Ok(())
}
