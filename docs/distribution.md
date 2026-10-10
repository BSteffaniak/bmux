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
- Platform packages: `@bmux/<platform>`; each Unix package contains the CLI and
  all manifest-declared executable siblings plus `artifacts.json`.

See `npm/` and `.github/workflows/release-npm.yml`.

## Auxiliary executable release contract

Plugins declare executable artifacts in top-level `[[auxiliary_executables]]`
entries in `plugins/*/plugin.toml`. The generic declaration fields are `id`,
`cargo_package`, `cargo_binary`, `relative_path`, and positive `protocol_version`.
IDs and destination names are validated; destination paths are single safe
basenames, never traversal paths. Duplicate destinations fail the release.
The pane plugin owns `bmux-pane-execution-supervisor`; release infrastructure does
not special-case its domain or protocol.

`cargo xtask auxiliary` discovers these manifests, builds each
specified Cargo binary for the release target, and stages the exact build outputs
alongside BMUX. It does not resolve a binary from PATH, a previous installation,
or a mutable default target directory. Callers pass the artifact directory
explicitly. Native developer builds must use the Nix-selected `CARGO_TARGET_DIR`;
release CI uses its pinned native target toolchain. The task itself runs on the
host; `auxiliary build` also builds the Rust xtask for the release target and
`auxiliary stage` records that exact executable as `bmux-install` in the inventory
for Unix shell installation. The CLI, helpers, and installer all come from the
same explicit release output directory.

```sh
cargo xtask auxiliary build --target "$TARGET"
cargo xtask auxiliary stage --target "$TARGET" \
  --artifact-dir "$CARGO_TARGET_DIR/$TARGET/release" --destination "$STAGE"
```

Do not run these build commands when another coordinator owns build outputs.
Staging requires an empty directory and all required regular executable artifacts;
missing artifacts fail rather than producing a nominally complete package.
`artifacts.json` records target, exact relative paths, byte lengths, SHA-256 hashes,
and owner/declaration/protocol metadata. Its hashes establish artifact identity,
not publisher authentication; verify the release archive's trusted checksum or
signature before installation.

Release archives include the complete stage. Linux deb/rpm configurations are
rendered from their stable/nightly templates with the same generic declarations:
executables install beside `/usr/bin/bmux`; inventory installs under
`/usr/share/bmux/artifacts.json`. npm platform artifacts are transferred through
host-native `cargo xtask install`, which verifies the inventory before any copy.
The npm publish job installs the pinned host Rust toolchain deliberately: it must
not execute downloaded cross-target binaries. Unix npm packages keep auxiliaries
beside the package's actual BMUX binary.
The launcher must execute that exact installed path, not a PATH substitute.

Auxiliary execution is supported here only for Linux and macOS release targets.
Windows archives/npm packages contain `bmux.exe` and an inventory explicitly
listing declared auxiliaries as unavailable. No supervisor stub is built or
advertised as Windows support. Unsupported platforms must return explicit feature
unavailability; baseline BMUX operation remains independent of the helper.

### Explicit installation and slots

For an extracted, authenticated archive, install the entire verified inventory:

```sh
cargo xtask install /absolute/extracted/archive /absolute/install/bin
```

Slot versions must not share a physical helper directory. Install into a fresh
version-specific directory and give the CLI an explicit slot basename:

```sh
cargo xtask install /absolute/extracted/archive \
  /absolute/bmux-slots/stable-version/bin --binary-name bmux-stable
```

Set the slot manifest's `binary` to that exact installed `bmux-stable` path.
Helpers retain their manifest basenames in that same directory. Do not copy only
the CLI into a shared `/usr/local/bin`, rename a helper to another slot's helper,
or replace a running slot's files. `installation.json` records the renamed CLI;
`artifacts.json` retains the original release identity. This task does not
rewrite slot configuration, start/stop servers, or change runtime endpoints.
The host resolver must select only the verified sibling of the selected executable
and reject absent/incompatible artifacts instead of falling back to PATH.

**Curl installation:** `/install` requires curl, tar, awk, and a SHA-256 utility
(`sha256sum` or `shasum`), not Python, jq, or a Rust toolchain. It verifies exactly
one archive `SHA256SUMS` entry **before** inspecting, extracting, or executing the
archive. Flat-member checks reject traversal, links, special files, duplicates,
and oversized archives. The authenticated archive contains `bmux-install`, built
from the repository-owned Rust xtask for the release target (not a downloaded
arbitrary installer script). The shell invokes it only after checksum and archive
safety validation:

```sh
/absolute/extracted/archive/bmux-install install /absolute/extracted/archive \
  --prefix /usr/local --version "$VERSION-$TARGET"
```

Rust validates the inventory schema, required artifacts, lengths and hashes before
publication. Both checksum and archive are obtained over the configured HTTPS
publisher channel; checksums are not an independent signature/trust root.

The packaged installer publishes all validated siblings into an immutable
`$BMUX_INSTALL_PREFIX/lib/bmux/versions/<version>-<target>` directory (default
prefix `/usr/local`), syncs it, then atomically replaces `bin/bmux` with a symlink
to that version's CLI. Running older binaries retain their complete sibling
installation; no CLI/helper pair is overwritten sequentially. Concurrent
publishers fail via an exclusive install lock. Existing immutable versions are
refused rather than changed. Old versions are retained, not automatically deleted.
Legacy archives without an inventory are explicitly refused with a manual-install
message; they are not silently represented as auxiliary-capable installations.

Existing npm launcher fallback behavior still needs separate review: packaging
complete platform files does not authorize falling back to an unrelated PATH
installation.

### Packaging validation

```sh
node --test infra/wrangler/test/install.test.mjs
```

These shell fixtures exercise checksum-before-extraction ordering, archive safety,
and invocation of the owned Rust installer with isolated mocked downloads, without
building Rust, contacting endpoints, or publishing packages. Rust xtask unit tests
cover inventory identity, corruption, slot layout and generated package entries;
the coordinator runs these through the Nix-selected toolchain along with mandated
workspace checks. Full release builds, archive/deb/rpm inspection and installed-
runtime validation remain coordinator/CI responsibilities.

## Channel manifests

Each release publishes:

- `stable/latest.json`
- `nightly/latest.json`

These are consumed by the Worker to resolve install defaults and by
`/channels.json` for clients.
