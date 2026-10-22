---
name: handoff
description: Preserve objective, evidence and next actions for continuation
---
# Preserve objective, evidence and next actions for continuation

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Read todo_read, task list, notes and the current diff. Retain the user's exact objective and constraints.
2. Write the exact relevant paths/symbols, completed changes, commands run and real outcomes, failed approaches and unresolved questions.
3. Keep unfinished todos pending/in_progress/blocked. Record blockers with the missing input and a concrete next action.
4. Save durable state with note/task/todo_write before producing a concise operational handoff. Never replace the actual state with a vague summary.
