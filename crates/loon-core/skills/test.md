---
name: test
description: Design useful regression tests around externally observable behavior
---
# Design useful regression tests around externally observable behavior

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect existing test conventions and the actual implementation/callers. Identify failure modes and boundary values.
2. Prefer a test that fails for a real bug: malformed inputs, Unicode, empty/large values, retries, timeouts, cancellation, ordering and persistence as applicable.
3. Use deterministic local fixtures for external services. Assert outcomes and relevant side effects, not duplicated implementation logic.
4. Run the new cases and appropriate surrounding suite. Keep unrelated pre-existing failures visible and distinguish them from regressions.
