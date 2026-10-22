---
name: database
description: Inspect and change database behavior with explicit data invariants
---
# Inspect and change database behavior with explicit data invariants

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Read the schema, queries, indexes and transaction boundaries. Confirm the actual database/version.
2. Identify integrity rules, cardinality, locking/isolation, consistency and relevant query plans.
3. Use parameterized queries. Add appropriate constraints/indexes after checking existing data and write/read tradeoffs.
4. Test rollback, duplicate input, concurrent access and empty/large data cases where relevant. Do not run destructive production changes without authorization.
