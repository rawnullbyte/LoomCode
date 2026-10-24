# Loom — Audit findings and feature backlog

Status: working document. Bugs listed here are confirmed by reading the code and, where
noted, by reproducing the behaviour. Anything unproven is marked as a hypothesis and kept
out of the "fix" list.

---

## Part 1 — Bugs found

### UI-1 · Tool-group collapse hides the output of every call but the first
**Severity:** High
**Location:** `crates/loon-tui/src/transcript.rs:709-733` (`push_calls`)
**Observed:** Rendering `read` then `bash` in one group prints

```
  ✓ read 1 file · ran 1 shell command
    ⎿  2 lines
       Ctrl+O expands tool output
```

The `bash` result (`12 passed · 0 failed`) never appears. Two defects in one:

1. `shown` takes `calls.iter().take(1)` and `call_summary` already counted *every* call in
   the head ("ran 1 shell command"), so the summary claims work the body does not show.
2. `tool_detail` falls through to `format!("{} lines", many.len())` because the first call's
   two-line excerpt yields 2 content lines. For a 2-3 line command result the count is
   strictly less informative than the text itself, and it is the *only* thing shown.

**Trigger:** Any turn where the model makes two or more tool calls in a row — the common case.
**Why it matters:** the transcript is the product. A user cannot see that their tests passed.
**Fix:** show a short excerpt per call (first content line) when the group is small, cap the
group, keep the full text behind Ctrl+O; use the line count only above a threshold where it
is genuinely shorter.

### UI-2 · Ctrl+O works, but the collapsed view does not say what it hides
**Severity:** Medium (corrected — my first reading was wrong)
**Location:** `crates/loon-tui/src/transcript.rs:877-896` (expansion), `:752-759` (hint)
**Verified:** rendering the same group with `show_tool_details = true` does print both calls
with full excerpts. The affordance is real.
**Remaining defect:** the hint is a bare "Ctrl+O expands tool output". It never says *what* is
hidden, so a user who just watched their test run get swallowed has no reason to press it.
**Fix:** put the hidden-call count in the hint ("Ctrl+O shows 1 more result") and show one
excerpt line per call inline, so the feature is discoverable by seeing its value.

### UI-3 · Queued prompts collide with the assistant block above them
**Severity:** Medium
**Location:** `crates/loon-tui/src/ui.rs:51-58` (layout) / `:113-129` (queue rows)
**Observed** at 46x14 and 30x9:
```
● The parser now preserves empty fields.
  Focused tests pass.
↳ queued prompt 0
```
No separator, and at 30 columns the text is truncated by the edge of the frame instead of
wrapping — queued rows are built as one unwrapped `Line`.
**Fix:** one leading blank/dim separator row and truncate queue rows to the width with an
ellipsis (they are a preview, not content).

### UI-4 · Long assistant text is hard-wrapped but `Code`/inline spans ignore the width budget in the tool detail path
**Severity:** Low
**Location:** `crates/loon-tui/src/transcript.rs:748` (`push_colored_excerpt`), `:719-730`
**Observed:** `Ctrl+O expands tool output` is pushed at a fixed 7-space indent and is clipped
rather than moved to the next row at narrow widths.
**Status:** cosmetic; fold into UI-1's rewrite.

### UI-5 · Status bar drops the queue indicator below 55 columns with no alternative
**Severity:** Low
**Location:** `crates/loon-tui/src/ui.rs:417-432`
**Observed:** at 46x14 the whole right-hand block (`~8.64k / 128k · medium · 2 next`) is gone,
including the queue depth. Below 55 the bar renders only `● Ready`.
**Fix:** the compact bar should keep the most actionable signal (busy/idle + queued count),
not just the activity word.

### BUG-1 · SSE: one CRLF line ending split every frame
**Severity:** High · **CONFIRMED (reproduced)**
**Location:** `crates/loon-providers/src/sse.rs:105` (`find_frame_end`)
`\r\n` is two bytes, so the terminator counter treated one line ending as a blank line.
Every CRLF stream was cut into one frame per field: `data: line1\r\ndata: line2\r\n\r\n`
yielded `["line1", "line2"]` instead of one frame with `"line1\nline2"`, and a split
multi-line JSON payload then failed to parse downstream.
**Fixed:** count line endings (`\r\n` = 1), added 4 regressions.

### BUG-2 · SSE buffer grows without bound
**Severity:** Medium · **CONFIRMED (code)** 
**Location:** `crates/loon-providers/src/sse.rs:43`
A stream that never sends a blank line grew `buf` forever; the idle timeout never fires
while bytes keep arriving.
**Fixed:** `MAX_FRAME_BYTES` cap + `FeedError::FrameTooLarge`, surfaced as a loud error.

### BUG-3 · Responses adapter lost error classification
**Severity:** High · **CONFIRMED (reproduced)**
**Location:** `crates/loon-providers/src/responses.rs:247`
Every `{"type":"error"}` event became `Rejected` (neither switchable nor retriable), so a
streamed 429 killed the turn instead of letting a combo back off or rotate.
**Fixed:** use `ProviderError::from_error_value`, same as the Chat adapter.

