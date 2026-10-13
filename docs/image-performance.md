# Image pipeline performance

## Opt-in pure benchmark

`packages/image/benches/image_pipeline.rs` is a dependency-light `harness = false`
benchmark. It generates fixed opaque eight-color RGBA fixtures and PNG bytes in
memory. It never probes or writes graphics to a terminal. Default fixture:
1280 × 720; default repetitions: 10. Select `--fixture palette` (default),
`solid`, `gradient`, `transparent`, or `detailed`. Transparent data exercises
alpha values 0/127/128/255; detailed data is deterministic high-frequency RGB,
not a photograph. Inputs are bounded to 4096 × 4096 and
1000 repetitions. Run inside the flake-selected shell:

```sh
bash scripts/dev.sh cargo bench -p bmux_image --bench image_pipeline \
  --features kitty,sixel,iterm2 -- --iterations 20
bash scripts/dev.sh cargo bench -p bmux_image --bench image_pipeline \
  --features kitty,sixel,iterm2 -- --iterations 20 --fixture detailed --json
bash scripts/dev.sh cargo test -p bmux_image --features kitty,sixel,iterm2 \
  --test image_work_regression
```

JSON output is JSON Lines, one object per case. Case names include fixture and
dimensions, so results cannot silently mix input profiles. Capture stdout separately from
Cargo diagnostics. Each case reports min/median/p95/max nanoseconds, sample count,
output bytes, protocol command/chunk count, upload chunk count, source decode and
fragment preparation deltas where available, and deterministic count stability.
No duration is a pass/fail threshold. Work-count instability is an error, not a
timing gate. Quantiles use sorted samples and integer indices (median `n/2`, p95
`(n-1)*95/100`); small sample counts do not support tail-latency conclusions.

Encoder cases isolate base64, bounded Kitty chunks, Sixel, and iTerm2 encoding of
pre-generated PNG. `encoder/sixel-reference` runs a frozen pre-optimization
encoder from revision `73b533482f80`, in the same binary/profile on the identical
fixture as `encoder/sixel`. It deliberately retains all 216 color scans per band,
original quantization, allocation, palette order, and RLE behavior. The harness
asserts exact reference/current byte equality outside timing. Compare their
medians to compute a same-build speedup; no historical-machine timing is needed.
The reference is benchmark/test-only, not a selectable production implementation.
PNG fixture construction is outside the encoder timing.
Encoder byte counts exclude terminal framing. Scene byte counts include framing.
Scene workloads cover cold/warm, movement, crop, hide/reveal, resize, replacement,
and deletion for each host protocol. Attach additionally exercises overlay
occlusion, a fresh-ID legacy baseline, and retained versus deliberately
invalidated upload state. The clipped attach path clones a candidate, begins a
frame, renders, finishes, then commits only after successful `Vec` output.
Decode/preparation counts come from `KittyHostState::work_counts`; preparation is
not an independently measured scale count. Unsupported counters are zero (not
proof of zero internal work). TUI scene reconciliation and fixture cloning are
outside its render timing; attach candidate cloning is inside clipped timing.

