# Documentation publishing

When adding or substantially changing public-facing documentation under `docs/`,
keep the documentation website in sync in the same change.

- Register relevant current user guides and developer references in `DOC_PAGES`
  in `packages/docs-site/ui/src/doc_pages.rs`. That registry drives site routes,
  sidebar navigation, and Markdown link rewriting; files are not auto-published.
- Choose an audience-appropriate section in `DOC_SECTIONS`, a unique `/docs/...`
  route, and clear titles/navigation labels. Preserve existing public routes.
- Label experimental integrations and distinguish implemented behavior from
  proposals. Reconcile conflicting availability/support claims before publishing.
- Historical measurements, migration inventories, draft plans, and agent
  instructions need not appear in the public navigation. For top-level Markdown
  files intentionally omitted from the site, add a specific reason to
  `SITE_EXCLUSIONS` in the registry's tests. Remove the exclusion when publishing.
- The coverage test checks every top-level `docs/*.md` file. Nested ADRs and
  benchmark artifacts are outside that check; still assess new nested public
  guides for registration rather than assuming they will appear automatically.

Run `cargo test -p bmux_docs_site_ui` using the repository's Nix toolchain to
validate registry coverage, routes, sources, and documentation snippets. Follow
root `AGENTS.md` for all additional validation required by the change; a Rust
registry edit is a code change, not a docs-only change.
