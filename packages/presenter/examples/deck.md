---
id: markdown-demo
title: BMUX presentations
author: BMUX
theme: dark
---
<!-- markdownlint-disable MD025 -->
<!-- Slide decks intentionally have one top-level heading per slide. -->
<!-- bmux:slide-id intro -->

# BMUX presentations

Markdown authoring and linked Rust applications share one portable model.

<!-- bmux:pause -->

Reveals are cumulative. Hidden content reserves geometry by default.

<!-- bmux:notes -->

These notes are private and are not rendered in the audience viewer.

<!-- bmux:end-notes -->

---

<!-- bmux:slide-id content -->
<!-- bmux:columns 1,1 -->

## Code

```rust
let state = PresentationState::new(&deck);
```

<!-- bmux:column -->

| Feature | Owner |
| --- | --- |
| Layout | BMUX |
| State | Application |

<!-- bmux:end-columns -->

---

<!-- bmux:slide-id image -->

# A bundled image

![Two authoring paths feeding one presentation model](architecture.png)

Blue and green inputs feed the shared purple model, then the orange viewer.

---
<!-- bmux:slide-id mermaid -->

# Mermaid rendering

```mermaid
flowchart LR
  Markdown --> Model
  Rust --> Model
  Model --> BMUX
```
