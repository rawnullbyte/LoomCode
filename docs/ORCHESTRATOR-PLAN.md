# Orchestrator plan — many agents, split work, shared task graph

Goal: an orchestrating agent can split a job into a task graph, run many subagents over
those tasks concurrently (with a cap), and have the results land back on the tasks — so the
work is coordinated instead of just parallel. This document states what exists (verified),
what is missing, and the order to build it in.

## What exists today (verified)

| Piece | Where | State |
| --- | --- | --- |
| Subagents | `subagent.rs` (1 323 lines + 2 676 test lines) | `background_spawn/list/message/kill` + blocking `delegate`. Hub tracks per-job label, status, elapsed, steering, cancel, report. Live TUI panel. |
| Task graph | `tasks.rs` on `WorkState` via `WorkLog` | `Task { id, subject, description, status, blocked_by }`, durable across resume, cycle-validated deps. **Parent-only**: children get no task tool. |
| Profiles | `.loon/agents/*.md` | name, model, effort, max_turns, tool allow/deny lists, `isolation: worktree`. |
| Isolation | `build_child` | `git worktree add --detach`, kept for review. |

## The gaps (each verified against the code)

1. **No concurrency cap.** `background_spawn` is unbounded — forty parallel children each
   running a model loop is a resource problem the parent cannot see.
2. **No fan-out.** Splitting 8 tasks means 8 separate tool calls, each a full round trip.
3. **No task ↔ agent link.** A finished report is not marked against the task it worked;
   the parent must correlate by memory.
4. **Children cannot see or update the shared task graph.** They get their own worklog, so
   a child that finishes its task cannot mark it complete — only the parent can, from
   reading the report.

## Design decisions (with the reasons)

**Sharing the task graph = cloning the parent's `WorkLog`.** `WorkLog` derives `Clone` and
holds `path`, `lock: Arc<Mutex<()>>`, `staged: Arc<Mutex<Option<WorkState>>>`. A clone
shares the lock and the staged state, so two holders doing `read-modify-write` are
serialised correctly — no new locking code. A child's `task` tool then reads the *parent's*
task list, and its writes survive in the parent's session worklog. This is why the naive
"same path, fresh `WorkLog`" approach is wrong: two fresh instances hold two different
mutexes, so the RMW races.

**The cap lives in the hub, not the tool.** `background_spawn` and a new `task_fanout` both
go through `BackgroundHub::spawn`, so one check covers both. Refusal names the cap and says
to `background_kill` or wait — actionable, not just an error.

**`task_fanout` is one tool call, N children.** Takes `[{id, task}]` pairs, spawns a child
per task (up to the cap), marks each task `in_progress` with the agent id as assignee, and
returns the id map in one reply. The parent model then does one tool call instead of N.

**Reports update tasks automatically.** When a child finishes, the hub's `finish` marks its
task `completed` (or `blocked` on failure) — the parent reads the task list to see state,
and reads the report for content. No correlation by memory.

## Phases

### P1 — concurrency cap (small, unblocks everything else)
- `BackgroundHub::new_capped(max)` plus a config default (8); `spawn` refuses with a clear
  error when `running >= max`.
- Test: spawn to the cap, assert refusal, finish one, assert the next spawn succeeds.

### P2 — task ↔ agent link
- `Task` gains `assignee: Option<u64>` (agent id) and `spawned_ms: Option<i64>`.
- Hub `finish` marks the child's task. Fail-open: no task linked → no task write.

### P3 — `task_fanout`
- One call: N tasks → N children (cap-limited), returns `[{task_id, agent_id}]`.
- Per-child failures (bad profile, cap) are reported per task, not fatal to the batch.

### P4 — children share the task graph
- `ChildSpec` gains `worklog: Option<WorkLog>`; `build_child` clones the parent's log into
  the child's toolset (task tool only — plan/notes stay per-session).
- A child can `task update` its own assignment; the shared lock serialises it.

### P5 — orchestration guidance
- Prompt guidance for the parent: split → fanout → monitor → integrate; when to prefer
  `delegate` vs `task_fanout`; cap awareness.
- README section + `/help` wording.

## Verification

- Hub cap test, fanout test (2 tasks → 2 children → both reports → both tasks completed),
  shared-graph test (child's task write visible to the parent), and the existing 40
  subagent tests staying green.
- `task_fanout` with zero tasks and an unknown profile are per-task errors, not panics.
