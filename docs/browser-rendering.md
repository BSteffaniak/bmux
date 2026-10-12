# Browser rendering

BMUX shares retained presentation primitives between terminal output and a Rust/WASM browser renderer. Canvas 2D is the initial browser backend; `BrowserBackend` keeps browser output replaceable without exposing Canvas, DOM, or GPU objects in the shared contract. HTML and WebGL backends are not implemented yet.

## Build and observe a local view

Use the locked Nix shell throughout:

```sh
bash scripts/dev.sh cargo install wasm-bindgen-cli --version 0.2.126 --locked --root "$PWD/target/web-tools"
bash scripts/dev.sh bash scripts/build-web-renderer.sh
bash scripts/dev.sh cargo run -p bmux_cli --bin bmux -- attach "<SESSION_UUID>" --web-assets "$PWD/target/web-assets"
```

The terminal attachment prints a loopback browser URL containing a short-lived access capability in its fragment. Open it locally; do not share or record that URL. The browser removes the fragment before making requests and keeps the capability only in memory. Reloading requires opening the original capability URL again.

This initial implementation **observes an existing terminal attachment**; it is not a standalone headless browser attachment. Replace `<SESSION_UUID>` with an ID from `bmux list-sessions --json`. The terminal remains interactive. The browser cannot send terminal input or mutate plugin state. Native provider-only attachment is explicitly unsupported by this observation entry point.

The host binds an ephemeral IPv4 loopback port, restricts assets to an explicit allowlist, checks Host/Origin and capability headers, limits connections and request/output sizes, and closes with the attachment. There is no unauthenticated remote deployment mode.

## Shared boundaries

- `bmux_tui::presentation` prepares owned cells, cursor, images and interaction/selection/semantic metadata. Successful output alone commits retained authority.
- The attach observer projects the same terminal lowering plans and retained compositor identities. Observed attachments currently force complete terminal repaint to produce coherent repaired snapshots; this is a correctness-first tradeoff, not an optimized delta transport.
- `bmux_web_renderer` validates bounded versioned output snapshots, stages Canvas output/resources, and commits metadata after successful backend output. It has no native client, daemon or plugin-loader dependency.
- `bmux_web_host` serves opaque immutable full snapshots using a latest-only feed. Transport publication revisions are distinct from presentation revisions and attachment generations.
- Full snapshots replace omitted surfaces and images. Slow consumers skip intermediate snapshots and recover from the latest complete value; incompatible or over-budget projection closes observation rather than silently presenting incomplete output.

Surface provenance preserves owner-local identity, retained identity, revision, clipping, ordering, content identity and continuation watermark. It is descriptive output, not another composition authority.

Standalone TUI consumers can prepare the same presentation and use `BrowserFrame::from_prepared`; `packages/web-renderer/examples/snapshot.rs` demonstrates this without native attachment dependencies.

## Future backends

A faithful HTML or WebGL backend should consume the same validated presentation and implement the browser backend contract. Backend resource caches remain private; geometry, clipping, ownership, selection and committed interaction semantics remain shared. A responsive, web-native HTML presentation would be a distinct presentation policy using preserved semantics, not a backend silently rearranging an already-composed frame.

Interactive browser input, mobile behavior, web-native HTML and remote hosting remain later work. Image formats unsupported by the neutral projection fail explicitly; they are not silently omitted.
