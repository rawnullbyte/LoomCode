---
name: api-design
description: Design and implement compatible, validated APIs
---
# Design and implement compatible, validated APIs

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Identify clients, existing versioning and backward-compatibility constraints. Define concrete request/response examples and error envelopes.
2. Validate input at the boundary, enforce authorization, and bound pagination, payload size, timeouts and fan-out.
3. Design idempotency and concurrency semantics for mutations. Avoid leaking internal errors or secrets.
4. Update handlers, schemas, clients and contract tests together. Verify both valid and invalid requests.
