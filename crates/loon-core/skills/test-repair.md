---
name: test-repair
description: Diagnose failing or flaky tests and repair the underlying behavior
---
# Diagnose failing or flaky tests and repair the underlying behavior

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Run the exact failing case, capture full diagnostics with read_output, and classify deterministic failures versus environmental/flaky behavior.
2. Read test intent and production callers. Do not weaken assertions or skip tests merely to obtain green output.
3. For flakiness, inspect timing, shared state, nondeterministic order, network dependence and cleanup.
4. Fix the defect or fixture determinism, rerun the original case and meaningful neighboring tests, and record evidence and any pre-existing failures.
