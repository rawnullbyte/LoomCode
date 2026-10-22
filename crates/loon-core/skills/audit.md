---
name: audit
description: Trace realistic security boundaries and verify findings
---
# Trace realistic security boundaries and verify findings

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Map sources, trust boundaries, validation, authorization, filesystem/network access and secret handling.
2. Trace actual inputs through execution paths, redirects, parsers, shell argument construction, caches and persistence.
3. Confirm suspected vulnerabilities with safe local tests. Explain preconditions, impact, affected paths and a practical mitigation.
4. Keep verified findings separate from theoretical concerns. Do not modify files or publish findings unless requested.
