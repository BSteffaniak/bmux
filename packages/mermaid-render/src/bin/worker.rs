//! Standalone packaging shim for the shared Mermaid worker.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]

fn main() {
    bmux_mermaid_render::worker_main();
}
