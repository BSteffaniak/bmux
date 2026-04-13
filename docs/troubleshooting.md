# Troubleshooting

Use this flow when bmux behavior is unexpected.

## 1) Check Server and Session State

```bmux-cli
bmux server status --json
bmux list-sessions --json
bmux list-clients --json
```

## 2) Inspect Terminal Compatibility

```bmux-cli
bmux terminal doctor --json --trace --trace-limit 25
```

## 3) Validate a Repro Playbook

```bmux-playbook
new-session
send-keys keys='echo hello\r'
wait-for pattern='hello'
assert-screen contains='hello'
```

## 4) Capture Logs for Investigation

```bmux-cli
bmux logs tail --since 30m --lines 250 --no-follow
```

## 5) Plugin-Specific Triage

Plugin operator index:

- `docs/plugin-ops.md`

For plugin discovery/doctor/rebuild/run issues, use:

- `docs/plugin-triage-playbook.md`

For plugin performance gate failures and baseline comparisons, use:

- `docs/plugin-perf-troubleshooting.md`
