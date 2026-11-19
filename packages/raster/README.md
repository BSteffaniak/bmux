# bmux_raster

Neutral, bounded pixel operations for BMUX. PNG support is always available;
`formats` adds JPEG, GIF (first displayed frame) and BMP input. Callers supply
image dimension and allocation limits; surface-wide budgets, placement,
clipboard policy and terminal protocol negotiation stay in their owners.
Rendered samples use tightly packed straight-alpha RGBA8. PNG validation
consumes the image data; reading dimensions alone does not establish validity.
