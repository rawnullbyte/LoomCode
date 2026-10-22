---
name: docs-research
description: Ground library/API work in current version-specific documentation
---
# Ground library/API work in current version-specific documentation

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Identify the dependency and installed version from the repository before searching.
2. Use context7_search with a focused question, or context7_resolve_library then context7_query_docs with an actual returned libraryId. Prefer the matching version.
3. If Context7 is unavailable or has no result, use fetch on official documentation. Do not retry missing credentials or invent an answer.
4. Extract only relevant APIs, constraints and examples. Record sources, version and remaining uncertainty in note, then apply the information to the task.
