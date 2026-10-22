---
name: backend
description: Implement resilient backend logic and service boundaries
---
# Implement resilient backend logic and service boundaries

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Trace the request lifecycle, domain rules, storage transactions and external dependencies.
2. Define input/output contracts, validation, authorization, idempotency, timeout/cancellation and error mapping before coding.
3. Keep domain logic testable and side effects explicit. Use transaction boundaries and bounded retries; avoid retrying non-idempotent writes blindly.
4. Add focused service/handler tests for success and failure paths, then run relevant checks and review observable behavior.
