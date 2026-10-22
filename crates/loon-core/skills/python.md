---
name: python
description: Implement maintainable Python with explicit data and failure boundaries
---
# Implement maintainable Python with explicit data and failure boundaries

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect supported Python versions, project/environment configuration and type/test conventions.
2. Keep public APIs clear, validate external data and avoid broad exception swallowing. Use context managers for resources and preserve cancellation in async code.
3. Handle path/encoding/timezone boundaries explicitly. Prefer standard libraries and existing dependencies.
4. Run targeted pytest/unittest and configured lint/type checks. Document dependency or environment limitations with the exact failure.
