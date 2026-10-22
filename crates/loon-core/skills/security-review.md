---
name: security-review
description: Review code changes for concrete exploitable security defects
---
# Review code changes for concrete exploitable security defects

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect the diff and relevant callers, identify attacker-controlled inputs and privilege boundaries.
2. Check authorization, injection, traversal/symlinks, unsafe deserialization, redirects/SSRF, credential handling and resource exhaustion where relevant.
3. Validate actual exploitability with safe isolated fixtures. Report realistic prerequisites, impact and a practical fix with file references.
4. Do not inflate hypothetical findings, expose secrets, or make unauthorized external requests.
