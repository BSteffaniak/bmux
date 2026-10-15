---
id: quickstart
title: BMUX presentation quickstart
author: BMUX
theme: dark
---
<!-- markdownlint-disable MD025 -->
<!-- Slide decks intentionally have one top-level heading per slide. -->
<!-- bmux:slide-id welcome -->

# Hello, BMUX

A self-contained terminal presentation. No external assets or custom components.

Use arrows or Space to advance, Left to go back, and q to quit.

<!-- bmux:pause -->

This paragraph appears on the next step. Reveals are cumulative.

<!-- bmux:notes -->

These notes appear with --presenter, not in the audience view.
Pause here to demonstrate navigation and incremental reveals.

<!-- bmux:end-notes -->

---
<!-- bmux:slide-id code -->

# Syntax highlighting

```rust
fn main() {
    let message = "Hello from BMUX!";
    println!("{message}");
}
```

Code is displayed, not executed.

---
<!-- bmux:slide-id columns -->

# Side by side

<!-- bmux:columns 1,1 -->

## Authoring

- Markdown files
- Rust builders
- One shared deck model

<!-- bmux:column -->

## Presentation

- Incremental reveals
- Private speaker notes
- BMUX terminal components

<!-- bmux:end-columns -->

---
<!-- bmux:slide-id finish -->

# Try live reload

Edit this file and save it while the presentation is open.

Use Home to return to the first slide, End to jump here, and q to exit.
