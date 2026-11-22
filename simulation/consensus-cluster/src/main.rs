//! Runs the owned-consensus cluster simulation; see the library documentation.
//!
//! `cargo sim consensus-cluster` runs it, configured through simvar's `SIMULATOR_*`
//! environment (`SIMULATOR_RUNS`, `SIMULATOR_SEED`, ...); `--nodes 5` runs five voters.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use consensus_cluster::ClusterBootstrap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let nodes = match args.as_slice() {
        [] => 3,
        [flag, count] if flag == "--nodes" => count.parse()?,
        _ => return Err("usage: consensus-cluster [--nodes N]".into()),
    };
    let results = simvar::run_simulation(ClusterBootstrap { nodes })?;
    let failed: Vec<_> = results.iter().filter(|r| !r.is_success()).collect();
    for result in &failed {
        println!("{result}");
    }
    println!("{} runs, {} failed", results.len(), failed.len());
    if failed.is_empty() {
        Ok(())
    } else {
        Err("consensus cluster simulation failed".into())
    }
}
