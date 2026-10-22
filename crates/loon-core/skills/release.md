---
name: release
description: Prepare a reproducible, validated release and handoff
---
# Prepare a reproducible, validated release and handoff

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect release/version conventions, packaging, CI and supported platforms. List required artifacts and acceptance checks.
2. Run release-mode build and relevant tests; smoke the produced binary/package, not only a development build.
3. Update changelog/setup instructions to match actual behavior. Verify archive contents and omit build caches/credentials.
4. Prepare release notes with actual checks and known limitations. Publish or deploy only within explicit user authorization.
