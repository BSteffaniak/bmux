# bmux_plugin_domain_compat

Opt-in domain-compat extension for bmux plugins.

This crate exists as a temporary migration shim during the M4
"Generic Core" refactor. Core plugin infrastructure (`bmux_plugin_sdk`,
`bmux_plugin`) is strictly domain-agnostic. Plugins that still need
ergonomic access to legacy session/context/pane/client domain types
and helper methods opt in by depending on this crate:

```toml
[dependencies]
bmux_plugin_domain_compat = { workspace = true }
```

```rust
use bmux_plugin_domain_compat::DomainCompat;

let sessions = caller.session_list()?;
```

Every method here is a thin wrapper over
`bmux_plugin::ServiceCaller::execute_kernel_request`. When the core
IPC variants this crate depends on are eventually deleted (final
Stage 10 of M4), the whole crate becomes deletable.
