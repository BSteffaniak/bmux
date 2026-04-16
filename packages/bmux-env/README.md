# bmux-env

Pure-printer PATH / env helper for bmux slot-based installs.

Emits shell code (`bmux-env shell`), launches a command with a slot's env
applied (`bmux-env exec`), or prints the resolved environment as structured
data (`bmux-env print --format json|nix|shell|fish`).

Never writes to disk. Nix-friendly by design.
