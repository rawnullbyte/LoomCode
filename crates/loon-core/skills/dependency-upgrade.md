---
name: dependency-upgrade
description: Upgrade dependencies with API and lockfile compatibility checks
---
# Upgrade dependencies with API and lockfile compatibility checks

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Read manifests and lockfiles, identify current/target versions and runtime/MSRV constraints.
2. Use Context7 or official migration guides for changed APIs and breaking behavior. Record necessary migration steps as todos.
3. Update manifests, lockfiles and call sites together; avoid unrelated broad upgrades.
4. Run targeted tests plus build/lint checks and inspect the final dependency diff. Report compatibility changes and actual evidence.
