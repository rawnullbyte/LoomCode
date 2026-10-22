---
name: go
description: Implement idiomatic Go with bounded concurrency and clear contracts
---
# Implement idiomatic Go with bounded concurrency and clear contracts

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect go.mod, package boundaries, interfaces and existing test patterns.
2. Propagate context cancellation/deadlines, close resources, handle errors explicitly and avoid leaking goroutines.
3. Use clear ownership around channels/shared state. Keep interfaces small and dependencies focused.
4. Run relevant go test/go vet and race checks when concurrency changes justify them. Verify behavior before claiming completion.
