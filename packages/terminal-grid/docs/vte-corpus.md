# Neutral terminal parser corpus

## Provenance and scope

The regression source is vte 0.15.0, upstream commit
`3b3da71c34cc1256c7e20981cf03f8eb95e08ffc`. Copied/adapted tests retain
Joe Wilm's 2016 copyright and MIT OR Apache-2.0 attribution. Both complete
license texts accompany the upstream `demo.vte` fixture in `tests/fixtures/vte`.
Production parser logic is independent; no ANSI Processor algorithms are copied
into the neutral parser.

Compatibility targets the upstream no-default-features configuration:
32 total numeric parameter/subparameter slots, two intermediate bytes,
16 OSC arguments, and 1024 OSC content bytes. Numeric values saturate at u16.
The tests preserve grouped parameters, callback ordering, ignore flags, C0/C1
handling, UTF-8 recovery, OSC termination flags, and DCS hook/put/unhook ordering.
They neither normalize callbacks nor reinterpret parsed commands.

## Upstream parser regressions: 34 of 34

`vt_parser_tests::upstream` retains every assertion applicable to fixed-buffer
configuration. Imports and private capacity constants are adapted locally.
The upstream `std` dynamic-buffer assertion is excluded deliberately; its
fixed-buffer assertion is retained unconditionally. The three custom-size
cases remain active.

- `parse_osc`
- `parse_empty_osc`
- `parse_osc_max_params`
- `osc_bell_terminated`
- `osc_c0_st_terminated`
- `parse_osc_with_utf8_arguments`
- `osc_containing_string_terminator`
- `exceed_max_buffer_size`
- `parse_csi_max_params`
- `parse_csi_params_ignore_long_params`
- `parse_csi_params_trailing_semicolon`
- `parse_csi_params_leading_semicolon`
- `parse_long_csi_param`
- `csi_reset`
- `csi_subparameters`
- `parse_dcs_max_params`
- `dcs_reset`
- `parse_dcs`
- `intermediate_reset_on_dcs_exit`
- `esc_reset`
- `esc_reset_intermediates`
- `params_buffer_filled_with_subparam`
- `build_with_fixed_size`
- `exceed_fixed_osc_buffer_size`
- `fixed_size_osc_containing_string_terminator`
- `unicode`
- `invalid_utf8`
- `partial_utf8`
- `partial_utf8_separating_utf8`
- `partial_invalid_utf8`
- `partial_invalid_utf8_split`
- `partial_utf8_into_esc`
- `c1s`
- `execute_anywhere`

## ANSI Processor accounting: 23 of 23

The ANSI Processor is a semantic layer, not the replaced byte parser. The ledger
below distinguishes retained syntax from algorithms that are intentionally not
implemented here. Syntax tests do not claim semantic equivalence for unsupported
algorithms. Implementing any new product behavior requires a separate decision.

| Upstream test | Retained evidence / semantic boundary |
| --- | --- |
| `parse_control_attribute` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_terminal_identity_csi` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_terminal_identity_esc` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_truecolor_attr` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_zsh_startup` | Original complete byte array copied with attribution; chunk invariance and temporary exact callback comparison. Upstream case had no semantic assertions. |
| `parse_designate_g0_as_line_drawing` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_designate_g1_as_line_drawing_and_invoke` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_valid_rgb_colors` | All original color strings retained as OSC 4 payloads with exact byte preservation. RGB scaling/legacy conversion and semantic rejection are outside parser scope. |
| `parse_valid_legacy_rgb_colors` | All original color strings retained as OSC 4 payloads with exact byte preservation. RGB scaling/legacy conversion and semantic rejection are outside parser scope. |
| `parse_invalid_rgb_colors` | All original color strings retained as OSC 4 payloads with exact byte preservation. RGB scaling/legacy conversion and semantic rejection are outside parser scope. |
| `parse_invalid_legacy_rgb_colors` | All original color strings retained as OSC 4 payloads with exact byte preservation. RGB scaling/legacy conversion and semantic rejection are outside parser scope. |
| `parse_invalid_number` | Original `1abc`, `123`, `321` payloads retained in OSC 104 callbacks. ANSI u8 numeric acceptance/rejection is not CSI u16 parsing; algorithm outside parser scope. |
| `parse_valid_number` | Original `1abc`, `123`, `321` payloads retained in OSC 104 callbacks. ANSI u8 numeric acceptance/rejection is not CSI u16 parsing; algorithm outside parser scope. |
| `parse_number_too_large` | Original `1abc`, `123`, `321` payloads retained in OSC 104 callbacks. ANSI u8 numeric acceptance/rejection is not CSI u16 parsing; algorithm outside parser scope. |
| `parse_osc4_set_color` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_osc104_reset_color` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_osc104_reset_all_colors` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `parse_osc104_reset_all_colors_no_semicolon` | Original input retained in `ANSI_INPUTS`, permanent exact callback assertions and chunk partitions. Semantic attribute/identity/charset/color side effects remain consumer-owned. |
| `partial_sync_updates` | Original concatenated input retained, including every split/bytewise boundaries; CSI private marker, SGR and ordering preserved. Synchronization timeout, buffering and flush policy outside parser scope. |
| `sync_bursts_buffer` | 65,536-byte text burst preserves ordered callbacks across chunks. Processor buffer limit, timeout, forced flush and restart policy are outside parser scope; no synchronization buffer introduced. |
| `mixed_sync_escape` | Original concatenated input retained, including every split/bytewise boundaries; CSI private marker, SGR and ordering preserved. Synchronization timeout, buffering and flush policy outside parser scope. |
| `sync_bsu_with_esu` | Original concatenated input retained, including every split/bytewise boundaries; CSI private marker, SGR and ordering preserved. Synchronization timeout, buffering and flush policy outside parser scope. |
| `contrast` | Outside parser scope: RGB luminance/WCAG contrast (white/black 21, same color 1, magenta/green and other numeric ratios). No byte input; no contrast algorithm added. |

