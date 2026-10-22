---
name: implement
description: Implement a feature end to end with explicit acceptance criteria
---
# Implement a feature end to end with explicit acceptance criteria

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Use project_info, git_status, read_many, find_symbol and grep to locate the implementation, callers and existing tests. Read applicable scoped guidance before editing.
2. Translate each user requirement into an observable acceptance check and a todo. Identify invariants, compatibility requirements and unrelated user changes.
3. Check installed dependency versions. Use context7_search for uncertain or version-sensitive APIs, or resolve then query focused documentation. Never invent documentation when credentials or results are unavailable.
4. Implement the smallest coherent change using edit/multi_edit/apply_patch or structured editors. Re-read on conflicts; preserve unrelated modifications.
5. Run targeted run_tests/run_build/run_lint checks or the repository's documented Bash command. Diagnose failures and fix regressions.
6. Read git_diff, reconcile every todo with actual evidence, then report changed behavior, validation and any specific unresolved blocker.
