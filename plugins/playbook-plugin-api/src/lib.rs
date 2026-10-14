//! Typed managed-playbook lifecycle. Runtime implementation lives in the plugin.
//!
//! `watch` is bounded snapshot polling: clients retain the last revision and
//! discard old generations. It is not a lossy event queue or a terminal surface.
//! `paused` retains an in-progress typing cursor; `next-step` executes one whole
//! action (including waits), never seeks through arbitrary side effects.
//! `stop` records cancellation intent; only `cleanup_acknowledged` permits restart.
//! Presentation attaches to the returned isolated BMUX runtime using its normal
//! committed-display protocol, not this service's diagnostics or runner output.

bmux_plugin_schema_macros::schema! {
    source: "bpdl/playbook-plugin.bpdl",
}