### BUG-4 · Negative tool-call index split a call in two
**Severity:** High · **CONFIRMED (reproduced)**
**Location:** `crates/loon-providers/src/chat.rs:159`
`{"index":-5}` created key `-5` for the name; arguments arriving under `index: 0` landed in
a different entry, surfacing an executable call with `{}` arguments — silently wrong.
**Fixed:** reject negative indices (clamp to "continue current call").

### BUG-5 · `args_delta` always empty for Chat tool calls
**Severity:** Low · **CONFIRMED (reproduced)**
**Location:** `crates/loon-providers/src/chat.rs:204`
Fragments were accumulated but the event carried `String::new()`, so streamed progress
counted zero characters for every Chat tool call.
**Fixed:** send the accumulated fragment.

### BUG-6 · Editing a file silently dropped its mode
**Severity:** High · **CONFIRMED (reproduced)**
**Location:** `crates/loon-workspace/src/lib.rs:280` (`write_atomic`)
`File::create` applies 0666 & ~umask, and the rename replaced the target's permissions: a
0755 script became 0644 after one edit and stopped being executable. Verified 755 → 644.
**Fixed:** copy the existing mode onto the temp file before the rename.

### BUG-7 · SSH host argument allowed option injection
**Severity:** Critical (arbitrary command execution) · **CONFIRMED (reproduced)**
**Location:** `crates/loon-tools/src/ssh.rs:171` and `:295`
The host was passed as a bare argv element, so a host beginning with `-` was parsed by ssh
as an *option*: `-oProxyCommand=touch /tmp/pwned` executed locally while connecting —
before any permission decision. Verified the injected command ran.
**Fixed:** `--` before the target in both call sites, plus explicit validation of `host`
and `user` at the input boundary.

### BUG-8 · `parse_reviewer_verdict` underflow (defensive)
**Severity:** Low · **Location:** `crates/loon-core/src/supervisor.rs:102`
`text.len() - 1` is only safe because the loop cannot run on an empty buffer. Replaced with
`saturating_sub` so a future edit cannot turn it into a panic.

### Hypothesis (NOT confirmed, not fixed)
- `crates/loon-providers/src/http.rs:330` `base.truncate(base.len() - suffix.len())` — needs a
  check that `base` ends with `suffix`; reading left as an exercise if the audit agent
  confirms. Not reported as a bug until there is a trigger.

---

## Part 2 — Feature backlog, by priority

### P0 — trust and correctness of the transcript (do first)
1. **Tool output is visible in groups** (fixes UI-1). One excerpt line per call, capped.
2. **Ctrl+O actually expands tool output** (fixes UI-2), per-call and per-group.
3. **Collapsed group says how much is hidden** — "… 3 more lines, Ctrl+O" instead of a bare count.
4. **Failed calls always show their first error line**, even inside a group of 5+.

### P1 — prompt-bar ergonomics (the surface typed into all day)
5. **Persistent prompt history** — the in-memory history from `47e3fbc` is per-process;
   persist it to `$LOON_HOME/state/history` so recall survives a restart, with a size cap and
   dedupe against the last entry.
6. **Ctrl+R reverse search** over prompt history, incremental, Esc cancels and restores the draft.
7. **History prefix filter** — Up after typing "cargo" only walks entries starting with it.
8. **Draft autosave** — the current prompt survives a crash/restart and is restored on launch.
9. **Multi-line paste safety** — a paste containing newlines must offer "send" vs "keep as draft"
   rather than silently either.

### P2 — session and workspace management
10. **`/resume --all` escape hatch** — the path scoping from `fb6f81f` is right, but there is no
    documented way to reach another directory's session; add an explicit flag that names the
    directory and refuses to switch workspace silently.
11. **Session titles** — derive a stable title from the first prompt and show it in the status
    bar, so two sessions are distinguishable at a glance.
12. **`/search` across the current session's transcript** with jump-to-match.
13. **Per-workspace recents in the picker** — group by date (today / this week / older).

### P3 — diagnostics and polish
14. **`/doctor` in the TUI** — currently CLI-only; surface the same checks as a menu page.
15. **Token budget warning** — a dim hint when the next request will cross the autocompact
    threshold, before it happens rather than after.
16. **Elapsed-time honesty** — a turn that has not received a token in 30 s says "waiting on
    provider" instead of showing a spinner that implies progress.
17. **Copy affordance** — the selection highlight exists, but there is no visible confirmation
    that a copy happened; flash a one-line "copied N lines".
18. **Error-line colouring** — failed tool output is all one colour; the first line should be
    the error, the rest context.
19. **Compact status bar keeps queue depth** (fixes UI-5).
20. **Queue separator + truncation** (fixes UI-3).
21. **`/help` grouped by task**, not a flat 40-row list.
22. **Fuzzy `/` completion ranking** — exact prefix first, then substring, then description match.
