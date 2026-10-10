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

Tests are ordinary Rust unit tests under `xtask/src/tests.rs`; the public shell
consumer maintains its own checksum/archive-security fixtures.
