# Loom agent harness

The runtime owns tools, permissions, history, durable state and supervision. Both the terminal UI and JSONL CLI consume the same events. No comparative performance claim against other agents is implied; use your models and representative tasks to evaluate it.

## Everyday controls

| Control | Behavior |
| --- | --- |
| `/ultracode TASK` | High-effort implementation workflow with independent completion review |
| `/ultraloop on`, `off`, `status` | Keep the main agent working when the supervisor identifies unfinished work; interrupt with Escape |
| `/ultraloop limit 0` | Unlimited completion reviews; positive values impose a review budget |
| `/ultraloop model REF` | Choose a separate configured supervisor model |
| `/plan on`, `off`, `status` | Restrict the session to inspection/planning until you explicitly leave plan mode |
| `/tasks add TEXT` | Add a durable task |
| `/tasks get ID`, `start ID`, `done ID`, `block ID`, `remove ID` | Inspect and update tasks |
| `/autocompact 50000` or `/autocompact 50k` | Save a token threshold for automatic context compaction |
| `/autocompact auto` | Return to the default threshold, 80% of model capacity |
| `/autocompact off` | Disable automatic compaction |
| `/context` | Inspect the estimated context size and effective compaction trigger |
| `/agents` | List configured subagent profiles |
| `/skills`, `/tools` | Inspect workflows and the actual tool catalog |
| `/mcp add local 3000` | Connect a local Streamable HTTP MCP server at `http://127.0.0.1:3000/mcp` |
| `/mcp add remote https://host.example/custom` | Connect an explicit HTTP MCP endpoint |
| `/mcp add local-command python3 server.py` | Connect a stdio MCP server |

Explicit compaction thresholds are capped at 80% of the configured context capacity to retain room for the next response. Context size is estimated. A single request-level `t/s` meter counts thinking, answer text and tool arguments together. `~` means a character-based estimate; provider-reported total output/completion tokens replace it when available. Reasoning subtotals are not added again. The request timer includes initial latency, resets on retries/new requests and freezes before tools execute. Summary-only reasoning streams can underestimate the rate until final usage arrives.

Compaction summarizes through the serving model in requests that each fit that model's own context window: the transcript is chunked to half the window and the partial summaries are folded into one handoff. A summarization that produces no text reports its actual cause (provider error, reasoning-only answer, empty completion) instead of a generic empty summary, keeps history unchanged, and pauses automatic retries for 60 seconds so a broken summarizer cannot stall every request. `/compact` retries immediately regardless of that pause.

Settings remain open as values change. Left/Right cycles choices and adjusts numbers without opening an editor. Enter opens text editing or a specialized picker. Theme navigation previews immediately with arrows and the wheel: Enter saves, Escape restores the original palette. A picker opened from Settings returns there. Popups have one row per item, a separate description pane, visible selection and paging.

Rewind selects a user message, removes it and later chat from the visible transcript, and places its exact original text in the draft. Multiline text is retained. Tool rounds are grouped under their user message. New work after a rewind survives resume. Workspace rewinds check hashes and retain conflicting manual changes. Shell side effects do not have file snapshots and cannot be automatically rolled back.

## Tools, tasks and supervision

Native tool schemas stay available. MCP schemas load through `tool_search`, keeping large remote catalogs out of ordinary requests. Search matches names and descriptions; an empty query lists metadata, and a nonempty query loads matching definitions. Entirely denied tools are omitted from provider schemas and search. Every actual call still passes runtime validation and permissions, including invented calls to hidden tools.

The prompt requires `todo_read` then `todo_write` before multi-step work, with every requested requirement and verification step recorded. `todo_write` replaces the same ordered checklist as `update_plan`, validates statuses and permits at most one in-progress item. `todo_read` also returns task descriptions and notes. The agent is instructed to update progress and use `note` for findings, changed paths, decisions, failed attempts, evidence, blockers and next actions. This working record survives compaction and resume.

The `task` tool supports `create`, `update`, `get`, `list` and `remove`. It accepts descriptions and `blocked_by` task IDs, rejects dependency cycles and rejects starting/completing tasks before dependencies finish. Tasks, the ordered plan, notes, goal and active skills are stored outside the lossy conversation summary. The supervisor cannot accept completion with unfinished plan steps or tasks. Cancellation and explicit denials remain effective in ultraloop.

Plan mode excludes workspace-mutating tools from provider requests and rejects attempted mutations at execution. Session bookkeeping (`task`, `note`, `update_plan`, `todo_write`) remains available. Bash and remote mutating tools are blocked in this mode. Only the user's `/plan off` command exits it; there is no model-accessible bypass.

Managed processes use pipe output, bounded polling and process-group cleanup on Unix. Cleanup tolerates processes that have already exited, and never runs a noisy external `kill` command. Quitting or aborting the driver also aborts its active operation.

