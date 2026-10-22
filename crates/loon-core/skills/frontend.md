---
name: frontend
description: Build responsive accessible interfaces with observable behavior
---
# Build responsive accessible interfaces with observable behavior

Task: {{args}}

For a multi-step request, call todo_read first, then todo_write with every requested deliverable, validation and final review. Keep one item in_progress. Record paths, decisions, failed attempts and blockers with note; use task descriptions/dependencies for larger work. Update the checklist as work changes. Do not mark unchecked work complete. Use tools to deliver the result; a plan alone is insufficient. Respect plan mode and permission decisions.

1. Inspect existing components, design tokens, state ownership and interaction conventions before adding UI.
2. Define loading, empty, success, error, keyboard/focus and resize states. Preserve user input through asynchronous updates.
3. Use installed component/framework APIs; look up uncertain version-specific behavior with Context7.
4. Implement cohesive components and state transitions. Avoid unnecessary dependencies and motion that obscures work.
5. Use the project's tests and available rendering tools to verify behavior at narrow/wide sizes. Never claim a visual check you did not perform.
