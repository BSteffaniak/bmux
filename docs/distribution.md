# Distribution

bmux distribution is channel-based and hosted on Cloudflare at:

- `https://packages.bmux.dev/stable`
- `https://packages.bmux.dev/nightly`

## Worker endpoints

- `GET /install` (defaults to stable)
- `GET /install?channel=nightly`
- `GET /channels.json`
- `GET /stable/*` and `GET /nightly/*` package/repository content proxied from R2

## APT

APT repositories are exposed at:

- `https://packages.bmux.dev/stable/apt`
- `https://packages.bmux.dev/nightly/apt`

Example source entry:

```text
deb [arch=amd64] https://packages.bmux.dev/stable/apt stable main
```

## RPM

RPM repositories are exposed at:

- `https://packages.bmux.dev/stable/rpm`
- `https://packages.bmux.dev/nightly/rpm`

Example repo file:

```ini
[bmux-stable]
name=bmux stable
baseurl=https://packages.bmux.dev/stable/rpm
enabled=1
gpgcheck=1
```

## npm packages

- Main packages: `bmux`, `@bmux/cli`
- Platform packages: `@bmux/<platform>`; each package contains the `bmux`
  binary, the `bmux-install` installer and `artifacts.json`.

See `npm/` and `.github/workflows/release-npm.yml`.

## Release payload contract

bmux ships as a single binary. Panes are spawned directly by the server
process; there is no separate execution supervisor or other plugin helper
executable (see `docs/development/in-server-execution-plan.md`). Plugins cannot
declare executables to ship beside `bmux`.

`cargo xtask build` is the canonical product build: the CLI and the installer
are built in one Cargo invocation with the same target and profile. The
default profile is `dev`; use `--profile release` for distribution. `--target`
defaults to the compiler host triple. Outputs live under
`$CARGO_TARGET_DIR/$TARGET/$PROFILE` (`dev` uses `debug`).

`cargo xtask stage` copies the exact build outputs into a release payload. It
does not resolve a binary from PATH, a previous installation, or a mutable
default target directory; callers pass the artifact directory explicitly.
Native developer builds must use the Nix-selected `CARGO_TARGET_DIR`; release
CI uses its pinned native target toolchain. The task itself runs on the host;
`cargo xtask build` also builds the Rust xtask for the release target, and
`stage` records that exact executable as `bmux-install` in the inventory for
Unix shell installation. The CLI and installer come from the same explicit
release output directory.

```sh
cargo xtask build --profile release --target "$TARGET"
cargo xtask stage --target "$TARGET" \
  --artifact-dir "$CARGO_TARGET_DIR/$TARGET/release" --destination "$STAGE"
```

Do not run these build commands when another coordinator owns build outputs.
Staging requires an empty directory and both regular executable artifacts;
missing artifacts fail rather than producing a nominally complete package.
`artifacts.json` records the target, exact relative paths, byte lengths and
SHA-256 hashes. Its hashes establish artifact identity, not publisher
authentication; verify the release archive's trusted checksum or signature
before installation. Inventories from older releases may also list plugin
helper executables with their declarations, and an
`unavailable_auxiliary_executables` list; the installer still reads and
verifies them, but current releases never produce them.

Release archives include the complete stage. Linux deb/rpm configurations are
rendered from their stable/nightly templates by `cargo xtask nfpm`, which adds
the inventory under `/usr/share/bmux/artifacts.json`; the templates install
`bmux` as `/usr/bin/bmux`. npm platform artifacts are transferred through
host-native `cargo xtask install`, which verifies the inventory before any
copy. The npm publish job installs the pinned host Rust toolchain deliberately:
it must not execute downloaded cross-target binaries. The launcher must execute
the package's exact installed `bmux` path, not a PATH substitute.

Windows archives/npm packages contain `bmux.exe`, `bmux-install.exe` and the
inventory.

### Explicit installation and slots

For an extracted, authenticated archive, install the entire verified inventory:

```sh
cargo xtask install /absolute/extracted/archive /absolute/install/bin
```

Install each slot version into a fresh version-specific directory and give the
CLI an explicit slot basename:

```sh
cargo xtask install /absolute/extracted/archive \
  /absolute/bmux-slots/stable-version/bin --binary-name bmux-stable
```

Set the slot manifest's `binary` to that exact installed `bmux-stable` path.
Do not replace a running slot's files. `installation.json` records the renamed
CLI; `artifacts.json` retains the original release identity. This task does not
rewrite slot configuration, start/stop servers, or change runtime endpoints.

**Curl installation:** `/install` requires curl, tar, awk, and a SHA-256 utility
(`sha256sum` or `shasum`), not Python, jq, or a Rust toolchain. It verifies
exactly
one archive `SHA256SUMS` entry **before** inspecting, extracting, or executing
the
archive. Flat-member checks reject traversal, links, special files, duplicates,
and oversized archives. The authenticated archive contains `bmux-install`, built
from the repository-owned Rust xtask for the release target (not a downloaded
arbitrary installer script). The shell invokes it only after checksum and
archive
safety validation:

```sh
/absolute/extracted/archive/bmux-install install /absolute/extracted/archive \
  --prefix /usr/local --version "$VERSION-$TARGET"
```

Rust validates the inventory schema, required artifacts, lengths and hashes
before
publication. Both checksum and archive are obtained over the configured HTTPS
publisher channel; checksums are not an independent signature/trust root.

The packaged installer publishes all validated inventory members into an
immutable
`$BMUX_INSTALL_PREFIX/lib/bmux/versions/<version>-<target>` directory (default
prefix `/usr/local`), syncs it, then atomically replaces `bin/bmux` with a
symlink
to that version's CLI. Running older binaries retain their complete
installation; no installed file is overwritten in place. Concurrent
publishers fail via an exclusive install lock. Existing immutable versions are
refused rather than changed. Old versions are retained, not automatically
deleted.
Legacy archives without an inventory are explicitly refused with a
manual-install
message; they are not silently represented as verified installations.

Existing npm launcher fallback behavior still needs separate review: packaging
complete platform files does not authorize falling back to an unrelated PATH
installation.

### Packaging validation

```sh
node --test infra/wrangler/test/install.test.mjs
```

These shell fixtures exercise checksum-before-extraction ordering, archive
safety,
and invocation of the owned Rust installer with isolated mocked downloads,
without
building Rust, contacting endpoints, or publishing packages. Rust xtask unit
tests
cover inventory identity, corruption, slot layout and generated package entries;
the coordinator runs these through the Nix-selected toolchain along with
mandated
workspace checks. Full release builds, archive/deb/rpm inspection and installed-
runtime validation remain coordinator/CI responsibilities.

## Channel manifests

Each release publishes:

- `stable/latest.json`
- `nightly/latest.json`

These are consumed by the Worker to resolve install defaults and by
`/channels.json` for clients.
