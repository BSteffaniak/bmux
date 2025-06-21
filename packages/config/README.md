# bmux_config

Configuration management for bmux terminal multiplexer.

## Overview

This package handles all configuration aspects of bmux, including file paths, themes, keybindings, and behavior settings.

## Features

- Configuration file loading and validation
- Default configuration generation
- Environment-specific settings
- Theme and keybinding management
- Path resolution utilities

## Configuration Structure

- **Behavior**: Terminal behavior settings
- **Appearance**: Visual themes and styling
- **Keybindings**: Custom key mappings
- **Paths**: File and directory locations

## Usage

```rust
use bmux_config::BmuxConfig;

let config = BmuxConfig::load_or_default()?;
```