## Built-in coding tools and documentation

The full host exposes more than 40 native tools before MCP additions. Repository tools include `project_info`, `list_directory`, `file_info`, `read_many`, `find_symbol` and `file_compare`. Symbol discovery uses bounded declaration matching rather than a language server. `read_json`/`json_edit` use JSON pointers; `notebook_read`/`notebook_edit` inspect and edit cells, preserve unrelated metadata and clear stale code outputs on replacement. Edits reuse hash checks and undo snapshots.

Six Git inspection tools cover status, diffs, history, revisions, blame and branches. They quote arguments, use literal paths and disable external diff/text conversion. `run_tests`, `run_build` and `run_lint` detect Cargo, Node scripts/lockfiles, Go and Python. They execute through the normal tool permission gate, report the actual command, honor timeouts and return real failures. Required executables and project scripts must exist.

Context7 is available through `context7_search`, `context7_resolve_library` and `context7_query_docs`. `/context7` opens its UI: enabled state, masked API key, environment variable name, timeout, draft connection check and key removal. Saving updates the tools immediately. An exported environment key takes precedence over the optional saved key. Requests use the official REST endpoints with bearer authentication, bounded responses, cancellation and explicit auth/rate-limit errors. Keys are masked in forms and omitted from status/events. Missing credentials produce actionable UI setup guidance and official-docs fallback.

## Complete configuration UI

`/settings` provides common controls, including per-session Ultra Loop, supervisor model/review budget, goal and plan mode. **All configuration** exposes every typed user-facing config field in a nested browser: appearance/color/ASCII mode; UI motion, placeholder, footer/banner and spinner text; session/context/retry/check settings; provider auth/headers/timeouts/models; model budgets/capabilities; combo members/retries/requirements; permissions and write roots; MCP command/URL/args/environment; lifecycle hook event lists; Context7. Schema-version metadata is read-only. Reserved fields such as cost ceilings and the legacy host list are labeled as metadata rather than promised enforcement.

The browser supports user, shared project and local-project scopes. Edits are staged with explicit save/discard, complete validation, sparse overlay persistence, comment/extension preservation, external-change checks and live application. Clearing an optional project/local field restores inheritance; inherited named entries must be removed at their parent scope. Scope switching requires saving or discarding pending changes. The menu stays open after saves and shows errors/results beside the settings. Left/Right adjusts values, Enter opens a field/category, Ctrl+D removes collection entries, and Escape returns through page history. Credentials have masked replacement and explicit keep/clear actions. File writes use 0600 on Unix.

Saving refreshes provider/model routing, permissions/write roots, effort/context limits, UI preferences and Context7. MCP changes reconnect using the effective scoped settings, with failures reported while the saved configuration is retained. Hook definitions are read from effective settings when invoked. Root changes preserve the file-mutation sequence used by undo.

## Scoped guidance and progressive skills

- Root `AGENTS.md` is refreshed each turn; root `CLAUDE.md` and general `.loon/rules/*.md` rules are refreshed for requests.
- Path-based tool results include nested `AGENTS.md`/`CLAUDE.md` and matching rules. Read the relevant directory/file before editing so these instructions are available before a mutation.
- Rules can use inline frontmatter such as `paths: [src/**/*.rs, tests/**/*.rs]`. Rules without `paths` apply generally.
- Twenty-six coding skills are embedded in the binary, covering implementation, debugging, review, testing, refactoring, documentation, language-specific work, frontend/backend, APIs, databases/migrations, dependencies, DevOps, security, performance and releases. The prompt directs the agent to load the relevant workflow and language/domain skill. Full bodies live in `crates/loon-core/skills/`.
- Skills load from global `<data_dir>/skills/` and workspace `.loon/skills/`, as `NAME.md` or `NAME/SKILL.md`. Workspace definitions override global ones, and both override bundled workflows.
- Skill frontmatter supports a `description`; the command name comes from the filename/directory. Only names and descriptions enter the discovery catalog. `load_skill` returns the body, and active bodies are refreshed/re-injected after compaction (up to eight active skills, 8,000 characters each).
- Frontmatter supports simple scalars and inline comma-separated lists. It is not a full YAML interpreter and never executes embedded substitutions.

## Subagent profiles

Create `.loon/agents/reviewer.md` or `<data_dir>/agents/reviewer.md`:

```markdown
---
name: reviewer
description: Review Rust changes with file references
model: local/code-model
effort: high
maxTurns: 20
tools: [read, grep, glob, tree, note, task]
disallowedTools: [bash, write, edit, apply_patch]
---
Inspect the supplied task and relevant code. Report verified defects and evidence.
```