## Permanent independent extensions

- Every two-part split, bytewise input, and intervening empty chunks for state
  prefixes are retained as exact schedule-specific matrix goldens. Selected
  malformed UTF-8, recovery and ANSI traces retain supported invariance checks.
- Exhaustive short nonempty partitions, with empty chunks at each boundary, for
  CSI subparameters, OSC, DCS and valid/invalid UTF-8.
- 33 prefixes covering ground, ESC entry/intermediate/overflow, CSI and DCS
  entry/parameter/intermediate/ignore/passthrough, OSC/string-ignore, and partial
  two-, three-, and four-byte UTF-8. Each prefix receives all 256 possible next
  bytes and a recovery suffix. `matrix.prefix-N.trace` retains bytewise and every
  two-part split including endpoints and empty chunks, without requiring traces
  from different schedules to agree.
- CAN/SUB cancellation, replacement/recovery of malformed UTF-8 (overlong,
  surrogates, out-of-range scalars, bad continuations and embedded controls).
- Numeric saturation, 31/32/33 parameter slots (including subparameters),
  intermediate overflow, 15/16/17 OSC arguments, and 1023/1024/1025 content
  sizes; custom-size OSC coverage also comes from upstream tests.
- Four reproducible nonzero xorshift64 seeds, each with 128 arbitrary-byte and
  128 grammar-generated streams. Seven chunk sizes must preserve deterministic same-schedule replay,
  recovery must reach the suffix, and callbacks must respect capacities. Exact
  semantics are retained in `generated.trace` for all 1024 generated inputs at
  seven chunk sizes plus whole-buffer input; universal chunk invariance is not
  claimed for arbitrary bytes.
- Full fixture exact ordered callbacks at nine retained schedules: whole and
  chunks 1, 2, 3, 7, 16, 31, 127, 1024. Schedule-specific golden traces are
  independent baselines, not recomputed by an oracle in ordinary tests.
- Decoded C1 U+0080 through U+009F explicitly execute in whole UTF-8 input but
  print when completed across calls, as pinned upstream does. Seven saved small
  C1/malformed byte cases have exact schedule baselines at chunks 1/2/3/whole.

## Fixture identity and baseline

`demo.vte`: 13809 bytes; SHA-256
`1ac4e7fe28999e2aa632cd80f85d4f5dae776499164ffe0fa792151614ef80fe`.

The fixture includes upstream boundary cases, terminal sequences, and arbitrary
binary/invalid UTF-8. `demo.chunk-N.trace` records one Rust Debug-formatted callback per
line, with no normalization, for each retained schedule. Permanent tests require
these files and compare the entire ordered output independently. Generated and
saved malformed baselines also retain input/seed/case/chunk headers.

Coordinator-reported execution evidence:

- All 34 upstream parser cases: PASS.
- All six exact differential groups: PASS.
- Stream integration: 14 new plus 81 existing tests PASS, including resource
  prefix bound <=208 bytes.
- Release timing, 1000 fixture parses with identical callback recording:
  independent parser 94.53 ms, upstream 94.80 ms. This is an observation,
  not a performance threshold or a claim about all workloads.
