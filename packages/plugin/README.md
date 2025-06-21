# bmux_plugin

Plugin system for bmux terminal multiplexer.

## Overview

This package provides a plugin architecture for extending bmux functionality with custom plugins and extensions.

## Features

- Plugin loading and management
- Plugin API definitions
- Extension points
- Plugin lifecycle management

## Core Components

- **PluginManager**: Plugin orchestration
- **Plugin**: Plugin interface definitions
- **PluginRegistry**: Plugin discovery and loading

## Usage

```rust
use bmux_plugin::PluginManager;

let mut manager = PluginManager::new();
// Plugin operations
```
