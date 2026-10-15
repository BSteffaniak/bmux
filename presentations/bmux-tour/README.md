# BMUX: a guided terminal tour

A zero-background story: **see and find real work → repeat it with playbooks → explain the mechanisms → let Jev fly**. The 23-slide main route introduces terms before using them, includes ready-to-speak private narration, and preserves all seven demos. Seven interface-heavy slides moved to optional appendix depth rather than interrupting the first-time explanation.

## Files

- [presenter-guide.md](presenter-guide.md): start here—setup, controls, fallback lines, slide route, and honest Q&A answers.
- [speaker-script.md](speaker-script.md): continuous ready-to-say narration and demo cue cards, in deck order.
- [deck.md](deck.md): beginner introduction, work/navigation, themes, playbooks, concrete architecture, output delivery, presentation composition, and Flappy Bird finale.
- [appendix.md](appendix.md): optional technical discussion of the demonstrated local features.
- [sources.md](sources.md): excerpt provenance, evidence limits, and final rehearsal checklist.
- [editorial-review.md](editorial-review.md): JEV review method, human decisions, implementation confidence, and acceptance limits.
- `demos/`: the six original embedded playbooks plus the opt-in [Jev Flappy Bird finale](demos/jev-flappy.md). Parsing the deck does not execute them.
- `assets/`: optional diagrams; the narrative does not depend on them.

Preserve stable `bmux:slide-id` and `bmux:block-id` identities when editing. Component identity participates in controller/reload behavior; do not reuse an ID for a different execution casually.

## Mechanisms behind the demonstrations

The main route uses actual implementation and shell-state checks. The three Mermaid models are now optional appendix material, not prerequisites for the main talk:

| Slide ID | Question answered | Read the arrows as |
| --- | --- | --- |
| `command-route` | Where does product behavior belong? | Actual plugin target resolution and bounded split ratio, followed by typed delegation |
| `ownership-map` | Did navigation return to the same shell? | Actual saved-PID and fixture commands from the navigation playbook |
| `preview-lifecycle` | Why is preview not a saved choice? | Successful token-owned preview/confirm/cancel paths; not all error transitions |
| `frame-transaction` | When is a rendering baseline safe to reuse? | Delivery success versus invalidation/full repaint; not atomic rollback |
| `demo-authority` | Who can send input to an embedded demo? | One worker-owned attach and a read-only output projection; not a second controller |

Each slide includes one takeaway and source/limitation notes. Colors reinforce the labels; no relationship depends on color alone. The diagrams are prepared by the native embedded Mermaid renderer, not browser-side Mermaid JavaScript. They remain fixed images within a responsive slide, so narrower screens shrink diagram labels rather than reflowing the graph. Rehearse readability at the actual audience dimensions.

## What the demos establish

The demos use generated isolated configuration with the built-in `tmux_compat` input profile and managed single-authority attach support, not a user's running session. `Ctrl-B` enters normal mode, Escape returns to shell input; visible UI actions use real key events and commands use attach paste plus Enter. No custom sandbox config or fabricated test/application status is needed.

| Block ID / source | Demonstrated behavior |
| --- | --- |
| `demo-panes` / `demos/panes.dsl` | Three panes, focus changes, layout/screen assertions |
| `demo-navigation` / `demos/navigation.dsl` | Original shell state survives tab switching; workspace switching returns to existing work |
| `demo-finder` / `demos/finder.dsl` | Real fuzzy `api` query from a generated second workspace, Enter selects the API tab |
| `demo-themes` / `demos/themes.dsl` | Select Theme previews every bundled preset plus default for two seconds each; Enter confirms Cyberpunk after the cycle; shell continues |
| `demo-rendering` / `demos/rendering.dsl` | Actual `seq` output, Page Up/Page Down scrollback, return to live shell; **not fault injection** |
| `demo-evidence` / `demos/evidence.dsl` | Fixture count plus independent second-pane count/sum, complete-line output/layout assertions and `verified-demo` snapshot |

