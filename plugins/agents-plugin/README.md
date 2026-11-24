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

Every pane's state comes from what the pane shows now: the signals the pane
runtime retains for it (its title, progress report, notifications and the
agent's own reports) and its screen. Events only prompt a fresh look, so a
missed event never leaves a stale state. Two questions decide it:

1. **Is it working?** The first of these that says either way:
   - its **title** or **progress report**, by its rules (Claude Code spins a
     glyph in its title while it works);
   - **its own report** (below);
   - its **screen**, by its rules (a spinner with "esc to interrupt");
   - for an agent with no rules, whether its screen's content keeps changing
     (redraws that change nothing do not count).
2. **If it is not working, is it waiting for an answer?** A blocker on its
   screen (a permission or question dialog), or its report saying so.

Each channel is read on its own, so one cannot hide another, and a dialog-like
line in an agent's transcript cannot make a working agent look like it is
waiting. A title the pane had before the agent started counts only once it
changes. States change as soon as the pane does: there is no settling delay.

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

## Alerts

Each attach client alerts its own terminal when an agent starts waiting for
an answer or finishes its work: a bell, a desktop notification (OSC 777 or
OSC 9, whichever the terminal takes), or both. By default an agent that needs
input rings and notifies, one that finished notifies, and the tab you are
looking at stays quiet, since its screen already shows what happened. You are
looking at it only while the terminal has focus: with another window or app in
front, that tab alerts too (the terminal must report focus, as most do).
Finishing alerts only once the agent has stayed idle for a moment, so an agent
stopping to ask for permission alerts only as needing input. States agents were
already in when you attached do not alert.

```toml
[plugins.settings."bmux.agents".notify]
on = ["needs_input", "finished"]   # which changes alert
via = ["bell", "notification"]     # how, for all of them (default: as above)
when = "not_viewing"               # or "always"
```

Set `BMUX_NOTIFICATIONS` to `osc777`, `osc9` or `off` to override which
notification sequence your terminal gets. Inside tmux only the bell rings.
Pane bells reach the terminal by `behavior.bell_action`.

## Settings

```toml
[plugins.settings."bmux.agents"]
agents = ["mybot"]    # more programs to treat as agents (no screen rules)
screen_rules = true   # read agents' screens with their rules

# Other names, exact paths, or regex patterns an agent runs as. Naming a
# known agent keeps its screen rules; any other id is an agent without rules.
[plugins.settings."bmux.agents".aliases]
claude = [
  "work-claude",                                  # program name
  "/opt/ai/bin/launch-claude",                    # exact program path
  { regex = '^/nix/store/[^/]+-claude-code' },    # whole command line
]
```

A `regex` alias matches anywhere in a foreground process's whole command line
(program and arguments; anchor it with `^`/`$`), and is tried before names. An
invalid pattern rejects the settings.

`aider`, `goose`, `crush`, `omp`, `mastracode`, `openhands` and `plandex` are
recognized by name without rules.
