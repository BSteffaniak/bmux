# bmux-docs-site

The bmux documentation website.

## Overview

Built with the HyperChad framework, this crate serves the bmux documentation as
a web application with SPA-like navigation. It pulls type information from
`bmux_cli_schema` and `bmux_config` to auto-generate the CLI and configuration
reference pages.

## Logging

The server writes synchronously to stderr using `tracing-subscriber`, including
records from dependencies using the `log` facade. The process-global subscriber
owns the writer for its lifetime; no background-worker handle is needed. No log
files or runtime directories are created. The cached `moosicbox_log_runtime`
initializer creates directories even with its file sink disabled, so it is not
suitable for this stderr-only entry point.

`RUST_LOG` is the only filter environment variable. Without a usable directive
(including an unset or empty value), the default is `error` in both debug and
release builds. Examples: `RUST_LOG=debug` or
`RUST_LOG=error,bmux_docs_site=debug`. Comma-separated target-prefix directives
and `off` are supported. `MOOSICBOX_LOG` is not consulted. Invalid directives
are diagnosed on stderr and ignored by the tracing filter parser.

This is not byte-for-byte or grammar parity with `env_logger`:

- Output uses tracing's timestamp, level, target and event formatting rather
  than env_logger's bracketed format. Native tracing events are also captured.
- Filtering uses tracing's `EnvFilter` grammar, including span/field directives.
  The env_logger `/message-regex` suffix is not supported; tracing regex matching
  applies to span field values instead, not a global formatted-message filter.
- ANSI color is enabled only when stderr is a terminal. Redirected stderr is
  uncolored. `RUST_LOG_STYLE` no longer controls color.
- Duplicate initialization returns an error; startup propagates that error
  instead of env_logger's initialization panic.

The `logging` integration-test target exercises the same private initializer in
isolated subprocesses, with redirected stderr and isolated filesystem paths.
The existing library and application-binary test targets remain disabled. Run
`cargo test -p bmux_docs_site --test logging` in the repository's Nix
development
shell.

## Routes

- `/` / `/home` -- landing page
- `/docs/installation` -- build-from-source instructions
- `/docs/quickstart` -- getting started guide
- `/docs/concepts` -- core bmux concepts and architecture mental model
- `/docs/cli` -- auto-generated CLI reference
- `/docs/command-cookbook` -- task-oriented command recipes
- `/docs/kiosk` -- kiosk SSH access and token workflow guide
- `/docs/config` -- auto-generated configuration reference
- `/docs/setup-guide` -- practical setup flows for local/remote/hosted
- `/docs/playbooks` -- headless scripted execution
- `/docs/images` -- terminal image protocol support
- `/docs/troubleshooting` -- debugging and failure triage playbook
- `/docs/operations` -- maintenance and operations workflows
- `/docs/docs-snippet-tags` -- author guide for verified snippets
- `/docs/plugins` -- plugin architecture
- `/docs/bpdl-spec` -- normative BPDL grammar and semantics
- `/docs/plugin-sdk` -- plugin authoring guide
- `/docs/plugin-example` -- walkthrough of the example plugin
- `/docs/plugin-ops` -- plugin operations playbook
- `/docs/plugin-triage-playbook` -- plugin failure triage runbook
- `/docs/plugin-perf-troubleshooting` -- plugin performance troubleshooting
- `/docs/changelog` -- release history
- `/faq` -- frequently asked questions
