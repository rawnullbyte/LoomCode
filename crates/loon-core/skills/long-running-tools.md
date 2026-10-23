---
description: Use persistent shells and process monitors without blocking the coding workflow.
---
Use bash for bounded one-shot commands. Use shell_session start/run/cwd/close when exported variables, working directory or a virtualenv must persist. It is pipe-based, not a PTY: do not launch editors, interactive password prompts or full-screen apps.
Use monitor start for a development server, test watcher or tail. Save the monitor_id and cursor in notes; wait_for a readiness pattern with a bounded timeout, then poll incrementally instead of repeated sleeps or full log reads. Always check running and exit_code. Use logs for diagnostics, stop and remove when no longer needed. Use process for ordinary background jobs. Do independent coding while a watcher runs. Clean up owned resources at task completion; keep a server only if the user needs it. {{args}}
