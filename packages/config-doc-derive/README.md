# bmux_config_doc_derive

Proc macros for generating configuration documentation schema.

## Overview

Provides `#[derive(ConfigDoc)]` for config structs and `#[derive(ConfigDocEnum)]`
for config enums. These macros generate `ConfigDocSchema` trait implementations
that extract TOML field names, Rust doc comments, type information, enum variant
values (respecting serde rename rules), and serialized defaults -- all consumed
by the docs site to auto-generate the configuration reference.

## Macros

- **`#[derive(ConfigDoc)]`**: For config structs. Generates `ConfigDocSchema` impl with field metadata.
- **`#[derive(ConfigDocEnum)]`**: For config enums. Generates `config_doc_values()` returning variant names (applying serde rename rules like `snake_case`, `camelCase`, `kebab-case`).

## Usage

```rust
use bmux_config_doc_derive::{ConfigDoc, ConfigDocEnum};

/// Terminal behavior settings.
#[derive(ConfigDoc, Default)]
struct BehaviorConfig {
    /// The TERM value to advertise to inner programs.
    #[serde(default)]
    pane_term: String,
}

#[derive(ConfigDocEnum)]
#[serde(rename_all = "kebab-case")]
enum SplitMode {
    Horizontal,
    Vertical,
}
```
