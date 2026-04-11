# Docs Snippet Tags

BMUX docs support opt-in snippet verification. Use these fenced code tags when
you want CI to validate examples against current APIs.

## Supported Tags

- `bmux-cli` - validates each `bmux ...` command using clap parsing.
- `bmux-playbook` - validates DSL or TOML playbook snippets.
- `bmux-config` - validates TOML snippets against config schema keys and types.

## Examples

```bmux-cli
bmux remote doctor prod --fix
bmux logs level --json
```

```bmux-playbook
new-session
send-keys keys='echo ready\r'
wait-for pattern='ready'
```

```bmux-config
[general]
scrollback_limit = 10000
```

## Authoring Notes

- Keep commands one per line in `bmux-cli` blocks.
- Use regular markdown fences (`bash`, `sh`, `toml`) for examples you do not
  want validated.
- Tag only high-value snippets first, then expand coverage over time.

CI also prints a coverage report showing how many fenced code blocks are
opt-in validated across docs files.

In GitHub Actions, this report is uploaded as `docs-snippet-coverage` with:

- `docs-snippet-coverage.md`
- `docs-snippet-coverage.json`
