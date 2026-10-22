---
name: document
description: Write documentation verified against current code and commands
---
# Write documentation verified against current code and commands

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Identify the reader and task. Read actual interfaces, configuration defaults and supported behavior.
2. Use Context7 for dependency/API specifics when needed and verify examples against installed versions.
3. Document working setup, examples, error recovery and explicit limitations without claiming unsupported features.
4. Check paths, links and relevant commands. Update stale sections and avoid copying the same instructions into multiple places.