The attach benchmark calls public image compositor entry points, not the full
CLI attach transport or PTY runtime. The TUI harness calls public scene and image
adapter APIs, not a complete application cell renderer. Resize changes viewport
and destination geometry; it is not a terminal scrollback/reflow simulation.
The plain attach warm/fresh-ID path and clipped retained path are distinct APIs:
compare `clipped-cold`, `retained-warm`, and `invalidated-upload-baseline`.
These three use identical source bytes, protocol, viewport, destination, crop,
size, and covers before any mutation workload. Both warm candidates are cloned
from the same committed cold frame (not each other's results), with decoded and
prepared caches equally populated. Only the invalidated candidate marks host
uploads uncertain, outside timing; repair deletion/reupload is inside timing.
Their decode/preparation deltas must match, including equally incurred eviction
work for oversized fixtures. No 17 × 13 replacement fixture contaminates this
comparison. The comparison still includes candidate cloning and host repair
bookkeeping, so it is an end-to-end upload-invalidation delta, not a pure base64
cost. These are diagnostic workloads, not substitutes for semantic
coverage of scrollback anchors, normal/alternate screens, overlay authority,
reflow, or partial-output repair in their owning lanes.

## Reproduction identity and baseline

Record revision plus uncommitted diff identity, `flake.lock` hash, `rustc -Vv`,
Cargo profile/features, benchmark artifact SHA-256, OS/kernel, CPU model/count,
CPU governor/power mode, memory, and competing load with every measurement.
Record fixture dimensions, iterations, protocol, and exact command. Debug and
release results must never be combined. No Bcode provider/model is involved in
executing the pure harness. For this source-preparation lane the orchestration
selection was profile `sol`, provider `bcode.openai-compatible`, model
`gpt-6.1-sol`; it is provenance, not benchmark machine identity.

Historical user-provided isolated Kitty debug baseline: 1280 × 720 redraw,
approximately 4.96 MB, 1200 upload chunks, 75–84 ms, with fresh IDs causing
reupload. This lane did not execute it or establish machine/profile identity.
The harness provides `attach/kitty/fresh-id-baseline` and
`attach/kitty/invalidated-upload-baseline` to remeasure rather than treating that
observation as a universal target. Raw fixture size is 3,686,400 bytes; padded
base64 payload is 4,915,200 bytes before headers/framing, split into 1200
3072-byte raw chunks. Warm retained output should be assessed independently.

There are **no flaky CI wall-clock performance gates**. Deterministic integration
tests cover base64 lengths/roundtrips, Kitty bounded chunks/continuations,
iTerm2 body roundtrip/byte count, Sixel fixed-palette partial bands, and retained
compositor behavior, selected fixtures against frozen Sixel wire bytes, and
injected partial-output repair without time thresholds. Coordinator owns compilation, full validation,
and benchmark execution in the reserved shared build resource.

## Read-only transport/base64 audit

Source observations from the initial ownership-v1 checkout (names are more
stable than evolving line numbers; other lanes may subsequently implement these):

- `codec/base64.rs::base64_encode` allocates the padded string and pushes four
  characters per three bytes. `base64_decode` allocates and accepts nonalphabet
  bytes permissively. Do not silently make parsing stricter as a perf change.
- `codec/kitty.rs::encode_transmit_chunks` collects chunk slices, allocates one
  base64 string plus a formatted command per chunk, then retains `Vec<Vec<u8>>`.
  At 1280 × 720 RGBA the payload expansion alone is 4/3, independent of content.
- `codec/iterm2.rs::encode_body` and `encode_body_with_cells` allocate a base64
  string and copy it into the returned body. PNG encoding costs are separate.
- `ipc_convert.rs::pane_image_to_ipc` clones raw bytes before passing a borrowed
  slice into `compress_image_payload`; the no-compression/small-payload branch
  copies again with `to_vec`. Compression benefits depend on source protocol;
  Kitty raw traffic is intentionally skipped by the existing policy.

Candidate follow-up changes from that audit, outside this lane; reconcile with
current owner changes before applying:

1. Borrow `img.payload.raw.as_deref().unwrap_or_default()` in `pane_image_to_ipc`
   to eliminate the intermediate raw clone while retaining owned IPC output.
2. Add a checked append-to-buffer base64 helper preserving the standard alphabet
   and padding. Write Kitty/iTerm2 headers and encoded payload directly into one
   reserved command buffer; iterate raw chunks without collecting slice pointers.
3. Consider a streaming Kitty command writer only with explicit candidate-state
   commit/repair semantics. Partial writes must not mark uploads successful.
   Keep the 4096 encoded-byte chunk budget, repeated geometry, `m=1/m=0`, and
   `q=2` suppression. Benchmark allocation changes separately from cache reuse.
4. Cache decoded/prepared content with bounded byte/count eviction and stable
   identity/content/geometry/protocol keys; preserve crop, occlusion, hiding,
   deletion, reset, and failure repair. Do not use unbounded global caches.

No codec, model, registry, transport, or core implementation is changed by this
lane. Suggestions require their owners' correctness review and measurements.

## TUI Sixel cold/warm audit (read-only)

A reported warm sample slower than cold is not yet attributed by measurement.
The current source path is `TuiImageCompositor::render` → `render_candidate` →
`render_pane_images_clipped`. The clipped cache retains encoded non-Kitty output;
`cache.find` success skips `emit_from_pixels` and Sixel encoding, then writes the
cached encoded bytes. Repeated encoding is therefore expected on misses,
eviction, or changed source/crop/size/protocol/cell metrics, not an ordinary hit.
Sixel is stateless on the host: warm still emits its complete encoded image.

Warm work that remains in the adapter:

- `render` clones `self` before rendering. Derived `Clone` deep-copies the
  payload map and registry images; `decoded` entries also retain owned source
  `TuiPayload` byte vectors alongside shared decoded results. Cold state has
  fewer owned buffers than warm state.
- `decode_cached` on a hit clones `(*entry.2)` (`ImagePayload`), including its
  decoded `PixelBuffer::data`, before cropping. Partial crop allocates and copies
  another pixel buffer. Full crop avoids that extra crop buffer, not the clone.
- Payload identity checks and both adapter/compositor cache lookup compare byte
  vectors; hits still scan content. Cache Arc handles avoid copying prepared
  bytes, but do not eliminate these byte comparisons.
- Each render recreates image vectors and output `Vec`, and warm Sixel still
  copies cached encoded bytes into output. Registry replacement drops old owned
  images. Candidate state destruction/allocation is part of the public render
  timing; scene reconciliation is not.

At 1280 × 720 each RGBA clone is 3,686,400 bytes; do not interpret a zero encode
or preparation counter as zero memory traffic. The public TUI adapter does not
expose its decode/preparation counters, so the harness reports unavailable
counts as zero there. A targeted owner follow-up could expose those counters,
measure allocations/copied bytes, and move immutable payload ownership to shared
handles without moving application authority or bypassing candidate commit.
Do not remove failure-safe cloning until an equivalent commit/repair design is
proved. The production adapter now retains shared immutable, uncropped sources;
candidate clones share source buffers and clip changes reuse the decoded source.

## Local informational measurements

On the Nix-selected Rust 1.96.0 toolchain, release profile with
`kitty,sixel,iterm2`, 1280 × 720 fixtures:

| Case | Reference median | Optimized median | Samples |
| --- | ---: | ---: | ---: |
| Palette Sixel encoding | 293.54 ms | 4.11 ms | 5 |
| Detailed Sixel encoding | 353.50 ms | 46.45 ms | 3 |
| Kitty deliberately invalidated upload / retained frame | 17.63 ms | 0.254 ms | 5 |

The frozen Sixel reference and optimized encoder produced identical bytes
(144,262 palette; 4,115,157 detailed). The Kitty comparison used identical
source and geometry: invalidation emitted 4,962,060 bytes / 1,200 upload chunks;
retention emitted 40 bytes / zero uploads. Warm TUI Sixel rendering measured
0.395 ms median after sharing source buffers. These are terminal-free harness
results, not nested-terminal latency or CI thresholds.

Commands: `cargo bench -p bmux_image --bench image_pipeline --features
kitty,sixel,iterm2 -- --iterations 5 --json` (palette) and the same command with
`--iterations 3 --fixture detailed --json`. Measurements ran alongside workspace
validation and other processes, not an isolated machine. Linux 6.12.74,
x86_64, AMD Ryzen 9 7945HX, 32 logical CPUs, 30 GiB RAM, `powersave` governor.
Rust commit `ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96`, LLVM 22.1.2.
Source base: `73b533482f807408d352babc00de43011417ad01` plus this image-pipeline
diff. `flake.lock` SHA-256:
`279d8c78e730c6b30e497f20ce8140ffcf6f8ec8285fd9162e1fd0b5b475052e`.
Benchmark executable SHA-256:
`6c39edfaf06928787f822f4bd0577270d15d1e920acb0af4927a79ffb20e4417`.

## Validation limitation

Focused image, transport, TUI runtime, rich presenter and pane image tests pass,
as do CLI tests, clippy, dependency checks, workspace builds, PTY smoke and the
compatibility matrix. Full nextest is **not green**: completed runs recorded
sandbox-startup failures in unrelated playbook fixtures (5,408/5,409 passed on
the final-source run). A serialized rerun encountered unrelated interactive
recording deadlines under concurrent external test load and was interrupted
following the user's explicit exception. Assertions and deadlines were not
relaxed. Failure logs remain in `/tmp/image-perf-nextest-v4.log` and
`/tmp/image-perf-nextest-scheduled.log`; these local paths are not durable CI
artifacts. Rerun the full suite on a quiescent host before claiming full-suite
acceptance.

## Actual nested-terminal reproduction (separate)

A pure encoder microbenchmark cannot establish visible correctness or nested
terminal transport throughput. For real reproduction, use an explicitly owned
isolated BMUX runtime and a child PTY under a known host terminal; never repoint
an active user's daemon. Record host terminal/version, TERM/profile/cell pixel
size, selected image protocol, BMUX client and daemon artifact hashes and
compatibility, isolated config/state endpoint, terminal size, and recording
identity. Display the same fixture inside the nested pane, record cold and warm
redraws, then movement, clipping/overlay, hide/reveal, resize/reflow,
normal/alternate screen transitions, deletion/reset, and injected output failure.

Capture wire bytes/chunks and decode/preparation counts alongside recording
frames and verify scrollback anchors and committed output. CLI attach integration
and rich presenter repro commands belong to the coordinator; this document does
not invent a command or claim a nested run was performed. Keep nested timings
separate from pure benchmark JSON and run release repetitions without a CI time
threshold. Retain exact commands and machine/profile metadata with results.
