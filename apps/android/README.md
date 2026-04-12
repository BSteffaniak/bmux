# bmux Android app (planned)

Android client for connection-first bmux remote workflows.

This app is planned for M4. M0 locks architecture and API contracts.

Product direction for v1:

- quick target import and connection
- iroh, SSH, and LAN TLS connectivity
- session selection and reconnect workflows
- mDNS discovery support
- strict TLS verification with optional pinning
- background reconnect via Android foreground service

Terminal emulation is out of scope for v1, but architecture should not block it.

See:

- `docs/mobile-m0-architecture.md`
- `docs/mobile-ffi-contract.md`
