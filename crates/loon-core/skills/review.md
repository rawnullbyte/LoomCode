---
name: review
description: Review code for verified, actionable correctness and security defects
---
# Review code for verified, actionable correctness and security defects

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect git_status and git_diff, then read complete changed functions, callers and relevant tests. Follow the intended behavior, not just style.
2. Check error paths, concurrency, resource lifetime, cancellation, input validation, compatibility and persistence. Use history when intent is unclear.
3. Attempt to reproduce suspected bugs with minimal fixtures or existing tests. Distinguish confirmed defects from hypotheses.
4. Prioritize concrete regressions. Give file/symbol, trigger, consequence and a practical correction for each finding.
5. Do not edit code unless requested. If no actionable defects are found, state that and identify meaningful coverage gaps without inventing issues.
