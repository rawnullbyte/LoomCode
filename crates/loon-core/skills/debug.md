---
name: debug
description: Reproduce and repair a defect using evidence and a regression check
---
# Reproduce and repair a defect using evidence and a regression check

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Capture the exact symptom, triggering input, environment/version and expected behavior. Reproduce with the narrowest command or fixture.
2. Trace callers and state transitions with find_symbol, grep, git_log and git_blame. Separate observations from hypotheses.
3. Test one falsifiable hypothesis at a time. Save each failed approach and its result with note so compaction does not cause repetition.
4. Inspect stderr, exit codes and stored output via read_output. A missing command or invalid argument is a repairable setup issue, not proof the task is impossible.
5. Fix the cause at the appropriate boundary. Add a regression that fails without the fix and covers adjacent edge cases.
6. Rerun the original reproduction and focused tests; inspect the diff and report evidence.
