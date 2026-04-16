# Kiosk Access

Use kiosk mode to grant SSH users controlled, token-based access to bmux sessions without exposing a general remote shell.

## What Kiosk Mode Does

- Enforces a forced-command entrypoint (`bmux kiosk attach ...`) for SSH entry.
- Supports multiple named profiles with independent defaults for session, target, SSH user, and token policy.
- Uses short-lived tokens (one-shot by default) that can be revoked at any time.
- Re-applies attach policy after reconnects, including remote targets.

## Quick Start

1. Configure kiosk defaults + one profile in `bmux.toml`.
2. Generate SSH include/wrapper assets.
3. Include generated SSH config in your `sshd_config`.
4. Issue a kiosk token for the profile.
5. Add the printed `authorized_keys` forced-command entry.

```bmux-config
[kiosk.defaults]
enabled = true
ssh_user = "bmux-kiosk"
role = "observer"
allow_detach = false
token_ttl_secs = 900
one_shot = true

[kiosk.profiles.demo]
session = "demo"
target = "prod"
```

```bmux-cli
bmux kiosk status
bmux kiosk init --all-profiles --dry-run
bmux kiosk init --all-profiles
bmux kiosk issue-token demo
bmux kiosk ssh-print-config --all-profiles
```

## Profile Model

Kiosk config lives under `[kiosk]`:

- `[kiosk.defaults]` applies shared defaults for all profiles.
- `[kiosk.profiles.<name>]` applies per-profile overrides.
- `[kiosk.files]` customizes output locations used by `bmux kiosk init`.

Use one default SSH user via `kiosk.defaults.ssh_user` unless you need per-profile Unix users:

```bmux-config
[kiosk.defaults]
ssh_user = "bmux-kiosk"

[kiosk.profiles.demo]
ssh_user = "demo-kiosk"

[kiosk.profiles.readonly]
ssh_user = "bmux-kiosk"
role = "observer"
```

## Bootstrap Files (`kiosk init`)

`bmux kiosk init` generates two artifact types:

- sshd include content (`Match User` blocks + `ForceCommand`)
- shell wrappers (one script per selected profile)

Useful commands:

```bmux-cli
bmux kiosk ssh-print-config --all-profiles
bmux kiosk init --all-profiles --dry-run
bmux kiosk init --all-profiles
bmux kiosk init --profile demo --yes
```

Notes:

- Interactive mode prompts before writing unless `--yes` is provided.
- Non-interactive execution requires `--yes`.
- Wrapper scripts require `BMUX_KIOSK_TOKEN` and run `bmux kiosk attach <profile> --token ...`.

## Token Lifecycle

Issue, use, and revoke tokens with kiosk commands:

```bmux-cli
bmux kiosk issue-token demo
bmux kiosk issue-token demo --session hotfix --ttl-secs 600 --multi-use
bmux kiosk revoke-token <token-id>
```

Token behavior:

- Format is `k1.<token_id>.<secret>`.
- Secret is hashed at rest in local token store.
- Expired, revoked, or consumed one-shot tokens are rejected.

## SSH Integration

`bmux kiosk issue-token` prints an `authorized_keys` example you can use directly:

```text
restrict,command="bmux kiosk attach demo --token <token>" <public-key>
```

Pair this with the generated sshd include file from `bmux kiosk init`.

## Attach Behavior and Security Semantics

- `allow_detach = false` blocks detach for that kiosk connection.
- Kiosk attach sets policy before attach and re-applies it after reconnect.
- If a profile pins `target`, conflicting `--target` overrides are rejected.
- Remote-target kiosk attaches reconnect with bounded retry/backoff behavior.

## Security Hardening Checklist

> Use these defaults for production-style kiosk access.
>
> - Keep `allow_detach = false` unless users explicitly need detach.
> - Keep `one_shot = true` and use short `token_ttl_secs` values.
> - Pin `target` (and optionally `session`) in each production profile.
> - Use dedicated SSH users for sensitive environments.
> - Prefer generated sshd settings that disable forwarding/tunneling features.
> - Revoke tokens immediately after support or demo windows close.

## Troubleshooting

- `unknown kiosk profile` — check `bmux kiosk status` and profile names.
- `unknown kiosk token id` — token not issued on this machine/state.
- `kiosk token is expired` — issue a new token or increase TTL.
- `kiosk token already used` — issue another token or use `--multi-use`.
- `kiosk init requires --yes in non-interactive mode` — rerun with `--yes`.

## Operator Checklist

1. Keep kiosk enabled only where needed.
2. Use one-shot tokens by default.
3. Keep TTL short for interactive support access.
4. Revoke tokens after support windows end.
5. Prefer pinned profile targets for production environments.
