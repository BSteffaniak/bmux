# Remote access and pairing (experimental)

> **Experimental.** Remote access is being rebuilt (see
> `docs/development/remote-access-architecture.md`). Formats and commands may
> change; pairings made with earlier versions do not carry over.

Pair a device once, then `bmux connect desktop` gives the same attach as
running `bmux` on the desktop. Only the host runs a bmux server. The connecting
device needs no server, account or port forwarding: the two machines talk over
[Iroh](https://iroh.computer) (QUIC), directly when they can and through a
relay otherwise.

## Pair a device

On the host, enable hosting once (it stays enabled across restarts):

```sh
bmux remote enable
```

Then open a pairing session:

```sh
bmux remote pair                     # full access
bmux remote pair --role view-only    # watch only
bmux remote pair --name laptop       # name the device yourself
```

`remote pair` prints `Pairing link: bmux-pair:…` and a QR code, then waits.
The link carries the host key, address hints and a 256-bit secret. It works
once and expires after 10 minutes. Treat it as a secret: share it privately,
never in an issue, recording or log.

On the other device:

```sh
bmux connect --pair                          # saves the host under its name
bmux connect --pair --name desk              # choose the saved name
bmux connect --pair --device-name work-mac   # what the host owner sees
```

Paste the link at `Pairing link (input hidden): `. The device proves it holds
the link's secret, and both sides show the same phrase:

- the device prints `Verify on the host: xxxx-xxxx-xxxx-xxxx`;
- the host prints `Compare this phrase on the other device: xxxx-xxxx-xxxx-xxxx`
  and asks to approve.

Approve only when the phrases match. The device then prints
`Paired. Saved host: <name>` and attaches.

The phrase is derived from the host key, the device key, the link secret, the
requested role and the protocol version, so a copied link used from another
device shows a different phrase. The host refuses a session after five wrong
proofs and holds at most four waiting requests.

## Connect

```sh
bmux connect desk            # attach to the host's sessions
bmux connect desk work       # attach to session `work`
bmux --target desk list-sessions
```

Every attach feature works, because it is the normal attach running over a
stream of the QUIC connection. One endpoint and one connection per host are
shared by every client in a process.

## Roles and devices (on the host)

| Role | Can |
| --- | --- |
| `full` | observe, type, and change the runtime (acts as the host user) |
| `view-only` | observe only |

Owner-local operations (pairing, enabling or disabling hosting, revoking,
changing roles, plugin and configuration management, stopping the server) are
never reachable from a paired device, whatever its role.

```sh
bmux remote list                      # paired devices
bmux remote role laptop view-only     # takes effect on the next request
bmux remote revoke laptop             # closes its connections now
bmux remote status                    # hosting, host key, devices, live connections
bmux remote log                       # audit log: pairings, revocations, connections
bmux remote disable                   # stops hosting and closes every remote connection
```

A revoked device is refused right after the QUIC handshake when it reconnects,
and is told that it was revoked.

## Status, diagnosis and the audit log

`bmux remote status` (add `--json` for scripts) shows:

- whether hosting is on, and why: `remote enable` / `remote disable` (the CLI
  opt-in), `config: auto_start`, `config: allowed`, `config: prohibited`
  (`allow_full_runtime_ingress = false`), or `default` (off);
- this machine's host key (its endpoint id) and bound addresses;
- the relay mode, the home relay, whether it is connected, and its latency;
- paired devices with role, state and when each was last seen;
- live remote connections with their path (`direct` or `relay`) and RTT;
- this device's key and the hosts it has saved.

It also works without a running server: it then shows what this machine stored
(the hosting choice, host key and paired devices) and the saved hosts, and
creates nothing.

`bmux remote doctor <host>` (on the connecting device; `--json`,
`--timeout SECS`) checks a saved host step by step and times each step:
`resolve` (the saved host and this device's key), `endpoint`, `relay`, `dial`,
`handshake` (proves the pinned host key; shows the path and RTT), `hello` (the
host admits this device) and `round-trip` (one typed call). It stops at the
first failure and names it:

| Failure | Meaning |
| --- | --- |
| `unknown_host` | no saved host by that name (`bmux remote hosts`) |
| `host_offline` | nothing answered: the host is off, unreachable, or has remote access disabled |
| `relay_unreachable` | this device reaches no relay and the host's direct addresses did not answer |
| `host_key_changed` | the host presents a different key than the pinned one |
| `device_revoked` | the host revoked this device; pair again |
| `device_not_paired` | the host does not know this device |
| `incompatible_version` | the host speaks a different remote protocol; update bmux on both machines |

It exits non-zero on a failure, needs no local server, and changes nothing.

`bmux remote log [--limit N] [--json]` (on the host) lists the newest audit
events, oldest first: `paired`, `pair-rejected`, `revoked`, `role-changed`,
`connected` (with path and RTT), `disconnected` (with why and how long) and
`refused` (an unpaired or revoked device), each with the time, device name and
key, and role. The log keeps the newest 1000 events in the trust store,
holds no secrets (no pairing links or phrases), records at most 20 refusals a
minute, and can be read without a running server.

## Saved hosts (on the connecting device)

```sh
bmux remote hosts
bmux remote rename desk desktop
bmux remote forget desktop
bmux remote status                    # also lists saved hosts and this device's key
bmux remote doctor desktop            # why can't I connect?
```

These never need a server. `forget` only removes the host from this device; the
host still lists the device until its owner runs `bmux remote revoke`.

If a saved host answers with a different host key, the connection stops with
an error: the host's key was reset, or something else answers at its address.
Forget it and pair again if the reset was intended.

## Settings

In `bmux.toml`, under the connections plugin:

```toml
[plugins.settings."bmux.connections".remote]
# Hosting is off until `bmux remote enable`. `false` forbids it outright.
# allow_full_runtime_ingress = false
# Bind only this address (host listener and the connecting endpoint).
# bind_addr = "0.0.0.0:7842"
# Extra addresses to advertise in pairing links, for example a port forward.
# external_addrs = ["203.0.113.7:7842"]
# Start hosting with the server (needs allow_full_runtime_ingress = true and
# a fixed bind_addr).
# auto_start = false

[plugins.settings."bmux.connections".remote.relay]
mode = "default"          # the public Iroh relays
# mode = "custom"         # your own relays (HTTPS; plain HTTP only on 127.0.0.0/8 or ::1)
# urls = ["https://relay.example.com"]
# mode = "disabled"       # direct connections only
```

## Where things live

- Host key: `<state dir>/connections/host.key` (one per machine and user; only
  one runtime hosts at a time).
- Device key: `<state dir>/connections/devices/<profile>.key`.
- Paired devices and the audit log (host): `<state dir>/connections/trust.redb`.
- Saved hosts (device): `<state dir>/connections/known-hosts/<profile>.json`.

All of these are owner-only. Keys and link secrets never appear in listings,
logs or errors.

## Testing

The end-to-end acceptance tests drive real host and client processes over
loopback:

```sh
cargo nextest run -p bmux_cli --test remote_access
```
