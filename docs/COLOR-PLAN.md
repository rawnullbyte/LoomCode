# Colour and syntax highlighting — plan

Goal: make code and prose in the transcript more readable, and let the palette express more
than four distinguishable colours. This states what exists, what is actually wrong (verified,
not assumed), and the order to fix it in.

## What exists today (verified by reading and by probe)

| Piece | Where | State |
| --- | --- | --- |
| Palettes | `theme.rs` | 7: `terminal`, `dark`, `light`, `ocean`, `violet`, `ember`, `graphite`. Slots: base, dim, accent, ok, err, warn, member. |
| Palette translation | `ui.rs::normalize_line` | Remaps the lexer's ANSI slots at render time: `Magenta\|Cyan → accent`, `Green → ok`, `Red → err`, `Yellow → warn`, `DarkGray → dim`, `Reset → base`. |
| Syntax lexer | `code.rs` | 9 families; token classes: keyword, string, comment, number, plain. |
| Prose styling | `transcript.rs:185-259` | Inline `` `code` ``, `#` headings, fenced blocks, diff lines. |
| Config knobs | `schema.rs:333` | None for syntax or colour. |

## What is NOT wrong (checked first, so it is not "fixed" by mistake)

Syntax colours are **already palette-aware**. A probe over all seven palettes confirmed
`normalize_line` maps each token onto the palette and yields four distinct token colours
everywhere, including `light`. The lexer emits ANSI names and the theme remaps them — that
indirection is the design, not a defect. Rewriting the lexer to emit explicit RGB would fight
it and break ~20 existing style comparisons.

## The real limitations

1. **Only ~4 distinct colours survive the mapping.** `Magenta` and `Cyan` both map to
   `accent`, so a keyword and inline code cannot be told apart. The vocabulary, not the
   translation, is the constraint.
2. **Only five token classes.** No function, type, operator, attribute or lifetime, so code
   reads as keywords-strings-comments-numbers-and-everything-else.
3. **Six languages alias to JavaScript.** `c`, `h`, `cpp`, `java`, `kotlin`, `kt`, `go`,
   `swift`, `css`, `c#` all resolve to `Lang::JavaScript`, so a Go file is coloured by
   JavaScript rules; `Yaml` and `Dockerfile` are lumped into `Toml`.
4. **Prose is thin.** Bold, italic, links, lists, blockquotes, rules and tables all render as
   plain text.

## Invariants every change must keep

1. **Transparent mode**: every application-owned cell has `bg = Color::Reset`
   (`tests/transparency.rs::assert_all_cells_bg`).
2. **Piece concatenation**: `highlight` pieces must join back to the input exactly
   (`highlight_invariant_tests::pieces_concatenate_to_the_input_exactly`). Copy and selection
   reassemble by position; a dropped char corrupts a paste.
3. **Linear time**: `highlighting_a_long_line_stays_fast`.
4. **No reverse video.**
5. **Any new token colour must be added to `normalize_line`'s match.** It ends with
   `color => color`, so an unmapped colour passes through raw and some terminals (kitty)
   remap 8-bit names — a new class would silently render wrong on some setups.

## Phases

### Phase 1 — More distinguishable token classes (highest value, do first)

Keep the ANSI-slot vocabulary and extend both ends of it. Add one palette slot and lean on
modifiers, which `normalize_line` already preserves:

| Class | Slot + modifier | Renders as |
| --- | --- | --- |
| keyword | `Magenta` + BOLD | accent bold |
| type / trait | `Cyan` + ITALIC | accent italic |
| function / method | `Blue` (new slot) | `fg_fn` per palette |
| attribute / decorator | `Yellow` + BOLD | warn bold |
| string | `Green` | ok |
| number | `Yellow` | warn |
| operator / punctuation | `DarkGray` | dim |
| comment | `DarkGray` + ITALIC | dim italic |

Work: add `fg_fn` to `Theme` (and per palette), map `Blue → fg_fn` in `normalize_line`, add
the classes to the lexer (`identifier` before `(` → function; `UpperCamelCase`/`SCREAMING` →
type; `#[`, `@` → attribute; punctuation runs → operator), extend the concatenation and perf
corpora, and add a test asserting every palette yields distinct keyword/function/string
colours with `bg = Reset`.

### Phase 2 — Real languages instead of aliases

Split the aliases into their own families with their own keywords, comment markers and string
delimiters: `C/C++`, `Java`, `Kotlin`, `Go`, `Swift`, `C#`, `Css`, plus `Yaml` and
`Dockerfile` out of `Toml`, then `Markdown`, `Makefile`, `Xml/Html`, `Ruby`, `Php`, `Lua`.
A table-driven test asserts no two families share a keyword set, so an alias cannot creep
back unnoticed. One commit per family keeps each reviewable.

### Phase 3 — Richer prose

- `**bold**`, `*italic*`, `_italic_` — attributes only, no background.
- `- ` / `* ` / `1. ` lists, with continuation lines indented under the bullet.
- `> ` blockquotes: a dim rule plus dim text.
- `[text](url)`: accent text, dim underlined URL.
- `---` rules and `| a | b |` table alignment.
- Every transform must still reassemble to the source for copy.

### Phase 4 — Diff and patch rendering

Diffs are first-class output here but use three rules: emphasise the changed span *within* a
line, style `@@ … @@` hunks in accent and file headers bold, and render
`\ No newline at end of file` dim italic.

### Phase 5 — Configuration and discoverability

- `[ui] syntax = true|false` to turn highlighting off.
- `[ui] syntax_intensity = auto|dim|vivid`, derived from the palette.
- Verify new colours survive `color = c256|c16|mono` (`terminal_color` already downgrades RGB).
- Make `/theme` preview a real code sample, so a palette is judged on what it is used for.
- Document in README, and extend `/help` if needed.

## Verification for every phase

- `cargo test --workspace --no-fail-fast` (50 binaries, green today).
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`.
- Render real frames and read them: a probe over all seven palettes asserting token
  distinctness, plus `docs/previews` regeneration reviewed by eye.
- Each new behaviour gets a test that fails without it.