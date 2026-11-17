# Native packaging acceptance

Run from this BMUX worktree. Do not run Cargo/Nix until the coordinator reserves
one validation slot. Use a fresh absolute output directory; never install these
artifacts or select them in an active runtime.

Source-only tests (no compiler, no native library load):

```sh
python3 -B scripts/test-package-plugin.py
```

Reserved-slot build and identity regression, including real Bcode
file-references:

```sh
OUT=$(mktemp -d /tmp/bmux-x1-acceptance.XXXXXX)
export OUT
nix develop --command python3 -B scripts/package-plugin-acceptance.py \
  --bcode-root /Volumes/ehdd/GitHub/wt-bcode/shared-plugin-architecture \
  --output "$OUT" --build --run-acceptance
export BMUX_ACCEPTANCE_NATIVE_ARTIFACT="$(python3 -c \
  'import json,os; print(json.load(open(os.environ["OUT"]+"/acceptance-artifacts.json"))["environment"]["BMUX_ACCEPTANCE_NATIVE_ARTIFACT"])')"
nix develop --command cargo test -p bmux_plugin --test shared_plugin_scoped_loader
nix develop --command cargo test -p bmux_plugin --test shared_plugin_scoped_loader -- --ignored
```

The first loader command covers static definitions; the second explicitly runs
all native fixture cases. Missing artifacts are errors, not successful skips.
The native fixture is one generated package holding the exact checked-in
scoped-loader entry and definition files, built directly with
`cargo rustc --crate-type cdylib` (no wrapper crate); it only exports that
factory. BMUX hello packages itself the same way (`plugin!`).
Cargo JSON selects library paths, never an assumed implementation dylib name.

The identity driver checks BMUX hello and the real Bcode file-references wrapper
against independent expected IDs. It accepts explicitly negotiated v2/bits15
and v3/bits31, requiring the v3 declaration callback. The entry symbol remains
`bmux_plugin_entry_v2`; that name alone does not determine descriptor version.
Native inspection executes trusted library initializers; this is not a sandbox.

These metadata checks do NOT establish invocation or mixed-host parity for
file-references. Mixed static/dynamic co-residence, Bcode-side native activation
and real file-reference invocation remain required integration coverage, owned
with the loader/host test lanes. Do not count a missing test or an ignored test
as passing. No kernel-port fallback is authorized by these commands.

After focused tests, the reserved validation slot must also run repository
AGENTS.md requirements: clippy before full nextest, cargo machete, and bundled
plugin rebuild (or explicit plugin builds if the rebuild command is
unavailable).
No compiler validation is implied by generation-only tests.
