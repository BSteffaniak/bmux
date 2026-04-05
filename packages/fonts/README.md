# bmux_fonts

Bundled font presets and registration for bmux renderers.

## Overview

Provides preset font configurations and embedded font data for bmux's rendering
pipelines (recording export, status bar, etc.). Fonts are compiled into the
binary as static byte arrays when the `bundled-fonts` feature is enabled,
removing the need for runtime font file discovery.

## Features

- Font presets with curated fallback chains (`GhosttyNerd`, `SystemMonospace`)
- Embedded Nerd Font data (`bundled-nerd-fonts` feature)
- Registration into `fontdb::Database` for resvg/text rendering
- Loading as `ab_glyph::FontArc` for glyph rasterization
- Zero runtime filesystem access when using bundled fonts

## Core Types

- **`FontPreset`**: `GhosttyNerd` (JetBrainsMono Nerd Font) or `SystemMonospace` (SF Mono fallback chain)
- **`FontStyle`**: `Regular`, `Bold`, `Italic`, `BoldItalic`
- **`EmbeddedFont`**: Family name, style, and static byte data

## Usage

```rust
use bmux_fonts::{FontPreset, register_preset_fonts, load_preset_fonts_for_ab_glyph};

// Register into a fontdb database
let mut db = fontdb::Database::new();
let count = register_preset_fonts(&mut db, FontPreset::GhosttyNerd);

// Or load as ab_glyph fonts
let fonts = load_preset_fonts_for_ab_glyph(FontPreset::GhosttyNerd);
```
