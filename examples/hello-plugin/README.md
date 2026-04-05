# bmux_hello_plugin

Minimal hello-world bmux plugin example.

## Overview

The simplest possible bmux plugin -- implements a single `hello` command that
prints a greeting. Use this as the starting point when creating a new plugin.

For a more comprehensive example covering services, lifecycle hooks, event
subscriptions, and cross-plugin calls, see the
[native-plugin example](../native-plugin/README.md).

## Build

```bash
cargo build -p bmux_hello_plugin
```

## Usage

```bash
bmux plugin run hello hello world
# => Hello, world!
```
