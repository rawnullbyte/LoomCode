---
name: migrations
description: Plan and implement compatible schema or data migrations
---
# Plan and implement compatible schema or data migrations

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect current schema/version tracking and deployment order. Identify rollback/roll-forward constraints.
2. Design an expand/migrate/contract sequence when clients may overlap. Bound backfills and make retries idempotent.
3. Preserve data, validate constraints and provide checks for partial completion. Use fixtures or an isolated local database.
4. Test upgrade paths and recovery behavior. Document commands and destructive boundaries; never assume production access or approval.