The model uses `delegate` with `{"agent":"reviewer","task":"..."}`. Profiles can set `isolation: worktree` to create a detached Git worktree. Worktrees are retained for review; the report includes their path and the child session ID. Inspect/cherry-pick needed changes and remove the worktree with Git when finished. A worktree starts from committed HEAD and does not include uncommitted parent changes.

Children have separate histories and journals, inherit the parent's permission rules, receive only the task text, and cannot recursively delegate. MCP tools may be inherited; profile tool restrictions still apply. `maxTurns` limits model requests, with a visible incomplete result on exhaustion. Parent cancellation propagates.

`delegate` blocks until the child answers. `background_spawn` returns an id immediately and the child runs concurrently; the parent keeps working and collects the result later.

- `background_list` reports each agent's status and, once finished, the report it produced.
- `background_message` steers a running child. The message is applied at the child's next step boundary, through the same channel as user steering, so a long task can be redirected without restarting it. Sending to a finished agent fails rather than silently doing nothing.
- `background_kill` cancels a running child cooperatively: it stops at its next step boundary and is journaled as cancelled. Aborting the task outright would skip that bookkeeping.

A finished child reports twice: the parent's turn loop drains pending reports at each step boundary and records them as context, so the model sees what its agents did on its next request; and `AgentEvent::SubagentFinished` reaches the UI, which renders the status and the report in the transcript. A report that arrives after the parent's last step is still collected by the next turn.

`background_spawn` fails the tool call when the task cannot be built (unknown profile, bad `cwd`, unresolvable model) instead of surfacing it later as a background failure. Children still cannot recursively delegate, and they inherit the parent's cancellation.

## Lifecycle hooks

Hooks use ordinary settings. For example:

```toml
[[hooks.PreToolUse]]
matcher = "write|edit|apply_patch"
command = "python3 scripts/check-edit.py"
timeout_secs = 10

[[hooks.Stop]]
command = "python3 scripts/check-completion.py"
timeout_secs = 15
```

Supported events: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure`, `PreCompact`, `PostCompact`. Unsupported event names produce config warnings.

Each hook has exactly one `command` or HTTP `url`. Commands receive JSON on stdin, HTTP hooks receive the same JSON body. It contains `hook_event_name`, `session_id`, `cwd`, `data`, and the event's fields at top level (`tool_name`, `tool_input`, etc.). Commands run in the workspace with isolated stdio and bounded output; HTTP redirects are disabled. `matcher` is an exact tool name, `*`, or `|`-separated names. Timeouts and cancellation terminate commands. SessionEnd runs on normal host shutdown, with an overall 15-second bound; forced OS termination cannot run hooks.

Exit 2 blocks pre-operations and stops. Exit 0 can return:

```json
{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"Explain the permitted alternative","additionalContext":"Project-specific guidance"}}
```

A Stop hook can return `{"decision":"block","reason":"Tests still fail; repair them"}` to continue the agent. Plain stdout adds context. A failing/timed-out pre-tool or prompt hook blocks that operation. A failed post hook is reported but cannot undo a completed tool. Hooks cannot grant access over a permission denial. Post-tool extra context is model-visible; hook stdout never writes directly over the terminal UI.

## Settings and permission precedence

Effective startup settings merge built-ins/user configuration, `.loon/config.toml`, `.loon/config.local.toml`, then host/CLI overrides. Tables merge recursively; arrays replace. Quick preferences save user defaults. The complete configuration browser selects user/project/local scope and applies the effective result immediately. There is no managed enterprise policy layer.

Permission decisions use **deny → ask → allow → mode defaults**. Rules support exact tool names, resolved absolute `path_prefix`, `command_prefix`, and an exact URL `host`:

```toml
[[permissions.allow_rules]]
tool = "bash"
command_prefix = "cargo test"

[[permissions.ask_rules]]
tool = "bash"
command_prefix = "git push"

[[permissions.deny_rules]]
tool = "fetch"
host = "private.example"
```

Command allow rules reject composed shell commands and shell substitutions; a prefix allow is not a shell sandbox. Path rules govern explicit path arguments, not arbitrary filesystem effects inside Bash or remote servers. Bypass still honors explicit deny and ask rules. Ask/auto approvals remain visible and headless runs cannot pretend a user approved.

## MCP support and boundaries

Both stdio and Streamable HTTP support initialization, tools/list pagination and tools/call. HTTP accepts JSON and SSE responses, correlates response IDs, and propagates the negotiated session ID/protocol version. Advertised resources and prompts become searchable adapter tools with list/read/templates and list/get operations. Server errors and connection failures remain visible; oversized responses/results are bounded.

Legacy SSE endpoints, WebSockets, OAuth discovery/token refresh, MCP sampling/elicitation, push catalog-change notifications, plugin marketplaces, LSP integration, cross-device/cloud handoff and voice control are not implemented. This is a local coding harness, and those reference features are not represented as working controls.
