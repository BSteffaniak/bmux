# Hello plugin: one implementation, two packages

`src/lib.rs` is an export-free rlib. `plugin_definition()` is inert metadata
plus
a fresh scoped factory; bundled registration calls that same function. No
`static-bundled` feature may change plugin behavior or emit dynamic exports.

Dynamic packaging uses the shared generator from the repository root:

```sh
python3 scripts/package-plugin.py \
  --implementation-manifest examples/hello-plugin/Cargo.toml \
  --sdk-manifest packages/plugin-sdk/Cargo.toml \
  --output target/plugin-packaging/hello/backend \
  --component backend
```

Without `--build` this only generates a thin standalone Cargo target. Add
`--build` to compile it; add `--install-root <directory> --declaration
examples/hello-plugin/plugin.toml` to publish an immutable artifact. Run build
commands in the repository's Nix development environment. Python 3.11+ is
needed.
The generator follows workspace Cargo patches and Cargo's reported library path,
including cross-target `.dll`, `.so`, or `.dylib` names, rather than guessing a
host filename. Durable installation currently supports POSIX hosts only; Windows
hosts fail explicitly before publication (cross-building a Windows artifact is
still supported). Repeat `--feature` to select implementation features
explicitly.

The generated wrapper calls `export_plugin_entrypoint!(packaged_definition)`;
it contains no command implementation. Only the requested component role is
advertised. Install directories separate logical ID, component, library hash
and declaration hash. `artifact.json` names the observed library/declaration;
it is not proof of compatibility, a credential, or permission to activate.
The declaration is retained verbatim rather than silently rewritten to resemble
a legacy ABI. Installation does not overwrite older artifacts or active runtime
selection. A loader must explicitly support this shared v2 descriptor and
layout.

## Later integration: real acceptance libraries

Do not run during initial source authoring. After explicit validation
authorization,
the driver generates concrete packaging under the chosen output directory:

```sh
python3 scripts/package-plugin-acceptance.py \
  --bcode-root /path/to/approved/bcode/worktree \
  --output target/plugin-acceptance
```

Add `--build` in the integration phase to build X2's exact
`scoped_loader_native.rs` plus sibling `scoped_loader_definition.rs`, with
shared
SDK and workspace-selected serde_json. No fixture workspace member is checked
in.
The other artifact is the actual Bcode file-references implementation packaged
through its public definition, never a fixture substituting its identity.
`acceptance-artifacts.json` records Cargo-reported paths and the exact
`BMUX_ACCEPTANCE_NATIVE_ARTIFACT` value. With that environment set, run the
ignored
`shared_plugin_scoped_loader` cases explicitly during integration.

`--build --run-acceptance` additionally checks embedded identities independently
against `acceptance.scoped-loader` and `bcode.file-references`, then invokes
X2's
artifact harness with both explicit expected IDs. It never installs or starts a
daemon. X2's harness must first match the current scoped descriptor layout and
capability bits (v2: `15`; v3: `31`, adding invocation-bound kernel requests);
no legacy
zero-capability reinterpretation is permitted.

The byte hash is the installed artifact identity. Publication loads the
explicitly
selected trusted native library for inert descriptor metadata discovery (never
creates an instance), requires scoped streaming/host-service/live-cancellation
capabilities including activation registrations, and verifies logical ID,
version, component, declaration and required
capabilities before copying bytes. Native loading is not a sandbox; do not use
installation to inspect untrusted code. Cross-compiled artifacts must be
installed
on a host that can load that artifact.

`python3 scripts/package-plugin.py --discover-root <directory>` inventories all
installed candidates with hash and metadata consistency checks, without loading
code or choosing a winner. The receipt retains complete observed shared
metadata,
including contracts and required capabilities. Host runtime discovery still
needs
to consume this layout; no legacy runtime declaration is reinterpreted.

`--artifact-id` supplies an optional build label retained in the artifact bytes;
it is not a descriptor credential or a substitute for the byte hash. Defaults to
a fresh UUID. These source-stage changes have not been compiled or executed yet.
