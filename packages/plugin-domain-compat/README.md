# bmux_plugin_domain_compat

Opt-in domain-flavored surface for bmux plugins.

Core plugin infrastructure (`bmux_plugin_sdk`, `bmux_plugin`) is
strictly domain-agnostic. Plugins that want ergonomic access to legacy
session/context/pane/client domain types and helper methods opt in by
depending on this crate:

```toml
[dependencies]
bmux_plugin_domain_compat = { workspace = true }
```

```rust
use bmux_plugin_domain_compat::DomainCompat;

let sessions = caller.session_list()?;
```

The crate also hosts the canonical state types owned by foundational
plugins (`FollowState`, `ContextState`, `SessionManager`) so core and
plugins can share them without core depending on plugin crates.

Every method on `DomainCompat` is a thin wrapper over
`bmux_plugin::ServiceCaller::execute_kernel_request`. The crate becomes
deletable once every IPC variant it wraps is replaced by a typed
plugin service.
