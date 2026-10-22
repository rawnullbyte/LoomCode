---
name: devops
description: Repair build, deployment and CI workflows with reproducible checks
---
# Repair build, deployment and CI workflows with reproducible checks

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect manifests, lockfiles, build scripts, CI configuration, container/runtime assumptions and environment-variable requirements.
2. Reproduce the failing stage locally where possible. Distinguish dependency resolution, compilation, linkage, test and deployment failures.
3. Make reproducible changes with pinned inputs where appropriate. Avoid embedding credentials and avoid printing environment dumps.
4. Verify the affected commands and configuration. Prepare deployment changes for review; perform external deployment only within user authorization.
