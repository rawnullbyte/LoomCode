# Changelog

All notable changes to LoomCode. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project intends to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.2.0] — 2026-10-27

Additive: four new subsystems, all opt-in and backward compatible. Every existing shortcut,
configuration file and session remains valid.

### Added — configurable keybindings, command palette, execution modes, role routing

- **Configurable keybindings** (`[keys]`, `/keybindings`, `/keybind`). Every command key resolves
  through one map (`crates/loon-config/src/keymap.rs`) with context scopes, so `esc` stops a turn in
  the prompt and dismisses a dialog in a list without ambiguity. Rebinding, unbinding, per-action
  and whole-profile reset, `default`/`emacs`/`vim` profiles, export, conflict detection and refusal
  of chords the terminal owns (`ctrl+z`, `ctrl+\`, `ctrl+s`). Changes apply immediately with no
  restart, and `[keys]` is validated by `loom config validate`. Taking a chord from another action
  names the displaced action rather than shadowing it silently.
- **Command palette (Ctrl+P).** Subsequence search over every slash command, configured model,
  saved session, theme, setting, execution mode and bound shortcut. Accepting an entry submits the
  real command, so the palette is a dispatcher over the existing registry rather than a second one.
- **Execution modes** (`/mode`, plus `ultrawork`/`ulw`/`ultracode`/`deepresearch` keywords). A mode
  derives the switches it implies: `plan`/`review`/`security` enforce read-only through the existing
  mutation gate, `ultrawork` enables the completion supervisor, `ultracode` raises effort. Durable
  across `/resume`; shown in the status bar. Implicit keyword routing is restricted to unambiguous
  keywords so `review the parser` stays a task.
- **Agent roles and category-aware model routing.** Fourteen built-in roles (orchestrator, planner,
  explorer, librarian, architect, implementer, frontend, backend, debugger, tester, reviewer,
  security, documentation, release), overridable by `.loon/agents/*.md`. `delegate`,
  `background_spawn` and `task_fanout` accept a role, a category and an explicit model; resolution
  is role → category (`[routing.models]`) → session, each candidate validated, with the chosen model
  and reason reported in the result. Read-only roles have mutating tools removed from the child.
- **`[routing.effort]`** per-category effort for delegated work.
- **Slash commands now work headlessly.** `loom run "/mode plan"` executes the command through the
  same parser the TUI uses (exit 64 on a usage error) instead of sending the text to the model, so
  every management command is reachable from the shell and in `--output json`/`jsonl`.
- **Three new real-widget previews** on the docs site: the command palette, the execution-mode
  picker and the keybinding editor.

### Fixed

- **Slash commands were unreachable from the shell.** `loom run "/agents"` sent the text to the
  model and printed its guess, because the headless path called `run_turn` directly.
- **`delegate` silently discarded model routing.** `refresh_delegate` resolved the routing tables
  and handed them to `background_spawn`, then rebuilt `delegate`'s spec from scratch, so category
  routing reached one tool and not the other. Both now share the resolved spec.
- **A built-in role id resolved to no role**, because the lookup was keyed off the profile it
  found: naming `reviewer` without a profile file kept the default category and dropped the
  role's instructions.
- **`/keybind` stealing a chord reported only "(saved)".** It now names the displaced action and
  how to restore it.
- **Ctrl+Q and Shift+Tab were dropped** by the keybinding refactor (`BackTab` never reached the
  map). Both restored, with the modal quit path pinned by a test.
- **CHANGELOG sections had drifted** from the tags: the `0.1.1` release shipped without a section
  of its own, and entries accumulated under `Unreleased` across two tags. The history below is now
  attributed to the version that actually shipped it.

## [0.1.1] — 2026-10-25

Fixes and additions accumulated between the first two tags. Grouped below by user-visible effect;
the `### Fixed` audit entries date from the first 60 commits of the project, entries added since:

- **TUI lag on long sessions.** Frame cost grew with session length (24 ms/frame at
  2 000 transcript blocks, over 200 ms at 20 000, against a 50 ms animation tick). Rows
  are now cached and only the visible window is rendered; cost is flat.
- **Test isolation fix:** the persistent-shell tests no longer share one process-wide tool
  registry, which made the `Tests` CI stage fail intermittently.
- **SSH host option injection:** a host beginning with `-` was parsed by ssh as an option,
  so `-oProxyCommand=` ran a local command while connecting. `connect` now rejects a
  leading-`-` host or user, and both call sites pass `--` before the target.
- **Editing a file silently dropped its mode:** a 0755 script became 0644 after one edit
  and stopped being executable. The existing mode is now copied onto the replacement.
- **`/new` and `/resume` dropped the background hub**, stranding running children.
  Both now go through `replace_with`, which carries the hub (and `loops`) across.
- **Fail-open reviewer verdict parser:** substring counting let `DISALLOW` match `ALLOW`.
- **Shadow-restore path** hardening, alongside the other audit fixes below.

### Fixed

Correctness defects found and repaired during a full audit pass. Each carries a
regression test that fails without the fix.

- **Slash commands were unreachable from the shell.** `loom run "/agents"` sent the text to the
  model and printed its guess, because the headless path called `run_turn` directly. Commands now
  execute through the same parser the TUI uses (exit 64 on a usage error), so `/mode`, `/agents`,
  `/keybind` and every other command work headlessly and in `--output json`.
- **`delegate` silently discarded model routing.** `refresh_delegate` resolved the routing tables
  and handed them to `background_spawn`, but rebuilt `delegate`'s spec from scratch, so category
  routing reached one tool and not the other. Both now share the resolved spec
  (`delegate_tool_with_spec`).
- **A built-in role id resolved to no role.** The role lookup was keyed off the *profile* it found,
  so naming `reviewer` with no profile file kept the default category and dropped the role's
  instructions. Lookup is now by the name the caller gave.

- **Credential redaction could leak or panic on non-ASCII input.** `redact()` indexed the
  lowercased copy of a string with byte offsets from the original. Lowercasing can change
  byte length (`İ` is 2 bytes, its lowercase form is 3), so the offsets desynchronised —
  either panicking on a non-boundary slice, or silently skipping the key and writing the
  secret to the audit journal in plaintext. Redaction now compares in place with
  `eq_ignore_ascii_case` and a char-boundary check.
- **Environment-shaped credentials were not redacted.** The boundary rule required a key
  to start a token, so `OPENAI_API_KEY=`, `GITHUB_TOKEN=` and `DB_PASSWORD=` were
  journaled verbatim — the exact shapes credentials arrive in. `_` now counts as a
  separator; prose and path false positives remain untouched.
- **A database written by a newer build opened silently.** `migrate()` computed the
  downgrade guard and then discarded it, so `SchemaTooNew` was declared but never
  constructed. Opening a newer database now refuses instead of risking corruption.
- **A trailing tab did not advance argument completion.** `/config tips<TAB>` kept
  suggesting keys instead of the completed key's values; only a literal space counted as
  "next argument".
- **Approval prompts could be answered by a modified key.** The ask-mode modal matched
  `key.code` without checking modifiers, so Ctrl+Y (copy-last-answer) approved the pending
  tool call. The modal now runs before the shortcuts and requires an unmodified y/n/Esc;
  all other keys keep the prompt waiting.
- **DCS/SOS/PM/APC payloads were painted into the transcript.** `clean()` stripped CSI and
  OSC but not the string sequences (`ESC P/X/^/_`), so sixel or terminal-query payloads
  rendered as if the command had printed them. Those sequences are now consumed to ST/BEL.
- **OSC 52 clipboard fallback emitted invalid base64.** Table bytes were formatted as
  decimal (`"f"` → `901036161` instead of `Zg==`), corrupting every copy on ssh/headless —
  the exact path OSC 52 exists for. Now encoded correctly and covered by RFC 4648 vectors.
- **Completing a slash command could panic on a multi-byte separator.** The cut assumed a
  one-byte whitespace; an ideographic space (U+3000) pasted from a CJK input method left
  the cut mid-character and `truncate` panicked mid-edit.
- **`/loop` panicked on a multi-byte interval unit.** `/loop 5µ …` split the string at
  `len - 1`, slicing a character in half. Units are now split by character.
- **Session delete demanded exactly 8 characters** while the prompt asked for a prefix, so
  pasting the full id — which is a prefix — aborted.
- **`sessions list` misaligned the TITLE column** because the cwd column was padded to a
  different width than `short_path` produced.
- **Diff rendering:** each stored diff line renders as its own row, and output is only
  called a diff when it actually carries a hunk header.
- **Session titles are flattened to one line** on every surface (list, delete prompt,
  `/sessions` search, `/find`), so a stored newline cannot forge extra rows.
- **Rewind, journal and worklog hardening:** unknown-session errors, a journal write
  crash, and orphaned worklog entries.
- **Supervisor refusal detection widened** to phrasings that decline verification without
  using the word "verify" ("stop reviewing", "verification is optional", …), while keeping
  plain progress reports and process objects ("do not stop the server") as non-refusals.

### Added

- **Sessions are ordered by last use**, with a `LAST USED` column; resume picks the most
  recent first.
- **Resume, export and delete accept the same session-id prefixes**, with one shared
  selector path and honest errors for empty or ambiguous input.
- **Syntax highlighting in the transcript**, including source files read by tools, not only
  fenced code blocks.
- **Diffs from edit tools are shown in the transcript.**
- **Prompt editing:** word-jump (Ctrl+Left/Right), Shift selection, and multi-line
  click positioning.
- **Lazy session creation** — no session is persisted until the first user message, so an
  empty session can never appear.
- **`--output json`** emits one document (previously aliased `jsonl`); the correspondence
  between the two modes is pinned by a test.
- **`loom doctor` and `loom config`** are documented subcommands; the dead `--opaque` flag
  was dropped.
- **Ultra Loop review repair:** a malformed review is repaired rather than failing the
  turn, and an unrepairable one raises a durable hold instead of completing.
- **Deterministic mock `Delay` step** so cancellation is observable end to end.
- **README updated** for the new flags and commands (json output, doctor, config,
  sessions delete, highlighting, diff rendering).

Added since the first 60 commits:

- **`find_symbol` and `diagnostics` parse Rust and Python in-process** (`syn`,
  `rustpython-parser`; both already in the dependency tree, pinned by the lockfile). Real
  definitions with kind, column, end line, visibility, doc comment and container, instead
  of a regex that matched any word next to a declaration keyword. No toolchain needed.
- **Background subagents:** a concurrent-children cap (`DEFAULT_MAX_RUNNING = 8`, checked
  under the id-assigning lock), per-job elapsed time frozen at completion, jobs named by
  their task, and child sessions excluded from `/resume`.
- **Persistent prompt history** in the state directory (bounded at 500 entries, adjacent
  duplicates skipped, a corrupt line never fails the session), with Ctrl+R reverse search
  that restores the draft on Esc.
- **Up/Down recall filters by the typed prefix**; a prefix that matches nothing falls back
  to the whole history rather than making Up a dead key.
- **Per-language syntax highlighting** — each real language has its own rules instead of
  aliasing to JavaScript — plus function/type/attribute/punctuation styles, prose structure
  and inline emphasis, emphasised changed spans in diffs, and `[ui] syntax = false`.
- **`/help` grouped by task** instead of one flat list of 52 rows.
- **`/resume` and `/delete` scoped to the current directory.**
- **Queued messages preview** as dim rows above the prompt bar (capped at three).

### Changed

- CI and docs runners pinned to `ubuntu-24.04`; GitHub Actions bumped to node24 runtimes.
- Review repair policy and the unverified-goal hold are documented.
- Central one-shot request factory; supervisor and compaction route through it.

## [0.1.0] — 2026-10-22

Initial tagged release: TUI, headless `run`/`resume`, sessions with lazy creation,
provider/registry with combos, the tool set, the permission gate, journal and rewind,
Ultra Loop, hooks, MCP, memory, compaction, the docs site and the Linux release workflow.

[Unreleased]: https://github.com/rawnullbyte/LoomCode/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/rawnullbyte/LoomCode/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/rawnullbyte/LoomCode/releases/tag/v0.1.1
[0.1.0]: https://github.com/rawnullbyte/LoomCode/releases/tag/v0.1.0
