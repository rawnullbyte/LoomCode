# Changelog

All notable changes to LoomCode. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project intends to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

59 commits since `v0.1.0`. Grouped below by user-visible effect.

### Fixed

Correctness defects found and repaired during a full audit pass. Each carries a
regression test that fails without the fix.

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

### Changed

- CI and docs runners pinned to `ubuntu-24.04`; GitHub Actions bumped to node24 runtimes.
- Review repair policy and the unverified-goal hold are documented.
- Central one-shot request factory; supervisor and compaction route through it.

## [0.1.0] — 2026-10-22

Initial tagged release: TUI, headless `run`/`resume`, sessions with lazy creation,
provider/registry with combos, the tool set, the permission gate, journal and rewind,
Ultra Loop, hooks, MCP, memory, compaction, the docs site and the Linux release workflow.

[Unreleased]: https://github.com/rawnullbyte/LoomCode/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rawnullbyte/LoomCode/releases/tag/v0.1.0
