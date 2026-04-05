# bmux_cli_output

Shared CLI output formatting helpers for bmux.

## Overview

Provides an ASCII table renderer used by CLI commands and plugins to produce
aligned, human-readable tabular output. Has zero dependencies so plugins can
use it without pulling in the rest of the CLI.

## Features

- Column alignment (left, right, center)
- Configurable minimum column widths
- Automatic width calculation from content
- Header row with separator line
- Supports both `std::io::Write` and `std::fmt::Write` outputs

## Core Types

- **`Table`**: Column definitions and row data
- **`TableColumn`**: Header text, alignment, and minimum width (builder pattern)
- **`TableAlign`**: `Left`, `Right`, or `Center`

## Usage

```rust
use bmux_cli_output::{Table, TableColumn, TableAlign};

let table = Table::new(
    vec![
        TableColumn::new("NAME"),
        TableColumn::new("STATUS").align(TableAlign::Right),
    ],
    vec![
        vec!["my-session".into(), "active".into()],
    ],
);
write_table(&mut std::io::stdout(), &table)?;
```
