# Loon — architecture

Status: foundation milestone (M1) implemented; later milestones extend it.
This file records decisions and assumptions. Where the target spec (README "Requirement
matrix") is not yet met, that is stated here, not papered over.

## Crate ownership

| Crate | Owns | Does not own |
| --- | --- | --- |
| `loon-events` | Stable IDs, versioned event model, append-only journal, content-addressed blob store | Business logic, I/O beyond journal/blobs |
| `loon-config` | TOML schema, providers/models/**combos** definitions, precedence, validation, XDG path resolution | Applying config at runtime |
| `loon-workspace` | Workspace roots, path validation/sandbox checks, mutation transaction journal (CAS on file content hashes) | Tools themselves |
| `loon-tools` | Tool registry, versioned tool schemas, structural JSON-schema validation, file/exec/search tools, bounded output | Agent policy |
| `loon-providers` | Provider registry, OpenAI-compatible **Chat Completions** and **Responses** adapters, incremental SSE parser, capability table, **combo resolution** (fallback + round-robin), deterministic mock provider | Tool execution |
| `loon-core` | Agent state machine (understand→act→observe→verify loop), tool scheduler, steering queue, budgets/cancellation, turn orchestration | Terminal I/O |
| `loon-tui` | Transparent-mode ratatui UI, transcript rendering, input, status bar | Agent logic (consumes events) |
| `loon-cli` (`loon` binary) | Arg parsing, headless `run`, `sessions`, `config`, `doctor`, JSONL output, exit codes | Everything else |

Deliberately **not** created in M1 (no useful ownership boundary yet, created in their
milestone): `context`, `memory`, `rewind` (checkpoint store lives in `loon-events` for M1),
`mcp`, `plugins`, `evals`.

## Event/data flow

```
user input ─▶ loon-core Agent.run_turn()
                │  assembles messages + tool defs
                ▼
          loon-providers (combo resolver → adapter → SSE stream)
                │  ProviderEvent (deltas, tool calls, usage, finish)
                ▼
          loon-core validates complete tool calls → executes via loon-tools
                │  ToolResult
                ▼
          every state change → loon-events Journal (append-only JSONL)
                │                                        │
                ▼                                        ▼
        loon-tui event consumer (renders)          blobs/ (large outputs, sha256-addressed)
```

- The journal is authoritative; transcripts, TUI state, and (later) indexes are rebuildable
  projections. M1 journal = JSONL + sidecar blobs; SQLite/WAL migration is M2 (tracked in the
  requirement matrix, not silently deferred).
- IDs are UUIDv7 (time-ordered). Every event carries `seq`, causal `parent_id`, `ts_ms`,
  `schema_version`.
- Large tool output goes to `blobs/<sha256>`; events keep an excerpt + handle.

## Tool scheduling

Read-only tools may run concurrently; mutating tools run serialized inside a transaction that
records a pre/post content hash per touched file (CAS: an edit against a stale hash is
rejected, see `loon-workspace`). M1 keeps the scheduler simple: mutating ops take the
workspace lock; concurrency within a step is `join_all` over reads.

## Provider abstraction

- One internal event model; two wire adapters (Chat Completions, Responses) preserving
  per-protocol semantics (e.g. `finish_reason` vs `response.completed`, tool-call id/index
  reconstruction, usage in `stream_options` vs `response.completed`).
- SSE parsing is hand-rolled and incremental: byte-buffer accumulation → frame split on blank
  lines → only then UTF-8 decode, so fragmented UTF-8 across HTTP chunks is handled; per-event
  `id:` dedup; malformed frames surface as classified errors instead of being dropped silently.
- Tool-call arguments may stream incrementally for display; execution happens only after the
  protocol marks the call complete **and** the arguments parse and validate.
- Capabilities are a table per (provider, model) seeded from adapters + config overrides;
  `unknown ≠ supported` is enforced at combo-selection time.
- The mock provider is a real `Provider` implementation fed scripted events — used by tests
  and by `--provider mock`; it is never described as live support.

## Model-combo resolution

`Combo { name, mode: fallback|round_robin, members: [(provider_id, model_id, overrides)] }`.

- **fallback**: attempt members in order; switch on connection error, 5xx, rate-limit-after-
  budget, timeout, auth failure, pre-send capability/context mismatch. Adapter-classified
  model-level errors do **not** switch.
- **round_robin**: rotation over requests, cursor per session (persistent cursor is config),
  failure within an attempt follows fallback rules; rotation advances regardless.
- Hard rules enforced in code: no member switch after first token streamed to the user
  (unless opt-in retry-from-scratch); capability filtering pre-send; credentials never cross
  origins (each member resolves its own auth; redirects are pinned to origin); opaque
  provider state (response ids, reasoning items) is member-local and dropped on switch;
  per-attempt rewind points with causal ids; usage/cost attributed to the member that served.
- Every attempt is recorded with member, rotation index, skip/fail reason, and outcome.

## Context assembly (M1 scope)

System instructions → task guidance (AGENTS.md if present, read at turn start, cached until
mtime changes) → conversation history → tool definitions (stable order). Token counts are
heuristic estimates (`tokens ≈ chars/4`) and labeled as estimates; exact tokenizers are a
later milestone. Compaction is **not** in M1; the history model keeps every message discrete
so the compactor can be added without a schema break.

## Mutation transactions and checkpoints

Every mutating tool runs in a `Transaction`: pre-read hash → apply → post-hash journal entry.
`/undo`-class rewind needs the shadow checkpoint store (git2-based) — that is M2; M1 records
the journal entries that make it possible and the TUI surfaces the mutation trail. This is an
honest partial, listed in the matrix.

## Terminal rendering invariant

**Every application-owned cell uses the terminal default background (`Color::Reset`).**
`loon-tui::theme` is the only place styles are created; all backgrounds are `Reset`, selection
is foreground+underline (never reverse-video). A test walks every rendered buffer cell on every
screen and asserts `bg == Reset`. Opaque mode exists behind an explicit opt-in flag and is
covered by the same test with its palette.

## Decisions and assumptions

1. The repo was empty; this is a new project (user said "finish the luma project"; no `luma`
   project exists on disk, so this spec is treated as that project. Binary/config names follow
   the spec: `loon`, `~/.config/loon`, …). Renaming later is mechanical if wanted.
2. M1 journal is JSONL+sha256 blobs, not SQLite. JSONL is trivially replayable and the schema
   is versioned so migration to SQLite is additive.
3. Structural JSON-schema validation is implemented by hand (types/required/enum/items) —
   full `jsonschema` semantics is not claimed.
4. Token counting is an estimate until a real tokenizer lands; provider-reported usage is
   authoritative where present.
5. `AGENTS.md` handling: read-only inclusion of the workspace-root file in M1; scoped/nested
   guidance precedence comes with the context milestone.
