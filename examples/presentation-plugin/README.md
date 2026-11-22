# Presentation example

Draws attach UI without running inside the attach client: at activation it
publishes its whole presentation (a right-hand panel) as retained JSON state on
`example.presentation/presentation`, declared in `plugin.toml` with
`forward_to_streaming_clients = true`. The server forwards it to every attach
client, which draws it through the same layout and surface registries as the
bundled sidebar and tab bar. The JSON is `bmux_plugin::surface::PublishedPresentation`;
see `docs/plugins.md`.
