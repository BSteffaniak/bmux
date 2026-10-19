# Release tasks

These are explicit repository tasks, not Cargo build-script hooks. The task uses
`bmux_plugin::PluginManifest` for the same declaration/ABI validation as the host;
it does not initialize a plugin runtime or contact a daemon.

```
cargo xtask auxiliary build --target TARGET
cargo xtask auxiliary stage --target TARGET --artifact-dir DIR --destination DIR
cargo xtask auxiliary nfpm --target TARGET --config FILE --destination FILE
cargo xtask install SOURCE DESTINATION [--binary-name bmux-SLOT]
cargo xtask install SOURCE --prefix PREFIX --version VERSION
```

Auxiliary operations accept `--workspace ROOT` (default: this repository).
`--activate-version VERSION` is an alias for `--version`. Version activation and
slot renaming are mutually exclusive. Install destinations must be absent or
empty dedicated directories. Unix activation retains immutable version directories
and atomically replaces only `PREFIX/bin/bmux`, under an exclusive publication
lock. A failed activation may retain a fully published but inactive version;
retrying that immutable version is explicitly refused, never overwritten.

Build selects `xtask` and each declared Cargo package/binary explicitly, using
`--locked --release --target`. Stage renames the target `xtask` binary to
`bmux-install` (`.exe` on Windows); invocation under that basename defaults to the
install task, without an `install` subcommand. The host task can install cross-target
inventories without executing their binaries. Windows inventories include only
`bmux.exe` and `bmux-install.exe`, with unavailable helper declarations recorded;
symlink activation is Unix-only. Supported targets are x86_64/aarch64 Darwin and
Linux GNU/musl, and x86_64 Windows MSVC.

The inventory is schema 1 and includes SHA-256, length, target, and declarations.
Install verifies every member, rejects unlisted payload members, unsafe/duplicate
names and symlinked files or path ancestors, then copies into a private staging
directory and verifies the copied bytes before atomic publication. Existing files
are never overwritten. On Unix files and directories are synchronized before
success. Windows uses atomic directory publication but does not claim POSIX
symlink activation or directory-fsync semantics. Installation prefixes must be
locally owned/trusted; pathname checks do not confer protection against an actor
with concurrent write authority over an ancestor directory.

Public download consumers must verify the archive checksum and reject unsafe
archive entries **before** extraction and execution of `bmux-install`. Inventory
hashes alone are not an authenticity signature. No Python installer is shipped.


## Bterm bundles

Inside the development shell, build and launch natively with one command:

```sh
cargo xtask bterm run
```

This detects the compiler host and Cargo target directory, incrementally builds
both binaries, stages and verifies a fresh bundle, and launches it directly so
errors remain visible. Repeat runs replace the previous development bundle
(including stale files); failed staging leaves the old bundle intact. Concurrent
runs are rejected while a window is open. The bundle lives under Cargo's target
directory at `bterm-run/<host>/Bterm.app` on macOS (`Bterm` on Linux). Persistent
state lives separately under `${XDG_STATE_HOME:-$HOME/.local/state}/bterm-dev/`,
keyed by the canonical checkout path; rebuilding or cleaning Cargo artifacts
never replaces it. Socket paths are length-checked before launch. Closing the
window detaches without stopping that runtime. An
incompatible existing runtime is reported, not silently replaced or redirected.
Windows native execution is not currently supported by this command.

Low-level packaging operations (staging requires a new destination):

```sh
cargo xtask bterm stage --target TARGET --artifact-dir DIR --destination ABSENT_DIR
cargo xtask bterm verify --target TARGET --root EXECUTABLE_DIRECTORY
```

Staging copies the actual `bterm` and `bmux` binaries, four bundled JetBrains
Mono Nerd Font faces, and font/project notices. Static plugins remain part of
the BMUX companion. Both processes retain independent lifetimes; packaging never
starts a runtime. The schema-1 `bterm-bundle.json` beside the executables records
`bterm-bmux-v1`, the exact target, and relative paths with SHA-256 checksums.
This is corruption detection, not signing or publisher authentication. Runtime
compatibility is separately negotiated over IPC.

Linux includes `share/applications/bterm.desktop`; a distributor must expose the
bundle's `bterm` on its launcher PATH. macOS produces `Contents/MacOS` plus an
Info.plist; use a `.app` destination. Windows uses explicit `.exe` paths. No
installer, signing, registration, or live user-directory mutation is performed.
Verification rejects traversal, symlinks, duplicate paths/required roles, missing
companions, wrong targets and corrupt bytes without executing either binary.

The Bterm bundle CI builds real binaries and checks staged executable `--help`
on Linux/macOS/Windows with the flake-selected compiler. Windows backend
availability remains an application-level explicit error, not a claimed working
GUI. Fixture tests cover all layouts on every host; native window/device testing
still requires each platform and is not implied by bundle verification.
