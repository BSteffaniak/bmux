# BMUX Unicode segmentation

`graphemes(&str)` and `grapheme_indices(&str)` borrow extended grapheme clusters
from the original string. Offsets are UTF-8 bytes relative to the supplied
slice. Both iterators support cloning and traversal from either end without
allocating. Segmentation does not normalize text, measure display width,
interpret VT controls, or decode incomplete byte streams.

Unicode 17.0.0 data is pinned to match the previously used unicode-segmentation 1.13.3. See [data provenance and regeneration](data/README.md). Run `cargo test -p bmux_unicode_segmentation` for scalar lookup, boundary and iterator checks. Independent baseline reproduction instructions live in `tools/grapheme-oracle` at the repository root.
