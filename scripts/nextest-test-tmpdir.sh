#!/bin/sh
# nextest setup script: give every test a short, canonical, private TMPDIR.
#
# Platform temporary directories differ in ways tests should not depend on:
# macOS uses /var/folders/... and /tmp, both behind symlinks (to /private/...),
# and `nix develop` points TMPDIR at /tmp/nix-shell.*. Symlinked ancestors trip
# code that deliberately refuses them (private caches, retained executables),
# canonical-path comparisons see /private/tmp where tests built /tmp, and long
# roots push Unix socket paths past the platform limit (104 bytes on macOS).
#
# One private per-user directory under /tmp, resolved to its real path, keeps
# temporary paths short and symlink-free for the whole run. Tests still create
# their own unique directories inside it (tempfile), so concurrent runs share it
# safely, and nothing accumulates beyond what tests fail to clean up.
set -eu

: "${NEXTEST_ENV:?nextest setup scripts receive NEXTEST_ENV}"

dir="/tmp/bmux-test-$(id -u)"
if [ ! -e "$dir" ]; then
    mkdir -m 700 "$dir" 2>/dev/null || true
fi
# Never adopt a directory another user (or a symlink) could control.
if [ -L "$dir" ] || [ ! -d "$dir" ] || [ -z "$(find "$dir" -prune -user "$(id -u)")" ]; then
    echo "nextest-test-tmpdir: $dir is not a private directory owned by $(id -un)" >&2
    exit 1
fi
chmod 700 "$dir"
dir="$(cd "$dir" && pwd -P)"
printf 'TMPDIR=%s\n' "$dir" >>"$NEXTEST_ENV"
