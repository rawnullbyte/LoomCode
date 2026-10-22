---
name: refactor
description: Refactor while preserving public behavior and compatibility
---
# Refactor while preserving public behavior and compatibility

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Locate callers, contracts, serialization formats and tests before choosing a boundary. Write behavior-preserving invariants in notes.
2. Move in coherent steps with a working tree that can be reviewed. Avoid opportunistic unrelated changes.
3. Keep public interfaces and compatibility unless the user authorizes a migration; explicitly handle every call site if changing one.
4. Run focused tests after meaningful steps and the appropriate build/lint check. Review the diff for accidental behavioral changes.
