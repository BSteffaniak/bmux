# bmux Feedback

Give Bcode feedback about what is on screen: `shift+b` in runtime mode opens a prompt, and
the feedback goes to the Bcode feedback store (`bcode feedback list`) and its intake session,
with facts about the focused pane. While it triages, the intake agent may attach the span
of bmux's rolling recording that shows the problem; typed input is never attached.

Bcode is reached through its session bridge and only once feedback is given. By default it
runs inside the bmux server (the `embedded-bcode` feature) and keeps sessions in the user's
Bcode store; an installed Bcode is used instead when one is named or selected. Settings:

```toml
[plugins.settings."bmux.feedback"]
runtime = "auto"                # auto | installed | embedded
auto_preference = "embedded"    # what auto uses when no Bcode is named
bcode = "/path/to/bcode"        # BCODE_BIN outranks it
max_capture_seconds = 120
```

Disable it with `plugins.disabled = ["bmux.feedback"]`.
