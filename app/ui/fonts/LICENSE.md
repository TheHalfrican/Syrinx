# Bundled fallback fonts

These fonts are registered **only on non-Linux builds** (see
`register_fallback_fonts()` in `app/src/main.rs`) as a glyph *fallback* so the
UI's symbol glyphs (⇄ ♫ ◉ ✂ ▤ ⚙ ⏸ ⏻ ⧉ ＋ …) render on Windows/macOS, whose
text stack does not fall back to system fonts the way Linux fontconfig does.
They are never made the primary/default family, so the themes' own font choices
(e.g. Tahoma for the '95 skin) still win for every glyph they cover. Linux does
not register them at all and keeps its exact fontconfig behavior.

## DejaVuSans.ttf — DejaVu Sans 2.37

Covers 42 of the 46 non-ASCII UI glyphs. Derived from Bitstream Vera; freely
embeddable and redistributable.

License (Bitstream Vera + DejaVu changes): the fonts are free. You may use,
study, modify and distribute them, including embedding. "Bitstream Vera" and
"DejaVu" trademark terms apply to renamed derivatives only; this file is the
unmodified upstream font. Full text: https://dejavu-fonts.github.io/License.html

## SyrinxFallback.ttf — SIL Open Font License 1.1

A ~2 KB purpose-built font containing exactly the four glyphs DejaVu Sans lacks:

- U+23F8 ⏸ DOUBLE VERTICAL BAR  — from Noto Sans Symbols 2
- U+23FB ⏻ POWER SYMBOL          — from Noto Sans Symbols 2
- U+29C9 ⧉ TWO JOINED SQUARES    — from Noto Sans Math
- U+FF0B ＋ FULLWIDTH PLUS SIGN   — from Noto Sans JP

Built by subsetting each source to the single needed codepoint and merging
(fontTools). All three sources are Google Noto fonts under the SIL Open Font
License 1.1 (https://openfontlicense.org), which permits subsetting, merging,
and redistribution (including bundling in software). The merged font is
therefore also OFL-1.1. It is named "Syrinx Fallback" purely so it can be
referenced; it is a mechanical subset of the OFL Noto originals.
