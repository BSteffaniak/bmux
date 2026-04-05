# bmux-docs-site

The bmux documentation website.

## Overview

Built with the HyperChad framework, this crate serves the bmux documentation as
a web application with SPA-like navigation. It pulls type information from
`bmux_cli_schema` and `bmux_config` to auto-generate the CLI and configuration
reference pages.

## Routes

- `/` / `/home` -- landing page
- `/docs/installation` -- build-from-source instructions
- `/docs/quickstart` -- getting started guide
- `/docs/cli` -- auto-generated CLI reference
- `/docs/config` -- auto-generated configuration reference
- `/docs/playbooks` -- headless scripted execution
- `/docs/images` -- terminal image protocol support
- `/docs/plugins` -- plugin system overview
- `/docs/plugin-sdk` -- plugin authoring guide
- `/docs/plugin-example` -- walkthrough of the example plugin
- `/docs/changelog` -- release history
- `/faq` -- frequently asked questions
