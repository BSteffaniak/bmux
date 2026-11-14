# Shared plugin integration checkpoint

Status: incomplete; not production-ready. This progress record does not relax
invariants or replace required validation. Work is uncommitted in the designated
BMUX and Bcode shared-plugin-architecture worktrees. Initial source-only
implementation has ended. The user has directed continued implementation,
integration, and verification through the remaining solvable gaps; the earlier
worker freeze is superseded for explicitly resumed file ownership. A handoff or
compilation checkpoint is not completion.

## Latest follow-up measurements (2026-10-29, revision 59)

- BMUX strict Clippy passed again at run 59. BMUX CLI library now passes all
  772 tests with one ignored (CLI-tests-13); the exact `cargo test -p bmux_cli`
  command then failed six application-resurrection integration tests because
  selected lifecycle transport was lost and native-entry discovery was attempted.
  Production integration is repairing that path, not adding legacy fallback.
- All three previously failing real-server capture tests now pass individually:
  simultaneous attach, captured history, and scrolled image (attach/history/image
  run 2). Full nextest run 8 nevertheless reports startup failures before capture;
  required sandbox capacity reservation and startup behavior remain under diagnosis.
- Full recording process integration now PASSES: 27 passed, four ignored, zero
  failed (`/tmp/bmux-shared-plugin-recording-tests-20261029-16.log`, 686 seconds).
  The durable start/send retry test also passed twice independently (runs 14/15).
  Host-owned bounded execution now separates nested async context from blocking
  callback execution; no worker-marker or authorization guard was relaxed.
- Theme library now PASSES all 69 tests (theme-tests-4), including both formerly
  hanging picker cases. Fixtures use checked envelopes and the actual selected
  theme caller; foreign checkpoint callers remain rejected. Kernel bridge library
  regressions pass all nine (kernel-tests-4). Machete-7 and workspace-deps-7 pass.
- Bcode real TUI frontend/backend wrapper acceptance now PASSES: 47 passed, one
  ignored (`/tmp/bcode-shared-plugin-tui-tests-20261029-20.log`). This includes
  selected native frontend and discovered dynamic visual adapter, without legacy
  exports. Standalone portable SDK: nine passed (portable-sdk-tests-9); typed host
  codec: three passed (host-codec-tests-1). Author callbacks enter a dylib-local
  owned runtime, with bounded task tracking and settlement-aware shutdown.
- Bcode TUI now consumes neutral SDK/runtime as production dependencies for actual
  scoped host dispatch and activation resources. Product host wire dispatch is
  implemented and focused owner/bounds/closure tests pass; complete static/native
  coverage of every host operation remains pending. Latest Clippy run 46 fails a
  newly added lifecycle-context fixture field, not a final strict pass.
- Actual BMUX full nextest run 8 is still active and already has failures,
  including selected playbook executor routing and lifecycle transport. Run 6
  ended with 7,181 passed, 110 failed (including two interrupted hangs), 30 skipped.
  File named BMUX nextest run 7 was accidentally launched from Bcode and explicitly
  interrupted; it is NOT BMUX validation evidence. Bcode full nextest run 3 failed
  compilation during in-flight frontend changes and still requires a final rerun.
- PTY smoke run 6 FAILED: sh startup passed, bash startup timed out after all three
  attempts. Strict Clippy/check successes and focused passes do not waive this.
  Final compatibility, plugin rebuild, complete suites, release/performance and
  cross-platform acceptance remain open. No production-ready claim is made.

## Latest follow-up measurements (2026-10-29, revision 52)

These are snapshot-specific measurements, not final acceptance:

- BMUX strict Clippy passed at run 52; Bcode workspace/all-target Clippy passed
  at run 35. BMUX SDK library: 184 passed (sdk-tests-4). BMUX host library:
  291 passed, one ignored (host-tests-11). Bcode host library: 168 passed, one
  ignored (host-tests-7). BMUX architecture guardrails: all 76 passed
  (architecture-tests-2). BMUX machete and workspace dependencies passed again,
  covering 130 manifests (machete-6).
- Actual ABI-v3 native wrappers rebuilt successfully (native-packaging-6).
  Bcode mixed embedded acceptance now passes all nine tests, including committed
  filesystem cancellation (`/tmp/bcode-shared-native-embedded-20261029-6.log`).
  Artifact SHA256 values were recorded with that run. Cancellation now preserves
  the typed pre-effect distinction; post-handler errors remain unknown outcomes.
  Temporary numeric diagnostics were subsequently removed; final rebuilt
  acceptance must verify the post-removal snapshot too.
