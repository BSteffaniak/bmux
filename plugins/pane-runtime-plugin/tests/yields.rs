//! Every `.await` the pane runtime makes is a point where a simulation may let another task run.
//!
//! `src/lib.rs` loads each of the plugin's modules with `switchy_async::inject_yields_mod!`,
//! which (under the simulator only; natively it expands to the declaration unchanged) rewrites
//! every `.await` inside, in `async fn`s, `async` blocks and `async` closures alike, into a
//! seeded yield point. This test fails on any async code outside `#[cfg(test)]` that no such
//! module or `#[switchy_async::inject_yields]` attribute covers, so new code cannot slip past
//! the transformation.

use std::path::Path;

use switchy_async_cargo::{Finding, Kind};

/// Async code left uncovered on purpose: `(file suffix, enclosing function, reason)`.
const EXEMPT: &[(&str, &str, &str)] = &[];

fn exempt(finding: &Finding) -> bool {
    EXEMPT.iter().any(|(file, name, _reason)| {
        finding.file.ends_with(file) && finding.name.as_deref() == Some(*name)
    })
}

#[test]
fn every_await_outside_tests_can_yield_in_simulation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let findings = switchy_async_cargo::uncovered(root);
    let uncovered: Vec<String> = findings
        .iter()
        .filter(|finding| finding.kind == Kind::Unreadable || !finding.cfg_test)
        .filter(|finding| !exempt(finding))
        .map(ToString::to_string)
        .collect();
    assert!(
        uncovered.is_empty(),
        "async code no `inject_yields` covers (load its module with \
         `switchy_async::inject_yields_mod!` in src/lib.rs, or put \
         `#[switchy_async::inject_yields]` on it):\n{}",
        uncovered.join("\n")
    );
}

#[test]
fn exemptions_name_real_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let findings = switchy_async_cargo::uncovered(root);
    for (file, name, reason) in EXEMPT {
        assert!(
            !reason.is_empty(),
            "exemption {file}::{name} needs a reason"
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.file.ends_with(file)
                    && finding.name.as_deref() == Some(*name)),
            "exemption {file}::{name} matches nothing uncovered: remove it"
        );
    }
}
