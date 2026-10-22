---
name: rust
description: Implement idiomatic Rust with ownership, cancellation and error discipline
---
# Implement idiomatic Rust with ownership, cancellation and error discipline

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect workspace manifests, features, lockfile, MSRV and crate boundaries. Check relevant trait implementations and callers.
2. Use ownership and typed errors to make lifetimes/resource cleanup explicit. Review Send/Sync, lock scope, async cancellation and task detachment.
3. Avoid panics on untrusted input, blocking I/O in hot async paths and unchecked indexing/UTF-8 slicing. Bound buffers and concurrent work.
4. Use Context7/official docs for unfamiliar crate APIs at installed versions. Run focused cargo tests, cargo check and project formatting/clippy conventions.
