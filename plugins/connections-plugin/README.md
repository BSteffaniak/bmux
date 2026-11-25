# bmux_connections_plugin

Bundled remote access for bmux (experimental). See `docs/remote-pairing.md` for
the user guide and `docs/development/remote-access-architecture.md` for the
design.

## Modules

- `identity`: the machine's host key and per-profile device keys (Iroh
  endpoint keys), and the host lock (one hosting runtime per machine).
- `trust`: the host trust store (device key -> principal, name, state,
  revision), in memory with write-through to redb; publishes changes.
- `known_hosts`: the connecting side's saved hosts (pinned host key, address
  and relay hints, principal).
- `link`, `pairing`: the `bmux-pair:` link and the `bmux/pair/1` exchange
  (transcript, proof, phrase, bounded sessions).
- `transport`: the listener (`bmux/session/1` streams handed to the server
  through `StreamIngressHandle` with a revocable binding per connection) and
  the connection manager (one endpoint per process, one connection per host).
- `host`: hosting policy, pairing approval (pending -> role -> active), revoke
  and role changes.
- `connector`: the saved-host `TransportConnector` for `--target <alias>`.
- `audit`: the host's audit log (pairings, revocations, role changes,
  connections and refusals), stored bounded in the trust store's database,
  with refusals rate-limited and no secrets.
- `cli`: `connect`, `remote …` and `kiosk …` commands; `cli::status` is
  `remote status` and `remote log` (both also work without a server, from
  stored state), and `cli::doctor` is `remote doctor <host>`, the timed
  step-by-step diagnosis of a saved host.
- `invoke`: `connections-commands.invoke-service` on `local` or a saved host.
