# bmux_connections_plugin

Bundled implementation of the transport-neutral `bmux.connections` domain.

It resolves local, SSH, TLS, and Iroh targets from bmux configuration and invokes an opaque typed service request on one selected endpoint. Selection policy remains with callers such as the cluster plugin; this crate does not contain cluster, workspace, placement, or gateway policy.
