# Command Cookbook

Task-oriented command recipes you can copy, adapt, and automate.

## Session Lifecycle

```bmux-cli
bmux new-session dev
bmux attach dev
bmux list-sessions --json
```

## Remote Target Workflow

```bmux-cli
bmux remote list --json
bmux remote test prod
bmux connect prod app
```

## Hosted Flow

```bmux-cli
bmux setup --mode p2p
bmux host --status
bmux hosts
```

## Logging and Diagnostics

```bmux-cli
bmux logs path --json
bmux logs level --json
bmux logs tail --since 15m --lines 200 --no-follow
```
