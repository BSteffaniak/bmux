# bmux_config_doc

Types and trait for auto-generating configuration documentation.

## Overview

Defines the `ConfigDocSchema` trait and `FieldDoc` metadata type that the docs
site uses to generate the configuration reference page. Config structs
implement this trait via the companion `bmux_config_doc_derive` proc macro,
which extracts field names, types, doc comments, and default values at compile
time.

## Core Types

- **`ConfigDocSchema`**: Trait providing `section_name()`, `section_description()`, `field_docs()`, and `default_values()`
- **`FieldDoc`**: Metadata for a single config field -- TOML key, type display string, description, and optional enum values

## Usage

```rust
use bmux_config_doc::{ConfigDocSchema, FieldDoc};

// Typically used by the docs site to iterate config sections:
let schema = MyConfigSection::default();
for field in schema.field_docs() {
    println!("{}: {} -- {}", field.toml_key, field.type_display, field.description);
}
```
