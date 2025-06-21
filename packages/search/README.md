# bmux_search

Search functionality for bmux terminal multiplexer.

## Overview

This package provides search capabilities across sessions, panes, and terminal content within bmux.

## Features

- Content search across panes
- Session and window search
- Regular expression support
- Search history and bookmarks

## Core Components

- **SearchManager**: Search orchestration
- **SearchQuery**: Query processing
- **SearchResults**: Result handling and navigation

## Usage

```rust
use bmux_search::SearchManager;

let manager = SearchManager::new();
// Search operations
```
