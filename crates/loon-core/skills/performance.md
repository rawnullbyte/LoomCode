---
name: performance
description: Profile latency, throughput and memory before optimizing
---
# Profile latency, throughput and memory before optimizing

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Define a representative benchmark and correctness baseline; identify what the user is experiencing.
2. Measure hot paths, allocation, I/O, rendering and external requests separately. Include warm/cold behavior where relevant.
3. Address the measured bottleneck without weakening validation, cancellation or bounds.
4. Rerun the same workload and tests; report measured results and uncertainty without unsupported comparative claims.