- Golden export succeeded for fixture, generated and saved malformed cases.
  After schedule-specific matrix capture, the full oracle-enabled run reported
  260 PASS, zero FAIL, and one timing test ignored in ordinary execution; the
  timing case was separately executed in release mode.

The failing matrix input was
`c2 80 18 1b 5b 30 6d 4f 4b 1b 5d 32 3b 78 07`:
bytewise completion prints U+0080, while whole-buffer ground dispatch executes
128. Both are pinned upstream behavior. The matrix now compares against exact
schedule-specific baselines for every original case/partition, rather than
asserting unsupported cross-schedule invariance. Matrix baseline export and the subsequent full run both passed.

All baselines were captured from pinned upstream with the opt-in exporter
before its removal. Matrix files are partitioned by prefix to bound each
permanent test comparison. Ordinary tests never update their expectations.

## Retained differential evidence

The removed temporary differential module targeted exactly vte 0.15.0 with default
features disabled. It compared all eight callback kinds, grouped u16 parameters,
intermediate bytes, ignore flags, OSC byte arguments, and bell flags without
normalization. Fixture, state-prefix matrix, boundaries and ANSI input traces
are included. Generated comparison uses four seeds, 512 streams per seed per
arbitrary/grammar mode, and nine schedules (whole and eight fixed chunk sizes). Failure messages retain
seed, case, chunk, input length/fingerprint and bounded input previews, first
differing event and nearby event windows. Full traces are compared exactly.
An independent upstream characterization test records the first generated
chunk-sensitive input and totals without asserting unsupported invariance.

The only ignored case is a timing-only fixture comparison: 1000 parses of each
implementation with identical callback recording; run explicitly in release mode
on an idle machine. It is not a flaky wall-clock assertion or a correctness gate.

After coordinator confirmation of exact comparisons, all retained goldens and
release timing, the temporary differential source was removed. The coordinator
owns removal of its module declaration, feature, dependency alias and lockfile
entries. Independent tests, complete ordered goldens, fixture and licenses remain;
no runtime external parser is required by the permanent corpus.

## Stream replay contract

The integration corpus validates bounded replay prefixes of at most 208 bytes.
It records the supported replay state rather than retaining an unbounded byte
history. Legacy restore with an empty prefix is a no-op for parser state; no new
wire or schema representation is introduced. All 14 added stream compatibility
tests and 81 existing parser/integration tests passed according to the coordinator.
Parser state remains scoped to each independent execution.

This corpus reports exercised state-prefix/next-byte/schedule matrix evidence,
not a numeric branch-coverage percentage; no branch-coverage instrumentation was
run. The copied regression cases and deterministic extensions establish greater
input/schedule coverage than the upstream test set without claiming exhaustive
coverage of every possible input stream.

## Exact compact golden format

The captured goldens are retained losslessly in paired `.dict` and `.idx` files.
Logical `.trace` names in tests select those pairs. The standard-library decoder
reconstructs complete original UTF-8 strings before exact comparison; no hashes
replace expected callbacks and no external decompressor or dependency is used.

Dictionary format (`VTDICT2`, UTF-8 text): the first line is `VTDICT2`. Each
following line is decimal prefix-byte count, TAB, decimal suffix-byte count, TAB,
literal middle text, LF. Reconstruct each dictionary entry from the previous
entry's prefix, the middle, and the previous entry's suffix. Prefix/suffix ranges
must not overlap or split UTF-8 scalars. Append LF when emitting an entry. The
first previous entry is empty. This affix coding preserves inspectable callback
and input-header text while avoiding repeated long seed/input headers.

Index format (`VTIDX2`): eight-byte magic `VTIDX2` followed by two NUL bytes,
then little-endian u64 decoded line count and u64 decoded byte length. Commands
follow: tag 0 plus one unsigned base-128 varint dictionary index; tag 1 plus
varint backward distance and varint line count. Indexes are zero-based.
Backreferences copy previous indexes sequentially, permitting overlap. Varints
carry seven low data bits per byte, high continuation bit, at most five bytes,
and fit u32. The decoder checks headers, truncation, varint overflow, dictionary
bounds, backreference distances, output line/byte lengths and allocation limits.

Conversion operated only on existing pinned-upstream captured files, not parser
output. All 44 originals passed on-disk byte-exact reconstruction before their
old representations were removed. Subsequent affix dictionary and varint
conversions separately checked exact entry/command conservation. Final Python
decode checks recovered 44 traces totaling 76,083,950 bytes. Fixture directory
storage fell from 76,108,534 bytes to 5,453,076 bytes (including licenses and
`demo.vte`). All expected headers, callback order, schedules and byte payloads
remain available. Rust decoder execution is delegated to coordinator validation.
