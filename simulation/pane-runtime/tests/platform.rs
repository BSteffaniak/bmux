//! The pane runtime's platform conformance scenarios against its simulated target, under every
//! personality of the simulated kernel (the plugin's own tests run them natively on each
//! operating system).

use switchy_pty::simulator::{self, Config, Faults, Personality};

#[test]
fn the_simulated_target_passes_every_scenario_as_every_system() {
    for personality in [Personality::Linux, Personality::MacOs, Personality::Windows] {
        for seed in 0..6 {
            let outcome = switchy_thread::simulator::run(seed, || {
                switchy_time::simulator::set_step_multiplier(1);
                simulator::configure(Config {
                    personality,
                    pid_reuse: 0.5,
                    faults: Faults::default(),
                });
                simulator::install_default_programs();
                let results = bmux_pane_runtime_plugin::simulation::platform_conformance();
                simulator::kill_all();
                switchy_thread::simulator::run_until_idle();
                results
            });
            let results = match outcome.result {
                Ok(results) => results,
                Err(panic) => std::panic::resume_unwind(panic),
            };
            let failures: Vec<String> = results
                .into_iter()
                .filter_map(|(name, result)| result.err().map(|error| format!("{name}: {error}")))
                .collect();
            assert!(
                failures.is_empty(),
                "{personality:?}, seed {seed}: {failures:#?}"
            );
            assert!(
                outcome.report.leaked.is_empty(),
                "{personality:?}, seed {seed}: leaked {:?}",
                outcome.report.leaked
            );
        }
    }
}
