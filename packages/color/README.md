# bmux_color

Terminal-independent color values and perceptual color math.

* `Rgb`: 8-bit sRGB with hex parsing and WCAG luminance/contrast.
* `Oklab`/`Oklch`: perceptual spaces with gamut mapping by chroma reduction.
* `mix`, `lighten`, `darken`, `saturate`, `desaturate`, `ensure_contrast`.
* `Gradient`: OKLab-interpolated gradients.
* `ansi`: xterm 256/16-color palettes and perceptual nearest-color mapping.

This crate has no terminal, frame, or style types, so renderer-neutral layers
may depend on it. It must not depend on `bmux_tui*`.
