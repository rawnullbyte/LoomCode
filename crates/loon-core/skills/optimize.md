---
name: optimize
description: Measure and improve a real performance bottleneck
---
# Measure and improve a real performance bottleneck

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Define a repeatable workload, representative inputs and a baseline including correctness checks. Do not optimize from intuition alone.
2. Identify the bottleneck through profiling, tracing or targeted measurement. Separate CPU, I/O, allocation, rendering and external service latency.
3. Make a focused change and measure the same workload under comparable conditions. Preserve cancellation, bounds and error semantics.
4. Report measured before/after results with limitations; do not fabricate benchmark numbers or claim superiority without a comparison.
