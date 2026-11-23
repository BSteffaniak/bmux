# bmux Agents

Recognizes coding agents (Claude Code, Codex, Gemini, Cursor, OpenCode and
others) running in panes and tells what each is doing: **working**,
**blocked** (it needs you to answer or approve something) or **idle**. The
tabs plugin folds each tab's most pressing agent into its status, which the
sidebar shows (see `docs/tab-presentations.md`).

```sh
bmux agents status          # every pane's agent, its state and why
bmux agents status --json
```

## Which program is an agent

Every second or so the plugin lists each pane's foreground job: the processes
in its terminal's foreground process group. A job is an agent when one of its
programs is named by an agent's rule file (its `id` or `aliases`), when a
runtime runs one (`node …/@anthropic-ai/claude-code/cli.js`, `npx
@openai/codex`, `uv run aider`), or when it is in `agents` or `aliases`
(below). Nix wrappers are seen through: `.claude-wrapped` and
`.claude-wrapped_` run `claude`. Programs
started without shell integration count too. The agent ends with its job.

## What it is doing

In order of authority:

1. **A blocker on screen.** Each agent's rule file describes its screens: a
   permission dialog, a spinner with "esc to interrupt", an empty prompt box.
   A visible blocker always wins.
2. **Visible work** on screen (a spinner, a busy title).
3. **The agent's own report** (below). A stale "working" yields to an idle
   prompt once output stops.
4. **Any other matching screen rule.** A known agent no rule matches is idle.
5. **Output.** An agent with no rules that never reported is working while it
   prints.

Going from working to idle waits about a second for the screen to settle,
unless the agent said so itself.

## Reports

A process in a pane reports its own state by writing

```text
OSC 633 ; bmux ; <$BMUX_SHELL_MARKER> ; agent ; <state> ; <base64 name> ; <base64 message> BEL
```

to its terminal, where `state` is `working`, `blocked`, `idle` or `release`
(the agent is leaving) and the name and message are optional. Every process in
a pane inherits `BMUX_SHELL_MARKER`, a secret per pane execution, so output
that merely prints such a sequence (a file being viewed, a remote shell)
cannot forge one. `bmux agents report <state> [--agent NAME] [--message TEXT]`
writes it for you. A report holds while the job that sent it runs.

### Claude Code

```sh
bmux agents install claude    # or: bmux agents uninstall claude
```

adds `~/.claude/hooks/bmux-agent-state.sh` (honoring `CLAUDE_CONFIG_DIR`) and
registers it in `settings.json` for `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `Notification` (permission prompts report
blocked, with Claude's message), `Stop` and `SessionEnd`. It keeps every other
setting and hook in place, installing twice changes nothing, and outside bmux
the script does nothing. Without it, Claude's screen rules still tell its
state; the hooks make it exact and carry its messages.

## Rules

The bundled rule files in `rules/` are herdr's agent detection manifests
(Apache-2.0, see `rules/NOTICE`), read by a compatible engine: regions of the
screen (`bottom_non_empty_lines(N)`, `prompt_box_body`,
`after_last_horizontal_rule`, `osc_title`, `osc_progress`, …) matched with
`contains`, `regex`, `line_regex`, `all`, `any` and `not`; the
highest-priority match wins. Put your own in `<config dir>/agents/<id>.toml`:
a file with a bundled `id` replaces it, any other adds an agent.

```toml
id = "mybot"
aliases = ["mybot-cli"]

[[rules]]
id = "busy"
state = "working"
priority = 900
region = "bottom_non_empty_lines(3)"
contains = ["esc to stop"]

[[rules]]
id = "approval"
state = "blocked"
priority = 950
region = "whole_recent"
line_regex = ['^\s*Allow .*\? \[y/n\]']
```

## Settings

```toml
[plugins.settings."bmux.agents"]
agents = ["mybot"]    # more programs to treat as agents (no screen rules)
screen_rules = true   # read agents' screens with their rules

# Other names, or exact paths, an agent runs as. Naming a known agent keeps
# its screen rules; any other id is an agent without rules.
[plugins.settings."bmux.agents".aliases]
claude = ["work-claude", "/opt/ai/bin/launch-claude"]
```

`aider`, `goose`, `crush`, `omp`, `mastracode`, `openhands` and `plandex` are
recognized by name without rules.
