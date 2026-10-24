# In-process language intelligence — plan ("LSP without installing anything")

Goal: real symbol navigation and syntax diagnostics, computed inside the binary. No
language servers to install, no subprocess protocol, no per-machine setup. If a language has
a Rust parser crate, support follows from a dependency.

## What exists today (verified)

| Piece | Where | State |
| --- | --- | --- |
| `find_symbol` | `loon-tools/src/inspect.rs` (`Kind::Symbol`) | Regex heuristic: `\bsymbol\b` plus a declaration keyword, line numbers only. Returns `"heuristic": true` — it says so itself. |
| `RepoIndex` | `loon-core/src/index.rs` | File list (path, size, mtime). No parsing. |
| Highlighting | `loon-tui/src/code.rs` | 24 language families for colour only; no structure. |
| `syn` | `Cargo.lock` | 2.0.119, already present transitively. A probe parsed Rust (struct/enum/trait/impl/fn/mod) and surfaced a syntax error with position — feasibility confirmed. |

## Why in-process

An external LSP needs the right binary on the user's machine, a version that matches their
toolchain, a spawn/stdio protocol and a health check. That is four failure modes before any
symbol is found, and it is exactly the kind of setup tax this tool avoids elsewhere (SSH and
shells need only `ssh`). A Rust parser crate is one dependency: it parses with the binary, is
versioned by the lockfile, and cannot be missing at runtime.

Honest limits, stated up front: in-process parsing gives syntax-level understanding
(definitions, kinds, positions, syntax errors), not name resolution or type inference. It
cannot tell you which `Foo::new` a bare `new()` calls. That is still a large step past regex.

## Phases

### Phase 1 — Rust: a real symbol table (`syn`, already in the tree)
New `loon-tools/src/lsp.rs`:
- `pub struct Symbol { name, kind: SymbolKind, line, column, end_line, visibility, doc }`
- `pub fn rust_symbols(src: &str) -> Vec<Symbol>` walking `syn::File`: fn, struct, enum,
  union, trait, impl (as `impl Path`), mod, type alias, const/static, macro_rules.
- Doc comments attached to their item.
- `find_symbol` upgraded: when `syn` parses the file, return `definition` vs `reference`
  (references still found by word scan), kind, column and visibility — and drop the
  `"heuristic"` flag for parsed files.
- Fail-open: a file that does not parse falls back to today's behaviour. A syntax error is a
  finding, not a crash.

### Phase 2 — Syntax diagnostics
- `pub fn rust_diagnostics(src: &str) -> Vec<Diagnostic { line, column, message }>` from
  `syn::parse_str`'s error (it already reports a span; render as line:col).
- New `diagnostics` tool: path (or file list) → syntax errors per file, empty array when
  clean. The agent can run it after an edit instead of guessing whether it broke the file.
- `loon-core` index stays as-is; diagnostics are per-call, so there is no stale-index
  problem to manage.

### Phase 3 — More languages, each a dependency
| Language | Crate (pure Rust) | Gives | Status |
| --- | --- | --- | --- |
| Python | `rustpython-parser` | symbols + syntax errors | **done** |
| JSON | `serde_json` (already present) | syntax errors | **done** |
| TOML | `toml` (already present) | syntax errors | **done** |
| JS/TS | `oxc_parser` | symbols + syntax errors | later |
| YAML | `serde_yaml` | syntax errors | later |

Each is a `match` arm in one dispatcher: `fn symbols(lang: Lang, src: &str)`. Languages
without a dependency keep the regex path and say so.

### Phase 4 — Subagent management gaps (independent of parsing)
Verified against `subagent.rs`: jobs have no start time, so nothing can show how long a
child has run or whether it is stalled; the TUI shows only transcript lines, so a parent
spawning three children gives the user no single view of what is running.
- Add `started: Instant` to `Job`; `background_list` reports per-job elapsed.
- New TUI subagent panel: one row per running child (id, label, elapsed, status) above the
  prompt bar, replacing the stream of update lines.

## Verification

- Tests per language: a sample file yields its expected symbols (name, kind, line, col); a
  syntax error yields a diagnostic with position; a broken file falls back without panicking.
- The concatenation/perf invariants are untouched — this work is in `loon-tools`, not the
  renderer.
- `find_symbol` keeps its existing output shape (additive fields only), so nothing that
  consumes it breaks.

## Non-goals

- Cross-file name resolution, type inference, completions or "go to definition" jumps in the
  TUI. The tool output gives the model positions it can act on with `read`; a TUI jump is a
  separate future decision.
- Replacing the regex path. It remains the fallback for unparsed languages and broken files.
