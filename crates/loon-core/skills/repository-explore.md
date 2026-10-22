---
name: repository-explore
description: Build a concise evidence-based map of an unfamiliar repository
---
# Build a concise evidence-based map of an unfamiliar repository

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Start with project_info and list_directory; identify languages, entry points, manifests, test/CI setup and guidance.
2. Use find_symbol, read_many and grep to trace a representative execution path across modules.
3. Record useful paths, boundaries, conventions and commands in note. Avoid reading generated/vendor trees or dumping entire files.
4. Return a concise architecture map tied to real files, with the concrete next step for the user's task.
