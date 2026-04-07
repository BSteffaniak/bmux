# bmux prompt showcase

This example runs an isolated sandboxed bmux server and launches attach mode,
then drives a sequence of modal prompts to showcase:

- confirm
- text input
- single select
- multi toggle

## Run

```bash
cargo run -p bmux_prompt_showcase
```

After the final prompt, detach from attach mode with the default key sequence:

- `Ctrl+b`, then `d`
