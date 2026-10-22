---
name: typescript
description: Implement type-safe JS/TS features using the installed runtime/framework
---
# Implement type-safe JS/TS features using the installed runtime/framework

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect tsconfig, module mode, runtime, package manager/lockfile and framework version.
2. Use precise types at external boundaries, validate runtime data, and handle asynchronous errors and cancellation. Avoid hiding defects with any or assertions.
3. Check framework/library behavior with Context7 when version-sensitive. Preserve existing state and component conventions.
4. Run focused tests, type checking and configured lint/build scripts; check emitted/runtime behavior where appropriate.