- Standalone Bcode SDK portable tests now pass all eight (portable-sdk-tests-8).
  Its dependency explicitly enables neutral TUI serde rather than accidentally
  relying on workspace feature unification. Bcode TUI depends on neutral plugin
  runtime in production for activation-owned resources, not just dev fixtures.
- Real Bcode frontend wrapper was separately built with `--component frontend
  --feature tui` from the Bcode hello implementation. The acceptance packager's
  existing `hello` wrapper is the BMUX example and is NOT interchangeable.
  Bcode TUI run 14: 43 passed, two failed, one ignored. The actual native path
  exposed dylib-local Tokio TLS absent during author `Open`; author-local runtime
  entry/task settlement is being repaired. The discovery fixture also still
  required migration away from legacy exports. Product frontend host-service
  forwarding remains under implementation, not proven.
- Recording now consumes the explicit async port; the trusted host retains scope
  and authenticated context through blocking settlement. This progressed past
  the original worker failure, revealing a nested permissions/provenance worker
  boundary, subsequently repaired. Fresh CLI build 21 passed. Recording run 12
  still fails with native launch outcome uncertain; evidence is retained under
  `/private/tmp/bmux-rp-6c0d72fb19fd4c2d918095b6b3c8fd1a`, with no implicit retry
  or cleanup. CLI check run 6 passed. The three capture/input failures remain open.
- BMUX full nextest run 6 reached 7,289/7,291 with 108 failure records, then hung
  in two theme picker tests beyond 780 seconds; no completed-suite pass exists.
  Bcode full nextest run 3 is compiling/running and must be observed separately.
  Both logs use `/tmp/<product>-shared-plugin-nextest-20261029-<run>.log`.
- PTY smoke run 5 timed out without a completed result. Bcode machete remains
  unavailable in its Nix shell. Its repository has no BMUX-named
  `scripts/check-workspace-dependencies.sh`; that attempted command is not a pass.
  Existing compatibility matrix run 7 passed all 16 cases before later repairs;
  final source and binaries still require the prescribed complete verification.

## Latest follow-up measurements (2026-10-29, revision 47)

The following supersede revision-32 measurements only where explicitly noted:

- BMUX strict Clippy passed again at run 47. Host library: 288 passed, one
  ignored (host-tests-10); SDK: 179 passed at sdk-host-tests-3. Retained portable
  host tests: 13 passed. Neutral portable scene data/conversion now lives in
  `bmux_tui::presentation::portable`; host activation resources and compositor
  adapters remain in `bmux_plugin`. No SDK dependency on host implementation was
  added to make frontend codecs compile.
- Latest freshly built BMUX CLI library suite: 764 passed, three failed, one
  ignored (`/tmp/bmux-shared-plugin-cli-tests-20261029-12.log`). Simultaneous
  attach renders the shell but misses injected output; history/image captures
  time out. Actual full terminal compatibility matrix now passes all 16 cases
  (`/tmp/bmux-shared-plugin-compat-20261029-7.log`), using the explicit Nix target
  binary with its SHA256 recorded in that log and isolated retained test state.
- Recording activation now binds actual selected configuration/host/process
  caller; lifecycle rejection acceptance passes. Full recording process suite:
  four passed, 23 failed, four ignored (recording-tests-6). The common failure is
  synchronous authenticated dispatch from the async process driver without the
  trusted host blocking-worker boundary. Owned async SDK callbacks and the
  trusted product executor are being integrated; no authority guard was relaxed.
- BMUX all 29 plugin implementation/renderer/client crates built directly at
  rebuild-2; later edits require a final rebuild. Machete and the workspace
  dependency check passed again (130 manifests, machete-5).
- Bcode host library: 167 passed, one ignored (host-tests-5). Native embedded
  acceptance remains eight passed, one failed (embedded-5). Actual bounded
  diagnostics isolated host-begin `FAILED=9` on predispatch cancellation becoming
  sticky and discarding a later authoritative report. Only negotiated v3
  `CANCELLED=10`, with no child admission, may avoid poisoning terminal drain;
  v2 semantics and genuine transport uncertainty remain unchanged. Typed
  cancellation propagation is still being repaired. Temporary numeric-only
  diagnostics must be removed after the root repair is validated.
- Bcode frontend author codecs now use explicit product wire adapters and the
  neutral retained scene contract. Production startup/real dynamic frontend
  acceptance is still under integration, not proven complete. Bcode strict
  Clippy and all acceptance must be rerun after this source snapshot settles.
- Some long-lived worker contexts exhausted provider context limits and were
  replaced with fresh ordinary child sessions. That replacement is orchestration,
  not completion evidence. No full-suite or production-ready pass is claimed.

