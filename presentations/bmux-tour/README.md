# BMUX engineering tour

A 15-minute technical walkthrough for engineers, with no assumed contributor background. Eight main slides, with deeper material in a separate appendix.

## Files

- [Main deck](deck.md): audience content, timed presenter notes, and transitions.
- [Appendix](appendix.md): six optional technical slides for questions.
- [Sources](sources.md): code/docs supporting the technical claims.
- `assets/`: editable SVG diagrams and bundled PNGs used by the deck.

## Present

From the repository root, with a current BMUX build and the presenter plugin installed:

```sh
bmux present play presentations/bmux-tour/deck.md
```

For rehearsal with speaker notes:

```sh
bmux present play presentations/bmux-tour/deck.md --presenter
```

Use arrows/Space to advance, Left to return, and q to quit. Do not screen-share the presenter viewport: it contains private notes. Verify a separate audience display arrangement before presenting with notes.

PNG diagrams require a supported terminal image protocol. Check legibility at the actual audience dimensions before the talk. Keep the assets beside the deck when copying it.

The deck runs locally without a Mermaid worker, network fetches, credentials, or custom Rust registrations.

## Prepare

Rehearse using the notes embedded in the slides. Add your real motivation sentence and approve the closing next-step wording. Target 14 minutes, leaving a minute for delays or questions.
