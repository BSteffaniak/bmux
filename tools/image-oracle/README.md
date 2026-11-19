# Raster compatibility oracle

This non-shipping workspace tests the migration from `image` 0.25.10 against
BMUX baseline `20c9a56cdee9e4e78e06f5ad7defd42850ff0009`. Its lockfile was
seeded from that revision. Run in the repository's Nix development environment:

```sh
cd tools/image-oracle
cargo run --offline --locked -- check
cargo run --offline --locked --features candidate-png -- check candidate
cargo run --offline --locked --features candidate -- check candidate
```

`check` never rewrites expectations. Valid codec TSV rows contain a name, width,
height, input hex and expected straight RGBA8 hex. Inputs are all small
deterministic synthetic images:

- `png.tsv`: all legal color type/bit depth combinations, standard and Adam7 scan order, grayscale/truecolor transparency keys, and partial palette transparency. The Python standard-library generator packs samples and computes expected pixels by formula, independently of both Rust implementations.
- `bmp.tsv`: spec-derived core/Windows headers, top-down and padded bottom-up rows, packed palette indices, RLE4/RLE8, and 565 bitfields.
- `formats.tsv`: frozen `image` encoder and decoder examples for RGB and grayscale JPEG, BMP, and animated GIF (first frame, including a subimage with nonzero offset).
- `invalid.tsv`: required rejections (malformed headers/data, zero and excessive dimensions). Positive cases also test per-axis and allocation rejection; candidate PNG validation checks decoded data and exact dimensions. Error text is not contractual.
- `transforms.tsv`: `pattern()` inputs and exact frozen output for mixed-axis nearest and Lanczos, identity, singleton axes, negative-offset overlay, and crop. Candidate comparison exercises the raster operations; crop is a caller-side reference golden.
- `compact()` checks the baseline projection threshold (256 KiB), strict raw-length comparison, and RGB/RGBA round trips for compressible and incompressible data. It tests codec suitability, not the production caller itself. Encoded bytes need not be identical.

To intentionally regenerate expectations: run `python3 generate_png.py`,
`python3 generate_bmp.py`, then `cargo run --locked --offline -- freeze`. Review
fixture diffs before accepting; never regenerate to accommodate a candidate
mismatch.

## Time and peak memory

Build once with both backends enabled. Use the executable printed by Cargo to
exclude compiler memory and startup from the encode/decode timers:

```sh
cargo run --offline --locked --release --features candidate -- check candidate
./perf.sh "$CARGO_TARGET_DIR/release/image-oracle" reference 3
./perf.sh "$CARGO_TARGET_DIR/release/image-oracle" candidate 3
```

Each sample is a fresh process, using the same 3840×2160 RGBA input (constant
or seeded noise), recording encode/decode wall time, encoded length, and process
lifetime maximum RSS. RSS includes the input, PNG and decoded pixels, not just
decoder scratch. macOS `time -l` reports bytes; Linux GNU `time -v` reports KiB.
Timings are observations, not pass thresholds.
