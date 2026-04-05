# bmux_cli_schema

CLI argument definitions for bmux.

## Overview

Contains all clap derive structs and enums that define bmux's command-line
interface. Intentionally has no runtime dependencies beyond `clap`, so the docs
site can import it to auto-generate the CLI reference page without pulling in
the entire bmux runtime.

## Features

- Complete CLI tree definition (all subcommands, flags, arguments)
- Value enums for structured CLI inputs (log levels, recording formats, etc.)
- Zero runtime dependencies -- only `clap` derive
- Shared between the binary crate and the docs site

## Core Types

- **`BmuxCli`**: Root clap `Parser` struct
- **`Commands`**: Top-level subcommand enum (`server`, `session`, `attach`, `plugin`, `recording`, etc.)
- Value enums: `LogLevel`, `RecordingExportFormat`, `RecordingRenderMode`, `RecordingProfileArg`, etc.

## Usage

```rust
use bmux_cli_schema::BmuxCli;
use clap::Parser;

let cli = BmuxCli::parse();
```