## Latest measured integration checkpoint (2026-10-29, revision 32)

Measurements below supersede older failure diagnoses in this document, not the
incomplete status. Source continues to change; targeted passes are
snapshot-local.

- BMUX strict `cargo clippy --all-targets -- -D warnings` passed in
  `/tmp/bmux-shared-plugin-clippy-20261029-32.log`.
- Selected host builtins now share storage/logging/settings/CLI mechanics with
  the legacy adapter, bound to actual selected owner/configuration and invocation
  admission. Storage preserves `plugin-storage/<owner>/<key>.bin`; checked
  contracts are validated before effects. Permissions delegates checked dispatch
  to the shared adapter rather than decoding checked envelopes as legacy bodies.
- Host and permissions library tests passed together; host: 286 passed, one
  ignored (`/tmp/bmux-shared-plugin-host-permissions-tests-20261029-1.log`).
- Fresh CLI/supervisor builds passed. `cargo test -p bmux_cli` still failed:
  759 passed, three failed, one ignored. Real retarget now passes; simultaneous
  attach exits before rendering, and two capture tests time out. Evidence:
  `/tmp/bmux-shared-plugin-cli-tests-20261029-10.log`.
- BMUX full nextest run 4 settled with exit 100: 7,151 passed, 110 failed,
  30 skipped out of 7,261 run (713 seconds). Evidence:
  `/tmp/bmux-shared-plugin-nextest-20261029-4.log`. No owned nextest remains.
  Run 3 had timed out after 4,488/7,261 and is superseded by run 4.
- BMUX cargo-machete and workspace dependency check passed (130 manifests).
- Bcode strict workspace/all-target Clippy passed at run 10; later blocking
  executor changes make that an intermediate pass, not final acceptance.
  Bcode TUI focused tests passed: 37 passed, one ignored.
- Actual ABI-v3 thin wrappers built and loader acceptance passed for scoped
  loader, hello, file-references, filesystem and shell. Artifacts and actual
  Cargo-emitted paths are recorded in
  `/Volumes/ehdd/bcode-shared-native-acceptance.BonWWT/acceptance-artifacts.json`.
  Packaging acceptance evidence: `/tmp/bcode-shared-native-acceptance-20261029-2.log`.
- Actual Bcode embedded/static/dynamic acceptance failed: four passed, five
  failed. Synchronous callback runtime reentry panicked and poisoned callback
  authority. B2/B4 repairs are in progress; joined per-poll threads are not the
  final bounded async executor. Evidence:
  `/tmp/bcode-shared-native-embedded-20261029-1.log`.
- Complete dynamic frontend startup, portable surface/action/update codecs,
  release performance, final full suites, PTY/compatibility, physical unload,
  Linux and Windows acceptance remain open. No production-ready claim.

## Integrated mechanisms

Shared definition/factory and scoped runtime ownership, static/native loaders,
ABI-v2 packaging, selected candidate validation, typed registration, retained
frontend mechanisms, and Bcode product extensions are implemented at source
level. Static compatibility adapters retain existing authors while packaging
exports are separated. This is not proof of complete migration or static/dynamic
parity.

Integration repairs preserve explicit identity and contract validation,
distinguish unavailable generated evidence from malformed evidence, validate
checked requests before legacy body dispatch, preserve checked response
acceptance, and inject snapshot runtime from the actual server registry through
a host-local validated bundled factory. Loader owners retain one executable
snapshot/digest per startup rather than rehashing the executable for every
bundled plugin. Dynamic selection never substitutes the bundled injected
factory.

## Measured evidence

All Rust commands used the respective Nix-selected compiler and target
directory. IPC tests used `TMPDIR=/private/tmp` because the Nix temporary root
beneath `/Volumes/ehdd` has an unsafe writable ancestor; production socket
policy was not weakened.

Evidence logs are in `/tmp/bmux-shared-plugin-swarm-20261028/` on the
implementation machine (temporary, not portable acceptance artifacts).

