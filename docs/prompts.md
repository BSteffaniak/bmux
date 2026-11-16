# Prompts and forms

Plugins ask the user for input through the generic prompt host
(`bmux_plugin_sdk::PromptRequest`). The attach client renders every prompt as
one centered modal with three regions:

- **Header**: the title (on the border, or inside it when a theme extension
  owns the border) and an optional wrapped `message`.
- **Body**: the prompt's control. Long bodies scroll and show a scrollbar.
- **Footer**: key hints and, where useful, `[ Submit ] [ Cancel ]` buttons.

The modal is sized by measuring its content, so it grows to fit and shrinks
on small terminals without overlapping regions. When space is tight the
spacer rows go first, then the hint row, then the footer.

## Collect several values with one form

Use `PromptRequest::form` whenever a flow needs more than one value. Avoid
chaining separate prompts: a chain gives no context, no way back, and loses
every earlier answer on Esc.

```rust
use bmux_plugin_sdk::{
    PromptFormField, PromptFormSection, PromptOption, PromptRequest, PromptValidation,
};

let request = PromptRequest::form(
    "Cut Recording",
    vec![PromptFormSection::new(
        "cut",
        "Cut",
        vec![
            PromptFormField::text("seconds", "Seconds", "")
                .placeholder("last N seconds")
                .validation(PromptValidation::PositiveInteger)
                .required(true)
                .description("Counting back from now."),
            PromptFormField::choice(
                "format",
                "Format",
                vec![PromptOption::new("gif", "GIF"), PromptOption::new("mp4", "MP4")],
                0,
            ),
            PromptFormField::toggle("open", "Open when done", true),
        ],
    )],
)
.message("Export the end of the recording.")
.submit_label("Export");
```

The response is `PromptValue::Form`, a map from field id to
`PromptFormValue`.

Field kinds:

| Kind           | Control                                          |
| -------------- | ------------------------------------------------ |
| `Text`         | Inline text input with placeholder/validation    |
| `Integer`      | Numeric input; `shift`/`ctrl` + arrows step 5/10 |
| `Number`       | Decimal text input with optional range           |
| `Bool`         | `[x]` toggle                                     |
| `SingleSelect` | Inline `(•)` radio options, wrapped as needed    |
| `MultiToggle`  | Inline `[x]` checkboxes, wrapped as needed       |

Sections with titles are shown as headers when a form has more than one
section; a section `description` is always shown. A field `description` is
shown as help under the focused field.

### Validation and errors

Fields are validated when focus leaves an edited field and on submit. Enter
submits when every field is valid; otherwise errors appear under their fields
and focus moves to the first invalid one. While a field shows an error, the
error clears as soon as the value becomes valid.

For checks the host cannot run (network, filesystem, uniqueness), validate
after submit and reopen the form with the user's values as initial values and
`PromptFormField::error` set on the failing fields. The form then opens focused
on the first field with an error.

### Layout

Forms use a **compact** layout by default: one row per field with a
right-aligned label column. **Stacked** puts labels above bordered inputs and
always shows help text. Plugins may hint a layout with
`PromptRequest::form_layout`; users override it with:

```toml
[appearance]
prompt_form_layout = "stacked" # or "compact"; "auto" follows the plugin hint
```

## Keys

| Key                       | Action                                        |
| ------------------------- | --------------------------------------------- |
| `tab` / `shift+tab`       | Next / previous field, then Submit and Cancel |
| `down` / `up`             | Next / previous field                         |
| `enter`                   | Submit (Cancel when Cancel is focused)        |
| `esc`                     | Cancel                                        |
| `left` / `right`, `space` | Change a choice or toggle                     |
| `shift`/`ctrl` + arrows   | Step integer fields by 5 / 10                 |
| `ctrl+r`                  | Reset to defaults (resettable forms)          |
| `F6`                      | Hide the modal to peek at live previews       |

Text fields accept every printable key, including `j`, `k`, `r`, and space.
While hidden with `F6`, the prompt keeps input ownership and ignores keys
other than `F6` and `esc`.

Mouse: click a field or option to focus or change it, click the footer buttons,
and use the wheel to scroll a long form.
