# BMUX Diagram font

`BmuxDiagram-Regular.ttf` is a renamed derivative of JetBrains Mono (Nerd Fonts
v3.4.0 input already in this repository) and Noto Emoji from google/fonts commit
`67bc647ce1910e92f461bcc82aae30f21d21a9cc`, file
`ofl/notoemoji/NotoEmoji[wght].ttf`.

Input SHA-256:
- JetBrains: `0ec29a68b539ece7078fc714cebff0c0accb2f4948f8f7963d9f5e86633b12d9`
- Noto Emoji: `de6c18832938afc99caf132b39d6a30a19bac7f2e812e28db2535b4608d27551`

Regenerate with fontTools 4.60.1:
`python3 scripts/build-diagram-font.py /path/to/NotoEmoji.ttf`.
The script performs no downloads. The derivative is about 866 KiB, regular
weight, monochrome, with Latin/Greek/Cyrillic technical text and Noto symbols/
emoji. It excludes private-use Nerd glyphs and complex ligatures. It is not a
universal CJK/RTL/ZWJ font. System fallback can extend coverage.

Both input licenses are SIL OFL 1.1, retained under `packages/fonts/licenses/`:
`JETBRAINS_MONO_OFL.txt` and `NOTO_EMOJI_OFL.txt`. The derivative is distributed
under SIL OFL 1.1 and renamed BMUX Diagram, not an upstream reserved font name.
The guest and presenter load the exact same bytes. Ordinary builds do not need
Python/fontTools or network access to generate the font.
