# Loon

A transparent, inspectable, terminal-native coding agent. Rust, one binary, terminal
default background everywhere so your compositor blur shows through.

**Status: Milestone 1 (foundation) implemented and tested.** The list below is exact
about what works and what does not; nothing here is aspirational phrasing.

## Quickstart

```sh
cargo build --release
# zero-config offline run against the built-in deterministic mock provider:
./target/release/loon run "Say hello" --output text
# interactive TUI (transparent mode is the default):
./target/release/loon
# point it at any OpenAI-compatible endpoint:
loon config init      # writes ~/.config/loon/config.toml (never overwrites)
$EDITOR ~/.config/loon/config.toml
loon run "Fix the failing parser tests" --model combo:daily-driver --output jsonl
```

Exit codes: `0` success · `1` task failed · `2` cancelled · `3` budget paused ·
`64` usage/config error. Machine output (`--output jsonl`) is versioned JSON on stdout;
diagnostics go to stderr.

Environment override `LOON_HOME=<dir>` nests all state under one directory (used by
tests; also handy for trying Loon without touching your real config).

## What works today (M1), verified

| Area | Status | Evidence |
| --- | --- | --- |
| Cargo workspace, 8 crates, builds clean (`clippy -D warnings`, `rustfmt`) | **implemented** | `cargo build` 0 warnings; `cargo fmt --check` clean |
| Test suite | **implemented** | 64 tests, 0 failures: `cargo test` |
| Event journal: UUIDv7 ids, causal parents, seq, JSONL append-only, sha256 blobs, torn-line recovery, malformed-line error | **implemented** | `crates/loon-events/tests/journal.rs` (6 tests incl. simulated crash) |
| Config: TOML schema, providers/models/**combos**, path-aware validation, comment-preserving save (`toml_edit`), XDG + `LOON_HOME` paths | **implemented** | `crates/loon-config/src/validate.rs` tests; `loon config path/validate/init` |
| Chat Completions adapter: streamed tool calls, fragmented arguments, usage in final chunk, `[DONE]`, non-stream fallback for endpoints that ignore `stream`, status classification (401/429/5xx) | **implemented** | `crates/loon-providers/tests/adapters.rs` over a raw-HTTP SSE fixture server |
| Responses adapter: typed stream events, `function_call_arguments.done`, usage in `response.completed`, Responses wire format (`input` items, flat function tools, `store:false`) | **implemented** | same fixture tests + wire-shape test |
| Incremental SSE parser: split frames, CRLF, comments, multi-line data, UTF-8 fragmented across chunks, invalid-UTF-8 frames skipped with a warning | **implemented** | `crates/loon-providers/src/sse.rs` unit tests + proptest over arbitrary bytes |
| Combos: fallback + round-robin, pre-send capability filtering (`unknown ≠ supported`), no switch after partial stream (unless opt-in), model-rejection does not switch, dropped-after-output = outcome-unknown, per-member credentials, usage attributed to serving member, visible skip trail | **implemented (resolution layer)** | `crates/loon-providers/tests/combos.rs` (11 tests, offline via mock) |
| Mock provider: real `Provider` implementation, scripted; default model works with zero config and no network | **implemented** | used across the whole test suite; `loon run` works offline |
| Tools: read (hash-bound, binary/UTF-8 refusal), write, edit (exact + single whitespace-normalized fallback, ambiguity refusal with line numbers), fuzzy_edit, multi_edit (all-or-nothing), glob, grep (gitignore-aware), tree, bash (filtered env, separate stdout/stderr, process-group kill on timeout/cancel) | **implemented** | unit tests + e2e turn test |
| CAS editing discipline: read hash → write refused on concurrent change, refusal journaled | **implemented** | `loon-workspace` tests + `stale_cas_write_is_refused…` e2e |
| Agent loop: step budget, cancellation at safe boundaries, steering queue, tool-argument validation, journaling of every request/tool/turn | **implemented** | `crates/loon-core/tests/e2e_turn.rs` (4 tests) |
| Transparent TUI invariant: every rendered cell bg = terminal default, no REVERSED, opaque only via explicit opt-in | **implemented** | `crates/loon-tui/tests/transparency.rs` renders full UI states on TestBackend and asserts every cell |
| Headless run, JSONL v1 event stream, documented exit codes, `--model combo:NAME` | **implemented** | smoke-tested via the binary; JSONL schema in `crates/loon-cli/src/main.rs` |
| `loon config path/validate/init`, `loon sessions list/export`, `loon doctor` (offline by default) | **implemented** | smoke-tested; doctor makes no network calls unless `--connect` |
| Arbitrary OpenAI-compatible base URLs as first-class providers; endpoint without `/models` works with manual models (Loon never requires `/models`) | **implemented** | config schema + registry tests |
| Token counting | **partial** — heuristic (chars/4) only, labeled as estimate; provider-reported usage replaces it on request records | `crates/loon-core/src/est.rs` |

## Milestone status (evidence-backed, no silent deferral)

- **M1 foundation — implemented.** 8-crate workspace, agent loop, toolset, provider
  adapters (Chat + Responses over a proptest-fuzzed SSE parser), combos, mock provider,
  headless CLI with documented exit codes, transparent TUI verified on `TestBackend`.
- **M2 reliability — implemented.** SQLite/WAL store with migrations + legacy JSONL
  fallback (`loon-events`); CAS rewind/undo/redo/fork/checkpoints with conflict
  preservation (`loon-core/tests/rewind.rs`, 6 tests); permission engine wired into the
  tool loop — deny rules bind every mode, allowlist is exact + component-prefix, ask
  gates via an approval hook and refuses headless, redacted audit JSONL
  (`crates/loon-core/tests/permissions.rs`, 5 tests); real `kill -9` crash-recovery test
  (`crates/loon-cli/tests/crash_recovery.rs`). Not done: git2 shadow repository.
- **M3 context — core implemented.** Hierarchical compaction with replay-identical
  summaries (`tests/compaction.rs` — replay reproduces live history exactly); persistent
  memory (global + workspace scopes, `memory` tool, prompt loading, `tests/memory.rs`);
  verification loop with baseline/Introduced/PreExisting/FlakySuspected classification
  (`tests/verify_shell.rs`); incremental repo index with persisted mtime cache
  (`--lib index`). Not done: LSP integration.
- **M4 UX — core implemented.** Slash commands `/help /undo /redo /rewind /checkpoint
  /fork /compact /context /diff /memory /model /quit` execute real machinery and are
  tested through the agent (`tests/slash_commands.rs`); subagent `delegate` tool with
  journaled child sessions, no recursion, cancellation propagation
  (`tests/subagent.rs`); mid-session model switching. Not done: `/config` TUI, prompt
  queue UI, skills/workflows, model routing rules.
- **M5 integrations — not implemented.** MCP client, plugin protocol,
  files/batch/embeddings APIs, monitors, SSH/container workspaces, supervisor/attach,
  browser/DB/binary/RE tools. The adapter seams exist (registry, tool registry) but no
  MCP code is written and none is claimed.
- **M6 hardening — partially done.** Crash/kill coverage and adversarial provider cases
  (partial streams, drops, malformed frames) are tested; measured benchmarks against the
  spec targets have **not been run** and no numbers are claimed; packaging and the
  portability matrix are not done.

Interactive TUI note: rendering, layout, and state transitions are verified on
`TestBackend`; key-handling paths are unit-tested where practical, but a full interactive
keymap session was **not** exercised in this environment and is not claimed.

## Compatibility matrix

| Capability | Live-tested | Fixture-tested | Not yet |
| --- | --- | --- | --- |
| OpenAI Chat Completions streaming | — | ✔ | live (no credentials in this environment) |
| OpenAI Responses streaming | — | ✔ | live |
| Arbitrary OpenAI-compatible endpoints | — | ✔ (non-SSE fallback, error statuses) | live |
| Anthropic Messages / Gemini | — | — | ✔ (adapter interface reserved, no implementation) |
| MCP | — | — | ✔ (not started) |

## Layout

```
crates/
  loon-events/      stable ids, event model, journal, blobs
  loon-config/      TOML schema, validation, XDG paths
  loon-workspace/   sandbox checks, CAS mutation records
  loon-tools/       tool registry + read/write/edit/glob/grep/tree/bash
  loon-providers/   SSE parser, Chat/Responses adapters, registry, combos, mock
  loon-core/        agent loop, steering, budgets, cancellation
  loon-tui/         transparent-mode ratatui UI
  loon-cli/         the `loon` binary
```

`ARCHITECTURE.md` records the design decisions and the two intentional M1 simplifications
(JSONL before SQLite; structural-not-full JSON Schema).

## Verification commands

```sh
cargo build                 # zero warnings
cargo test                  # 118 tests, all suites green
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
LOON_HOME=$(mktemp -d) target/debug/loon run "smoke" --output jsonl
```

## License

MIT — see `LICENSE`.
