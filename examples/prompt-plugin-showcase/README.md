# bmux plugin prompt showcase

This example runs an isolated sandboxed bmux server and launches attach mode,
then executes a prompt sequence implemented in the `example.native` plugin
crate (`examples/native-plugin`).

It demonstrates plugin-provided modal prompts using the same sandbox harness
pattern as `examples/prompt-showcase`.

## Run

```bash
cargo run -p bmux_prompt_plugin_showcase
```

After the final prompt, detach from attach mode with the default key sequence:

- `Ctrl+b`, then `d`
