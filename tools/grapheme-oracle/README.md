# Extended grapheme oracle

This isolated Cargo workspace depends only on the old, exactly pinned registry
crate. The BMUX test suite reads frozen data and does not depend on this tool.
From this directory:

```sh
cargo test --locked
cargo run --quiet --locked | cmp - ../../packages/unicode-segmentation/tests/fixtures/oracle-1.13.3.txt
```

The first command runs the candidate's integration assertions against the old
implementation via a two-function adapter. The second reproduces the
deterministic corpus without overwriting the baseline. Its 4,096 rows encode
input scalars and **original UTF-8 byte starts**, not code-point indices.
Baseline SHA-256 (captured before any candidate comparison):
`c9551002e4146a5fe88bae7d4896e5fb22304b04aee2ead2f2899b3401bc4695`.

## Fixture provenance

Files live in `packages/unicode-segmentation/tests/fixtures/`:

- [GraphemeBreakTest.txt](https://www.unicode.org/Public/17.0.0/ucd/auxiliary/GraphemeBreakTest.txt), Unicode 17.0.0, SHA-256 `e2d134d2c52919bace503ebb6a551c1855fe1a1faec18478c78fff254a1793ec`. This is also the source of upstream `TEST_SAME` and `TEST_DIFF`; only extended mode applies here.
- [emoji-test.txt](https://www.unicode.org/Public/17.0.0/emoji/emoji-test.txt), Emoji 17.0, SHA-256 `1d8a944f88d7952f7ef7c5167fef3c67995bcae24543949710231b03a201acda`. All statuses are exercised, including components.
- [Unicode license](https://www.unicode.org/license.txt) in `LICENSE-UNICODE`; original fixture headers retained.
- Additional extended-grapheme cases adapted from `tests/test.rs` in [unicode-segmentation 1.13.3](https://crates.io/crates/unicode-segmentation/1.13.3), Copyright 2012–2015 The Rust Project Developers. `COPYRIGHT`, `LICENSE-MIT` and `LICENSE-APACHE` copied from that distribution. Word, sentence, legacy grapheme and chunked cursor APIs are not part of the replacement surface.
