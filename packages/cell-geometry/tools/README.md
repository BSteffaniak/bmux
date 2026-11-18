# Width engine compatibility data

`width_data.rs` is checked in. Building, testing, and packaging BMUX never run
the generator or need the reference crate. The generator is a
migration/verification tool for the Unicode 17 narrow-width policy from
unicode-width 0.2.2 (upstream commit
`9d98411769fe13c7c18cab0b3fbbab29ba8350ea`). BMUX uses neither the CJK
ambiguous-wide API nor the Rust trait extension surface.

To reproduce or audit the data, copy this directory's `generate.rs` into a
temporary directory and place the pinned crate's `src/tables.rs` beside it as
`reference.rs` (SHA-256
`4c3e673da24634c269562ca4d3652b494542c78cafe56da69d23df9178adffd4`). Run `rustc
--edition 2024 -O generate.rs -o generate` there, then `./generate >
candidate.rs`. Compare the candidate byte for byte with `src/width_data.rs`. Use
a temporary file: generation must succeed before replacing data.

The generator discovers reachable right-to-left suffix states, partitions
scalars by their signed width adjustment and next state in *all* those states,
and deduplicates pages of scalar classes. It verifies the resulting two-level
lookup for every Unicode scalar in every reachable state against the reference.
Equality of the start state and every transition implies the same raw width for
every scalar sequence, not just the emoji fixtures. Raw width is not always cell
occupancy: Khmer U+17D8 has raw width three; BMUX caps a single cell leader at
two. The BMUX control, segmentation and clipping policy is applied separately,
as before.

`src/compatibility.rs` adapts all 34 narrow-width behavioral tests from the
pinned crate, including exhaustive scalar consistency and variation-selector
loops. `tests/emoji-test.txt` is its complete Unicode 17 emoji fixture.
Reference-derived material and tests retain upstream licensing notices in
`licenses/`; the emoji fixture retains its Unicode notice. When updating the
baseline, review Unicode version, licenses, behavior and fixture changes
together rather than blindly refreshing expected widths.