The acceptance harness observes intermediate feature output and commits it to browser Canvas, retaining screenshots as well as managed results. **This does not establish venue legibility, final audience geometry, or final note privacy.** Demo acceptance is not a claim that all project validation is complete.

## Present with an approved executable

Review scripts/includes as executable input before approval. Isolated runtime/config/data paths are **not a security sandbox** for shell actions. Managed execution rejects `real-attach`, custom `@sandbox-config`, and `BMUX_`/`XDG_` environment runtime-selector overrides. Managed attach support does not relax these restrictions.

From the repository root, replace the placeholder with an explicitly approved immutable native artifact:

```sh
/path/to/approved/bmux present play presentations/bmux-tour/deck.md --allow-playbooks --no-reload
```

Add `--presenter` for private local speaker notes. Select matching presenter/playbook plugins and managed-worker/observer protocols. Record executable identity/hash; protocol compatibility alone does not authenticate content. Approval covers the actual frozen script/include bytes; changed executable input requires fresh approval. `--no-reload` keeps presentation inputs stable.

See [live component controls](../../docs/live-components.md):

| Control | Meaning |
| --- | --- |
| Tab, then `o` | Focus demo and open overlay; does not start execution |
| `r` | Explicitly prepare/start |
| `p` / `n` / `c` | Pause / one whole action including waits / resume at boundaries |
| `s` | Request stop |
| `R` | Restart a new generation after cleanup acknowledgment |
| Escape | Hide overlay only; execution may continue |
| Leave slide | Request stop; retained final output remains inspectable |
| `q` / Ctrl-C | Exit local presenter and request owned cleanup |

Use arrows/Space for normal slide navigation outside the overlay. A step is not seek/replay. A stop request is not cleanup acknowledgment; inspect unresolved diagnostics. The audience observer is output-only. Scripted attach commands/input exercise the owned managed runtime, not arbitrary audience keyboard/paste access.

## Repeat acceptance with explicit artifacts

[The tour harness](../../scripts/presenter-tour-acceptance.py) requires existing artifacts; it does not build them or attach to an existing user runtime:

```sh
python3 scripts/presenter-tour-acceptance.py \
  --binary /absolute/path/to/approved/bmux \
  --plugins /absolute/path/to/matching/plugins \
  --assets /absolute/path/to/matching/web-assets \
  --browser /absolute/path/to/browser \
  --state-parent /tmp
```

Mermaid rendering is embedded in the matching native presenter artifact; no separate renderer executable is needed. The tour demos do not cover Mermaid rendering; the dedicated `semantic_mermaid` case in `scripts/presenter-web-acceptance.py` checks loader PNG substitution and actual browser Canvas output at two widths.

All six demos run by default; `--demos finder themes` selects a subset. The harness checks HTTP snapshots, expected visible feature milestones, and browser/Canvas commits, retaining intermediate screenshots. Keep its printed evidence directory private: PTY output and runner artifacts may contain sensitive details. Compare the actual artifacts and source bytes when repeating a run; the recorded pass is not approval for arbitrary replacements.

## Final audience and privacy rehearsal

1. Approve the executable and current scripts; rehearse manual controls, stop/cleanup, and restart on the intended setup.
2. Check actual audience dimensions, legibility, focus/navigation, and pacing. If a run fails, disclose it and use the following implementation slide.
3. Do **not** screen-share the presenter viewport. Verify notes and private source paths are absent from public audience output.
4. Enable an optional browser audience only after separate native/WASM, authentication, delivery-mode, and Canvas acceptance. Document and fixed-frame delivery are distinct contracts; a query string cannot convert one host mode into the other. The browser is read-only; embedded rasters do not promise logical selection/copy.
5. Keep capability URLs, credentials, observer/runtime paths, and retained evidence private or redacted. See [responsive delivery](../../docs/responsive-presentations.md) and [web hosting](../../packages/web-host/README.md).
6. Close before the appendix unless questions call for deeper discussion. The appendix elaborates demonstrated local features; it is not a roadmap or a showcase for unfinished subsystems.
