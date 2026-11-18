# Unicode 17.0.0 grapheme properties

`17.0.0/` contains unmodified copies from the Unicode Consortium:

- [GraphemeBreakProperty.txt](https://www.unicode.org/Public/17.0.0/ucd/auxiliary/GraphemeBreakProperty.txt) — Grapheme_Cluster_Break.
- [emoji-data.txt](https://www.unicode.org/Public/17.0.0/ucd/emoji/emoji-data.txt) — Extended_Pictographic, including reserved code points explicitly marked in the file.
- [DerivedCoreProperties.txt](https://www.unicode.org/Public/17.0.0/ucd/DerivedCoreProperties.txt) — Indic_Conjunct_Break (InCB).
- [LICENSE.txt](https://www.unicode.org/license.txt) — Unicode License v3, applicable to the data and derived tables. The license URL is unversioned; the local copy is pinned by digest.

`17.0.0/SHA256SUMS` records SHA-256 hashes of the downloaded bytes, not an
upstream signature. From the package directory, `python3 tools/generate.py`
verifies these pins and regenerates `src/properties.rs` using only the Python
standard library and local files. `python3 tools/generate.py --check` verifies
without writing. The Rust build reads the generated source; no generator or
network access is needed.

The table shares identical 256-code-point pages. Page IDs follow first
occurrence in ascending code point order. Unlisted properties default to Other,
No and None, respectively. The Rust test in `tools/verify_properties.rs`
independently parses the raw inputs into a flat reference and checks the
production lookup on every Unicode scalar. To run it alone in the repository's
Nix shell, from the package directory:

```sh
tmp=$(mktemp -d)
rustc --edition 2024 --test src/properties.rs -o "$tmp/verify" && "$tmp/verify"
```
