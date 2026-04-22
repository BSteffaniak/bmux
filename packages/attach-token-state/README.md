# bmux_attach_token_state

Neutral primitive crate hosting the trait abstractions + registry
handle for the pane-runtime plugin's attach-token surface.

`AttachTokenManager` itself (the concrete implementation) lives on
the server so it can be registered during `BmuxServer::new` before
any plugin activates. Plugins reach it through
`AttachTokenManagerHandle` looked up from the shared plugin state
registry — no cross-crate dependency on server internals required.
