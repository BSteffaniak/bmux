# Remote pairing and daemonless clients

Remote hosting is opt-in. Only the host needs a running bmux server; a
connecting
client does not start or require a local server. This workflow uses a
self-contained invitation over Iroh, not a short-code rendezvous service.

## First connection

On the host, start the server if necessary and explicitly enable remote access:

```sh
bmux server start --daemon
bmux remote enable
bmux remote pair
```

The last command prints a short-lived pairing link and waits. Treat that link as
sensitive: share it privately with the intended device, not in a public issue,
recording, or log. No files need to be exchanged.

On the other computer:

```sh
bmux connect --pair
```

Paste the link at the hidden prompt. Choose a saved host alias and a device
name.
Compare the displayed verification phrase on both machines before approving on
the host. Link possession alone does not authorize terminal access. The client
saves the host's identity and its own device credentials, then attaches.

On later connections:

```sh
bmux connect desktop
bmux connect desktop work
```

Use the same terminal name to resume that terminal. The host must remain running
and reachable. This is currently single-terminal access, not remote layout/tab
synchronization. Do not interpret a failed or uncertain input delivery as a safe
reason to replay that input.

## Hosting policy and startup

Remote hosting is off by default. Omitting `allow_full_runtime_ingress` allows a
trusted local owner to opt in with `bmux remote enable`; that preference
persists
across server restarts. `bmux remote disable` persists an off preference.

An explicit `allow_full_runtime_ingress = false` is a **hard prohibition**:
CLI enablement and previously saved opt-ins cannot override it. Change the
configuration and restart the host before enabling again. This differs from
omitting the field, which leaves the owner opt-in available.

To host automatically from configuration:

```toml
[plugins.settings."bmux.connections".remote]
allow_full_runtime_ingress = true
auto_start = true
bind_addr = "0.0.0.0:7443"
```

Auto-start from configuration requires an explicit, nonzero UDP bind port.
A saved CLI disable still suppresses configured auto-start until the owner
explicitly enables it again. Enabling hosting never enrolls a device by itself.

## Managing trust

Client-side saved-host commands require no local server:

```sh
bmux remote list
bmux remote status desktop
bmux remote rename desktop workstation
bmux remote forget workstation
```

Forgetting a host removes local trust information; it does not revoke the device
on the host. Host-side management remains a trusted local-owner operation:

```sh
bmux remote revoke mac
bmux remote disable
```

Pairing grants terminal resource access as the host user, not cluster membership
or cluster administration. Only approve devices you trust with that access.
Revocation must reject current authority and future admission; saved client keys
are not authority to bypass it.

## Networking and infrastructure

The default Iroh preset uses external public connectivity infrastructure.
Connections may use an encrypted relay path and upgrade to direct connectivity;
restrictive networks can require relaying. Relays cannot decrypt terminal
contents, but can observe traffic metadata, interrupt service, or be
unavailable.
No port forwarding or separate bmux pairing-mailbox deployment is needed for
this invitation format. Public relay availability is not an unconditional
connectivity guarantee.

Host relay settings belong to the connections plugin:

```toml
[plugins.settings."bmux.connections".remote.relay]
mode = "default"
```

For a managed relay, use `mode = "custom"` and `urls =
["https://relay.example/"]`.
`mode = "disabled"` is useful for direct-only/offline setups and deterministic
local testing. A pasted invitation does not override the client's local relay
policy.

## Local socket upgrade compatibility

Local IPC now requires a same-user peer and a private, owned runtime directory
(`0700` on Unix); sockets are created with mode `0600`. Existing directories
are not silently chmodded or chowned, and live sockets are not unlinked to
repair an unsafe configuration. Custom `BMUX_RUNTIME_DIR` locations remain
supported when they meet these requirements.

With `XDG_RUNTIME_DIR`, the default location is unchanged. Without it, the Unix
fallback changes from the shared temporary `bmux` directory to `bmux-UID`.
Because runtime identity is scoped to the runtime path, this fallback change
also selects a different identity/grant namespace. To retain the old namespace,
stop the old runtime, verify that the old directory belongs to your user and
has private permissions, and explicitly select it with
`BMUX_RUNTIME_DIR=/tmp/bmux` (adjust for a nonstandard temporary directory).
Do not reuse a directory owned by another user or remove another user's files.
Otherwise, use the new default and pair clients to its new identity.

## Validation boundaries

Run the isolated Unix PTY acceptance fixture against an explicit newly built
binary:

```sh
bash scripts/dev.sh cargo build -p bmux_cli --bin bmux
# In the Nix development shell:
python3 scripts/smoke-remote-pairing.py --binary "$CARGO_TARGET_DIR/debug/bmux"
```

The fixture owns all host/client state and processes; it does not manage a
user's
running server. It checks direct-loopback behavior, not public relay
reachability
or native macOS behavior. Release acceptance additionally needs native platform
CI and a two-computer, two-network trial, including a proven relay-only path,
network interruption, host restart, and revocation.