- BMUX `cargo clippy --all-targets -- -D warnings`: passed repeatedly, including `bmux-workspace-clippy-32.log`; later in-flight test-fixture edits produced failures in `-34.log`, so the final checkpoint still requires a settled rerun.
- Bcode same strict workspace Clippy: passed, latest `bcode-workspace-clippy-17.log`.
- BMUX runtime/SDK/schema/macros `cargo test`: passed 287 tests including compile suite (`bmux-foundation-tests-4.log`), with nine ignored doctests.
- BMUX host library tests: passed after v1 single-invocation fixture correction (`bmux-host-tests-4.log`).
- Bcode plugin/SDK/bundled library tests: passed 284 tests, one ignored (`bcode-plugin-foundation-tests.log`). This includes real question native pending/concurrent-service tests through the shared loader.
- Bcode CLI timeout/cancellation fixtures: passed after private IPC fixture correction and explicit Tokio runtime feature (`bcode-timeout-tests-3.log`).
- Bcode auth-broker library tests: passed (`bcode-auth-broker-tests-2.log`).
- BMUX snapshot library tests: passed (`bmux-snapshot-tests-2.log`).
- BMUX architecture guardrails: all 76 passed (`bmux-architecture-2.log`). Retest after further source changes remains required.
- Bcode `cargo check -p bcode_plugin -p bcode_server -p bcode_tui --all-targets`: passed (`bcode-integration-check-9.log`).
- BMUX `cargo check -p bmux_cli`: passed at earlier checkpoints; a later intermediate snapshot injection edit failed and was subsequently completed. Rerun final settled source.
- BMUX workspace-dependency guard and `cargo machete --with-metadata`: passed (`bmux-workspace-dependencies-3.log`, `bmux-machete-4.log`). Bcode Nix shell lacks cargo-machete; its attempted command failed as unavailable, not an unused-dependency pass.
- Actual macOS aarch64 hello and scoped-loader fixture dylibs built. Metadata harness validated identity, ABI v2 and capability bits 15. Real scoped loader suite passed all seven tests including explicitly enabled artifact tests (`scoped-loader-tests-4.log`). One omitted-artifact invocation failed; the correctly configured rerun passed.

Retained test artifacts under `/tmp/bmux-x1-artifacts.GANWTY/retained/`:

- `libbmux_hello_plugin_dynamic_backend.dylib`: SHA-256 `859186d78459e00d0d59112deacb1be6867dc0a1661e982d3babb7fd34f21ccd`.
- `libbmux_acceptance_scoped_loader_native.dylib`: SHA-256 `6ea5e51750d91b9366aa7201485c117b8f6eb9be1f8926d4043a655135c0b814`.

These artifacts predate later source repairs and are not final release
artifacts. They establish neither physical unload nor cross-platform parity.

## Blocking validation and production gaps

Latest BMUX CLI library run (`bmux-cli-tests-16.log`): 746 passed, four failed,
one ignored. All four failures are isolated sandbox startup tests. All plugin
activations now succeed, but stream admission fails closed. The exact retained
startup diagnostic is `host-internal provider invocation required`.

Source diagnosis: scoped foundational providers have `PluginHostServices`, but
`TypedServiceCaller::execute_kernel_request` rejects kernel requests when that
service-only port is selected. The clients provenance facade needs
`CurrentStreamProvenance`; the server retains authenticated `HostInternal`
context, but there is no invocation-bound foundational kernel port carrying it
through this path. Required next work is a generic, explicitly authorized kernel
capability preserving the actual retained invocation context, restricted to the
foundational boundary. Caller strings, absence of a client, global bridges,
unchecked fallback and permissive relabeling must not substitute for that
capability.

Other open gaps:

- Full dynamic checked-service negotiation and typed registry export remain unsupported rather than silently accepted.
- Actual attach prompt/action factory injection and complete rich dynamic frontend proxies remain incomplete. Cells-only fallback is not equivalent retained-scene support.
- Bcode native authority/lifecycle recovery, invocation-specific host binding and full production cutover remain incomplete; legacy paths are still present.
- Foundational test fixtures using old global routers are being migrated to explicit bound ports. Later strict Clippy found missing fixture identifiers and used-underscore bindings; these remain pending until repaired and rerun.
- Full BMUX nextest did not finish: latest substantial run reached 3,089/7,165 before tool timeout and recorded 115 failure records, including startup/integration failures. No full pass claimed.
- Full Bcode nextest did not finish: substantial run reached 4,670/10,048 before timeout and recorded 99 failure records. Several targeted failures were subsequently fixed; no full pass claimed. Some artifact conformance tests still expect old implementation dylibs/legacy loader paths and require genuine new-wrapper migration, not ignored tests or restored implementation exports.
- PTY smoke timed out; compatibility matrix failed. Both require rerun after real startup is restored.
- Final bundled plugin rebuild/direct all-plugin build, release performance, actual Bcode file-references artifact regression, physical unload evidence and Linux/Windows validation remain outstanding.

No user-runtime installation, daemon replacement, commits or pushes are
acceptance evidence here. Do not report the refactor complete from compilation,
worker handoffs or these targeted successes.
