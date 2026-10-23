//! Slash commands: one parser shared by the TUI and headless surfaces, and one
//! executor on [`Agent`]. Commands with real machinery execute it; anything not built
//! says so explicitly instead of pretending.

use crate::rewind::RewindMode;
use crate::Agent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Shell {
        args: Vec<String>,
    },
    Monitors {
        args: Vec<String>,
    },
    Jobs {
        args: Vec<String>,
    },
    UltraLoop {
        args: Vec<String>,
    },
    UltraCode {
        task: String,
    },
    Tools {
        name: Option<String>,
    },
    Undo,
    Redo,
    /// /rewind [n] [conversation|workspace|both]
    Rewind {
        n: usize,
        mode: Option<String>,
    },
    Checkpoint {
        name: String,
    },
    Fork {
        name: String,
    },
    Compact,
    /// /context [SIZE|PERCENT%] — usage report, or set the window in one command.
    Context {
        size: Option<String>,
    },
    Diff,
    /// /memory — show; /memory global|workspace — show one scope;
    /// /memory <scope> <text> — save.
    Memory {
        scope: Option<String>,
        text: Option<String>,
    },
    /// /model — show; /model REF — switch mid-session.
    Model {
        reference: Option<String>,
    },
    /// /config — show; /config set <key> <value> — change and persist.
    Config {
        args: Vec<String>,
    },
    /// /new — abandon this session, start fresh (journal kept). `/clear` is an alias.
    New,
    /// /resume [n|id] — list sessions or switch to one.
    Resume {
        target: Option<String>,
    },
    /// /rename <title> — rename this session.
    Rename {
        title: String,
    },
    /// /delete <id|n> — delete a saved session and its journal.
    Delete {
        target: String,
    },
    /// /theme — cycle transparent/opaque.
    Theme,
    ThemeSet {
        name: String,
    },
    Tasks {
        args: Vec<String>,
    },
    Agents,
    Context7,
    PlanMode {
        setting: String,
    },
    Todos {
        args: Vec<String>,
    },
    Notes {
        args: Vec<String>,
    },
    /// /usage — per-day token/request totals from the journal.
    Usage,
    /// /init — generate AGENTS.md for this workspace.
    Init,
    /// /provider add NAME BASE_URL [api] [key_env] | list
    Provider {
        args: Vec<String>,
    },
    /// /combo add NAME provider:model [provider:model...] | list
    Combo {
        args: Vec<String>,
    },
    /// /mcp — browse, add, edit, remove or reconnect MCP servers.
    Mcp {
        args: Vec<String>,
    },
    /// /shadow <path> — the mutation history of one file, restorable via /undo.
    Shadow {
        path: String,
    },
    /// /shadow-restore <seq> <path> — restore the file to its state BEFORE seq.
    ShadowRestore {
        seq: u64,
        path: String,
    },
    /// /sessions search <query> — find text across all sessions.
    SessionsSearch {
        query: String,
    },
    /// /find <query> — search this session's transcript.
    Find {
        query: String,
    },
    /// /doctor — offline health checks.
    Doctor,
    /// /export [path] — write the session as Markdown.
    Export {
        path: Option<String>,
    },
    /// /workflows — list user-defined skills (workflows).
    Workflows,
    /// /autocompact on|off|toggle — enable/disable context compaction.
    AutoCompact {
        setting: String,
    },
    /// /effort low|medium|high — output budget per request.
    Effort {
        level: String,
    },
    /// /goal <text|clear> — session goal, injected into every request.
    Goal {
        text: String,
    },
    /// /btw <text> — record context without triggering a turn.
    Btw {
        text: String,
    },
    /// /recap — summarize the session so far without compacting.
    Recap,
    /// /loop <interval> <prompt> — recurring prompt task.
    Loop {
        interval: String,
        prompt: String,
    },
    /// /loops — list loop tasks.
    Loops,
    /// /loopcancel <id> — cancel one loop task.
    LoopCancel {
        id: u64,
    },
    /// A user-defined skill (`/name args…`) from `skills/*.md`. Resolved against the
    /// agent's loaded skills; a name with no skill behind it is an unknown command.
    Skill {
        name: String,
        args: String,
    },
    Quit,
}

/// Parse one input line starting with `/`. Returns None for non-commands. Unknown
/// names come back as [`SlashCommand::Skill`] and are resolved (or refused) by the
/// agent, which knows the loaded skill set.
pub fn parse(input: &str) -> Option<Result<SlashCommand, String>> {
    let line = input.trim();
    let rest = line.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let name = parts.next()?;
    let args: Vec<&str> = parts.collect();
    let cmd = match name {
        "help" => SlashCommand::Help,
        "shell" | "shells" => SlashCommand::Shell {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "monitor" | "monitors" => SlashCommand::Monitors {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "jobs" => SlashCommand::Jobs {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "ultraloop" => SlashCommand::UltraLoop {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "ultracode" => SlashCommand::UltraCode {
            task: args.join(" "),
        },
        "tools" => SlashCommand::Tools {
            name: args.first().map(|s| s.to_string()),
        },
        "undo" => SlashCommand::Undo,
        "redo" => SlashCommand::Redo,
        "rewind" => {
            let n = args
                .first()
                .and_then(|a| a.parse::<usize>().ok())
                .unwrap_or(1);
            let mode = args.get(1).map(|s| s.to_string()).or_else(|| {
                // single arg that is a mode, not a count
                args.first()
                    .filter(|a| a.parse::<usize>().is_err())
                    .map(|s| s.to_string())
            });
            SlashCommand::Rewind { n, mode }
        }
        "checkpoint" => SlashCommand::Checkpoint {
            name: args.join(" "),
        },
        "fork" => SlashCommand::Fork {
            name: args.join(" "),
        },
        "compact" => SlashCommand::Compact,
        "context" => SlashCommand::Context {
            size: (!args.is_empty()).then(|| args.join(" ")),
        },
        "diff" => SlashCommand::Diff,
        "memory" => match args.len() {
            0 => SlashCommand::Memory {
                scope: None,
                text: None,
            },
            1 => SlashCommand::Memory {
                scope: Some(args[0].to_string()),
                text: None,
            },
            _ => {
                if args[0] == "global" || args[0] == "workspace" {
                    SlashCommand::Memory {
                        scope: Some(args[0].to_string()),
                        text: Some(args[1..].join(" ")),
                    }
                } else {
                    SlashCommand::Memory {
                        scope: Some("workspace".into()),
                        text: Some(args.join(" ")),
                    }
                }
            }
        },
        "model" => SlashCommand::Model {
            reference: (!args.is_empty()).then(|| args.join(" ")),
        },
        "config" | "settings" => SlashCommand::Config {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "new" | "clear" => SlashCommand::New,
        "resume" => SlashCommand::Resume {
            target: (!args.is_empty()).then(|| args.join(" ")),
        },
        "delete" => SlashCommand::Delete {
            target: args.join(" "),
        },
        "rename" => SlashCommand::Rename {
            title: args.join(" "),
        },
        "theme" => {
            if args.is_empty() {
                SlashCommand::Theme
            } else {
                SlashCommand::ThemeSet {
                    name: args.join(" "),
                }
            }
        }
        "todos" | "todo" => SlashCommand::Todos {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "notes" | "note" => SlashCommand::Notes {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "usage" => SlashCommand::Usage,
        "init" => SlashCommand::Init,
        "provider" | "providers" => SlashCommand::Provider {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "combo" | "combos" => SlashCommand::Combo {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "export" => SlashCommand::Export {
            path: (!args.is_empty()).then(|| args.join(" ")),
        },
        "find" => SlashCommand::Find {
            query: args.join(" "),
        },
        "doctor" => SlashCommand::Doctor,
        "sessions" if args.first() != Some(&"search") => SlashCommand::Resume { target: None },
        "sessions" => SlashCommand::SessionsSearch {
            query: args[1..].join(" "),
        },
        "shadow-restore" => SlashCommand::ShadowRestore {
            seq: args.first().and_then(|a| a.parse().ok()).unwrap_or(0),
            path: args.get(1..).map(|a| a.join(" ")).unwrap_or_default(),
        },
        "shadow" => SlashCommand::Shadow {
            path: args.join(" "),
        },
        "mcp" => SlashCommand::Mcp {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "workflows" | "skills" => SlashCommand::Workflows,
        "autocompact" => SlashCommand::AutoCompact {
            setting: args.first().copied().unwrap_or("toggle").to_string(),
        },
        "effort" => SlashCommand::Effort {
            level: args.first().copied().unwrap_or_default().to_string(),
        },
        "goal" => SlashCommand::Goal {
            text: args.join(" "),
        },
        "btw" => SlashCommand::Btw {
            text: args.join(" "),
        },
        "recap" => SlashCommand::Recap,
        "loop" => SlashCommand::Loop {
            interval: args.first().copied().unwrap_or_default().to_string(),
            prompt: args.get(1..).map(|a| a.join(" ")).unwrap_or_default(),
        },
        "loops" => SlashCommand::Loops,
        "loopcancel" => SlashCommand::LoopCancel {
            id: args.first().and_then(|a| a.parse().ok()).unwrap_or(0),
        },
        "context7" => SlashCommand::Context7,
        "tasks" => SlashCommand::Tasks {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "agents" => SlashCommand::Agents,
        "plan" => SlashCommand::PlanMode {
            setting: args.first().copied().unwrap_or("status").to_string(),
        },
        "quit" | "exit" => SlashCommand::Quit,
        // Anything else may be a user-defined skill: resolved against the agent's
        // loaded skills; a miss is reported as an unknown command with the same wording
        // as before.
        other => SlashCommand::Skill {
            name: other.to_string(),
            args: args.join(" "),
        },
    };
    Some(Ok(cmd))
}

/// One configurable setting: the single source of truth that drives `/config`
/// suggestions (keys, then values), validation and persistence.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingDef {
    pub key: &'static str,
    pub description: &'static str,
    pub kind: SettingKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingKind {
    Bool,
    Int { min: u64, max: u64 },
    Enum(&'static [&'static str]),
    Text,
}

impl SettingKind {
    /// Value suggestions for the completion popup.
    pub fn suggestions(&self) -> Vec<String> {
        match self {
            SettingKind::Bool => vec!["true".into(), "false".into()],
            SettingKind::Enum(vals) => vals.iter().map(|s| s.to_string()).collect(),
            SettingKind::Int { .. } | SettingKind::Text => Vec::new(),
        }
    }
}

/// Every setting `/config` understands — typing `/config ` suggests these keys,
/// typing `/config <key> ` suggests its values.
pub fn settings_registry() -> Vec<SettingDef> {
    use SettingKind::*;
    vec![
        SettingDef {
            key: "ultraloop",
            description: "independent completion review for this session",
            kind: Bool,
        },
        SettingDef {
            key: "ultraloop_limit",
            description: "supervisor reviews; 0 is unlimited",
            kind: Int {
                min: 0,
                max: 1_000_000,
            },
        },
        SettingDef {
            key: "supervisor_model",
            description: "reviewer model reference; empty uses the current model",
            kind: Text,
        },
        SettingDef {
            key: "plan_mode",
            description: "inspection and planning only; block workspace mutations",
            kind: Bool,
        },
        SettingDef {
            key: "session_goal",
            description: "durable goal for the current session",
            kind: Text,
        },
        SettingDef {
            key: "check_command",
            description: "required verification command; empty disables",
            kind: Text,
        },
        SettingDef {
            key: "ultra_rounds",
            description: "verification repair attempts; 0 disables",
            kind: Int { min: 0, max: 1000 },
        },
        SettingDef {
            key: "max_retries",
            description: "retry budget for transient provider failures",
            kind: Int { min: 0, max: 10 },
        },
        SettingDef {
            key: "animations",
            description: "animate activity, progress, modals and tool calls",
            kind: Bool,
        },
        SettingDef {
            key: "popup_rows",
            description: "visible slash command suggestions",
            kind: Int { min: 3, max: 20 },
        },
        SettingDef {
            key: "tips",
            description: "show the input placeholder tip",
            kind: Bool,
        },
        SettingDef {
            key: "theme",
            description: "terminal | dark | light | ocean | violet | ember | graphite",
            kind: Enum(&[
                "terminal",
                "dark",
                "light",
                "ocean",
                "violet",
                "ember",
                "graphite",
                "transparent",
                "opaque",
            ]),
        },
        SettingDef {
            key: "permission_mode",
            description: "bypass | auto | ask | allowlist",
            kind: Enum(&["bypass", "auto", "ask", "allowlist"]),
        },
        SettingDef {
            key: "effort",
            description: "coding budget and supported provider reasoning",
            kind: Enum(&["low", "medium", "high", "xhigh", "ultra"]),
        },
        SettingDef {
            key: "context7_key_env",
            description: "environment variable holding your Context7 API key",
            kind: Text,
        },
        SettingDef {
            key: "model",
            description: "model key or combo:NAME",
            kind: Text,
        },
        SettingDef {
            key: "context_window",
            description: "tokens (1m, 200k) or a number; 0 disables compaction",
            kind: Int {
                min: 0,
                max: 10_000_000,
            },
        },
        SettingDef {
            key: "tool_output_bytes",
            description: "tool output cap in bytes",
            kind: Int {
                min: 1_024,
                max: 100_000_000,
            },
        },
        SettingDef {
            key: "autocompact_tokens",
            description: "estimated tokens before compaction (50k, 1m); 0 uses 80% of capacity",
            kind: Int {
                min: 0,
                max: 10_000_000,
            },
        },
        SettingDef {
            key: "autocompact_percent",
            description: "compact at this percent of the window (1-99); 0 uses 80%",
            kind: Int { min: 0, max: 99 },
        },
        SettingDef {
            key: "autocompact",
            description: "on | off — alias for context_window 128k/0",
            kind: Enum(&["on", "off"]),
        },
    ]
}

/// Look up one setting by key.
pub fn setting_def(key: &str) -> Option<SettingDef> {
    settings_registry().into_iter().find(|d| d.key == key)
}

/// The canonical command table: (name, one-line description). Single source of truth
/// for `/help` and the TUI autocomplete — a command listed here must parse.
pub fn command_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("help", "this list"),
        (
            "ultraloop [GOAL|on|off|status|limit N|model REF]",
            "independent supervisor continues unfinished work",
        ),
        (
            "ultracode [TASK]",
            "maximum coding effort; optionally start a task",
        ),
        (
            "tools",
            "browse available tools, effects and parameter schemas",
        ),
        ("undo", "undo the newest file mutation"),
        ("redo", "redo the newest undone mutation"),
        (
            "rewind [n] [mode]",
            "rewind to n-th previous request boundary",
        ),
        ("checkpoint NAME", "mark a restore point in the journal"),
        ("fork NAME", "fork the current session"),
        ("compact", "summarize older context now"),
        (
            "context [1m|200k|90%|off]",
            "context usage report; an argument sets the window",
        ),
        ("diff", "files this session changed (from the journal)"),
        (
            "memory [global|workspace] [text]",
            "show or save persistent memory",
        ),
        ("model [REF]", "show or switch model/combos:NAME"),
        (
            "config [key[=value]]",
            "show, change and persist one setting",
        ),
        (
            "provider add NAME URL [api] [key_env]",
            "add an OpenAI-compatible provider",
        ),
        ("combo add NAME provider:model ...", "define a model combo"),
        ("find <query>", "search this session's transcript"),
        ("doctor", "offline health checks"),
        ("sessions search <query>", "find text across all sessions"),
        ("shadow <path>", "mutation history of one file (restorable)"),
        (
            "shadow-restore <seq> <path>",
            "restore a file to its state before seq",
        ),
        ("mcp", "manage MCP servers and their tools"),
        ("workflows", "list user-defined skills (workflows)"),
        (
            "autocompact [1m|200k|90%|on|off|auto]",
            "compact at a token count or a % of the window",
        ),
        (
            "effort [low|medium|high|xhigh|ultra]",
            "coding and reasoning effort",
        ),
        ("goal <text|clear>", "session goal, kept in context"),
        ("btw <text>", "record context without triggering a turn"),
        ("recap", "summarize the session so far"),
        (
            "loop <interval> <prompt>",
            "run a prompt on an interval, e.g. /loop 1m check CI",
        ),
        ("loops", "list loop tasks"),
        ("loopcancel <id>", "cancel a loop task"),
        (
            "providers",
            "browse, add, edit, refresh or remove providers",
        ),
        ("combos", "manage fallback and round-robin combos"),
        ("settings", "searchable settings with descriptions"),
        (
            "theme [NAME]",
            "choose a terminal, dark, light or ocean palette",
        ),
        ("sessions", "search and resume saved sessions"),
        ("resume [ID|n]", "open session browser or resume a session"),
        ("new", "start a new session; keep this one saved"),
        (
            "shell [list|start NAME|run NAME COMMAND|close NAME]",
            "persistent named shells",
        ),
        (
            "monitors [list|start COMMAND|logs ID|stop ID|remove ID]",
            "watch servers and logs",
        ),
        ("jobs [list|poll ID|stop ID]", "manage background processes"),
        ("delete <id|n>", "delete a saved session and its journal"),
        ("clear", "start a new session (alias for /new)"),
        ("rename TITLE", "rename the current session"),
        ("skills", "list/reload bundled and custom skills"),
        (
            "tasks",
            "durable tasks: add, get, start, done, block, remove",
        ),
        ("agents", "list configured agent profiles"),
        (
            "context7",
            "documentation tools and API-key environment status",
        ),
        (
            "plan",
            "on/off/status: inspect safely before allowing workspace changes",
        ),
        (
            "todos [add|start|done|block|remove N]",
            "manage the durable session plan",
        ),
        ("notes [add TEXT|remove N]", "manage session working notes"),
        ("quit", "exit"),
    ]
}

/// One-line, length-bounded excerpt for list output. Flattening comes first: a
/// multi-line message would otherwise continue onto following lines and forge
/// extra hits in `/find` and `/sessions search`.
fn truncate_prompt(p: &str, max: usize) -> String {
    let flat = p.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "…"
    }
}

pub fn help_text() -> String {
    let mut out = String::new();
    for (name, desc) in command_list() {
        out.push('/');
        out.push_str(name);
        let pad = 38usize.saturating_sub(name.len() + 1);
        out.push_str(&" ".repeat(pad));
        out.push_str(desc);
        out.push('\n');
    }
    out.pop();
    out
}

/// Titles are stored text and older rows may contain newlines; every surface that
/// shows one is single-line, so collapse whitespace at render time too.
fn one_line_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Shortest prefix of `id` that no other id in `all` shares, so a displayed id is
/// always usable as a selector. Session ids are UUIDv7, whose leading 48 bits are a
/// millisecond timestamp: ids created in the same instant share their first 24
/// characters, and every id created within ~65s shares the first 8. A fixed 8-char
/// prefix therefore collides for any two sessions started moments apart, and the
/// listing would print two different sessions under one id.
fn unique_id_prefix(id: loon_events::EvId, all: &[loon_events::EvId]) -> String {
    let full = id.to_string();
    // Hex chars only: byte and char boundaries coincide, so slicing is safe.
    for len in 8..=full.len() {
        let candidate = &full[..len];
        if all
            .iter()
            .filter(|other| other.to_string().starts_with(candidate))
            .count()
            == 1
        {
            return candidate.to_string();
        }
    }
    full
}

impl Agent {
    /// Execute one slash command, returning a human-readable result block.
    /// `/quit` is handled by the caller (it owns the event loop).
    pub async fn run_slash(&mut self, cmd: SlashCommand) -> Result<String, String> {
        match cmd {
            SlashCommand::Shell { args } => self.run_terminal_command("shell_session", args).await,
            SlashCommand::Monitors { args } => self.run_terminal_command("monitor", args).await,
            SlashCommand::Jobs { args } => self.run_terminal_command("process", args).await,
            SlashCommand::Help => Ok(help_text()),
            SlashCommand::Undo => {
                let events = self.journal.events().map_err(|e| format!("journal: {e}"))?;
                let report = crate::rewind::undo(&self.journal, &self.ctx.ws, &events)
                    .map_err(|e| e.to_string())?;
                Ok(format!("undo: {report}"))
            }
            SlashCommand::Redo => {
                let events = self.journal.events().map_err(|e| format!("journal: {e}"))?;
                let report = crate::rewind::redo(&self.journal, &self.ctx.ws, &events)
                    .map_err(|e| e.to_string())?;
                Ok(format!("redo: {report}"))
            }
            // Bare /rewind in an interactive session opens a picker: the raw report is
            // useless for choosing, the list of turns and what they changed is not.
            SlashCommand::Rewind { n, mode }
                if mode.is_none() && n == 1 && self.events.is_some() =>
            {
                let events = self.journal.events().map_err(|e| format!("journal: {e}"))?;
                let entries = crate::rewind::rewind_entries(&self.journal, &events);
                if entries.is_empty() {
                    return Ok("nothing to rewind — no request boundaries yet".into());
                }
                let items = entries
                    .iter()
                    .map(|e| crate::catalog::MenuItem {
                        label: if e.prompt.is_empty() {
                            format!("(no prompt) · rewind {}", e.n)
                        } else {
                            e.prompt.clone()
                        },
                        detail: if e.files_changed == 0 {
                            "No code changes".into()
                        } else {
                            format!(
                                "{} file{} changed +{} −{}",
                                e.files_changed,
                                if e.files_changed == 1 { "" } else { "s" },
                                e.added,
                                e.removed
                            )
                        },
                        command: format!("/rewind {} both", e.n),
                    })
                    .collect();
                self.emit(crate::AgentEvent::OpenMenu {
                    title: "Rewind".into(),
                    items,
                });
                Ok(String::new())
            }
            SlashCommand::Rewind { n, mode } => {
                let mode = match mode.as_deref() {
                    None => RewindMode::ConversationAndWorkspace,
                    Some(s) => RewindMode::parse(s).ok_or_else(|| {
                        format!("unknown rewind mode `{s}` (conversation|workspace|both)")
                    })?,
                };
                let mode_str = mode.as_str().to_string();
                let events = self.journal.events().map_err(|e| format!("journal: {e}"))?;
                let outcome = crate::rewind::rewind_n(
                    &self.journal,
                    &self.ctx.ws,
                    &events,
                    &mut self.history,
                    n.max(1),
                    mode,
                )
                .map_err(|e| e.to_string())?;
                let report = format!(
                    "rewound {n} boundary(ies) ({mode_str}): history now {} message(s){}",
                    outcome.history_len_after,
                    outcome
                        .restore
                        .as_ref()
                        .map(|r| format!(", {r}"))
                        .unwrap_or_default()
                );
                if outcome.mode != RewindMode::WorkspaceOnly {
                    let blocks = crate::replay::visible_blocks(
                        &events,
                        Some(outcome.boundary.truncate_to_seq),
                    );
                    self.emit_async(crate::AgentEvent::ConversationRewound {
                        blocks,
                        prompt: outcome.prompt,
                    })
                    .await;
                    self.emit_async(crate::AgentEvent::ContextUpdated {
                        tokens: self.estimated_context_tokens(),
                    })
                    .await;
                    // Keep file conflicts visible; ordinary rewinds replace the
                    // transcript atomically without adding another status message.
                    if self.events.is_some()
                        && outcome
                            .restore
                            .as_ref()
                            .map(|r| r.conflicts.is_empty())
                            .unwrap_or(true)
                    {
                        return Ok(String::new());
                    }
                }
                Ok(report)
            }
            SlashCommand::Checkpoint { name } => {
                let seq = crate::rewind::checkpoint(&self.journal, &self.events()?, &name)
                    .map_err(|e| e.to_string())?;
                Ok(format!("checkpoint `{name}` at seq {seq}"))
            }
            SlashCommand::Fork { name } => {
                let seq = crate::rewind::fork(&self.journal, &self.events()?, &name)
                    .map_err(|e| e.to_string())?;
                Ok(format!(
                    "fork `{name}` created at seq {seq} (new session id journaled)"
                ))
            }
            SlashCommand::Compact => match self.compact_report(true).await {
                crate::compaction::CompactOutcome::Compacted {
                    before_tokens,
                    after_tokens,
                } => Ok(format!(
                    "context compacted: ~{before_tokens} → ~{after_tokens} est. tokens (estimate)"
                )),
                crate::compaction::CompactOutcome::NothingToDo => {
                    Ok("nothing to compact: only recent exchanges in context".into())
                }
                crate::compaction::CompactOutcome::Failed(reason) => Ok(format!(
                    "compaction failed: {reason} — history retained (/model REF picks another summarizer)"
                )),
            },
            SlashCommand::Context { size } => match size {
                None => Ok(self.context_report()),
                Some(size) => self.set_context_size(&size),
            },
            SlashCommand::Diff => Ok(self.diff_report()),
            SlashCommand::Memory { scope, text } => {
                let paths = self.memory_paths()?;
                match text {
                    Some(t) if !t.trim().is_empty() => {
                        let s = match scope.as_deref() {
                            Some("global") => crate::memory::Scope::Global,
                            _ => crate::memory::Scope::Workspace,
                        };
                        crate::memory::save(&paths, s, t.trim())
                            .map_err(|e| format!("memory write: {e}"))?;
                        Ok(format!("saved to {} memory", s.as_str()))
                    }
                    _ => {
                        let s = match scope.as_deref() {
                            Some("global") => crate::memory::Scope::Global,
                            Some("workspace") => crate::memory::Scope::Workspace,
                            _ => {
                                // show both
                                return Ok(format!(
                                    "[global]\n{}\n[workspace]\n{}",
                                    crate::memory::read_scope(&paths, crate::memory::Scope::Global),
                                    crate::memory::read_scope(
                                        &paths,
                                        crate::memory::Scope::Workspace
                                    ),
                                ));
                            }
                        };
                        Ok(crate::memory::read_scope(&paths, s))
                    }
                }
            }
            SlashCommand::Model { reference } => match reference {
                None if self.events.is_some() => {
                    self.open_catalog("models");
                    let missing = self
                        .catalog()
                        .providers
                        .into_iter()
                        .filter(|p| p.count == 0 && !self.registry.is_mock(&p.name))
                        .map(|p| p.name)
                        .collect::<Vec<_>>();
                    if !missing.is_empty() {
                        self.refresh_catalogs(missing).await;
                    }
                    Ok(String::new())
                }
                None => Ok(format!(
                    "current model: {} (context window: {} tokens)",
                    self.cfg.model_ref, self.cfg.context_window
                )),
                Some(r) => {
                    if r == "refresh" {
                        return self
                            .provider_command(vec!["refresh".into(), "all".into()])
                            .await;
                    }
                    if self.events.is_some() && (r == "all" || self.registry.provider(&r).is_some())
                    {
                        self.open_catalog(&format!("models:{r}"));
                        return Ok(String::new());
                    }
                    self.choose_model(&r)
                }
            },
            SlashCommand::Context7 => {
                if self.events.is_some() {
                    self.config_ui_command(vec![
                        "open".into(),
                        crate::config_ui::encode("/context7"),
                    ])
                    .await
                } else {
                    Ok(self.context7_status())
                }
            }
            SlashCommand::Tasks { args } => {
                let tool = crate::tasks::TaskTool {
                    log: self.worklog.clone(),
                };
                let value = match args.first().map(String::as_str) {
                    None | Some("list") => serde_json::json!({"action":"list"}),
                    Some("add" | "create") => {
                        serde_json::json!({"action":"create","subject":args[1..].join(" ")})
                    }
                    Some("get" | "remove") => {
                        serde_json::json!({"action":args[0],"id":args.get(1).and_then(|s|s.parse::<u64>().ok())})
                    }
                    Some("done" | "start" | "block") => {
                        serde_json::json!({"action":"update","id":args.get(1).and_then(|s|s.parse::<u64>().ok()),"status":match args[0].as_str() {"done"=>"completed","start"=>"in_progress",_=>"blocked"}})
                    }
                    _ => {
                        return Err(
                            "usage: /tasks [add TEXT|get ID|start ID|done ID|block ID|remove ID]"
                                .into(),
                        )
                    }
                };
                tool.apply(value).map_err(|e| e.to_string())
            }
            SlashCommand::PlanMode { setting } => {
                let state = self
                    .worklog
                    .update(|state| {
                        match setting.as_str() {
                            "on" => state.plan_mode = true,
                            "off" => state.plan_mode = false,
                            "status" => {}
                            _ => {
                                return Err(loon_tools::ToolError::InvalidArgs(
                                    "usage: /plan on|off|status".into(),
                                ))
                            }
                        };
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                let text = format!(
                    "plan mode: {}",
                    if state.plan_mode {
                        "on — workspace mutations blocked"
                    } else {
                        "off"
                    }
                );
                self.emit(crate::AgentEvent::WorkStateChanged { state });
                Ok(text)
            }
            SlashCommand::Agents => Ok(crate::subagent::profiles(
                &self.data_dir,
                self.ctx.ws.root(),
            )
            .iter()
            .map(|p| format!("{} — {}", p.name, p.description))
            .collect::<Vec<_>>()
            .join("\n")),
            SlashCommand::Skill { name, args } => {
                self.skills = crate::skills::load_with_builtins(&self.data_dir, self.ctx.ws.root());
                let Some(skill) = self.skills.iter().find(|s| s.name == name).cloned() else {
                    return Err(format!("unknown command `/{name}` — try /help"));
                };
                let _ = self.worklog.update(|s| {
                    if !s.active_skills.contains(&name) {
                        s.active_skills.push(name.clone());
                    }
                    Ok(())
                });
                let prompt = crate::skills::expand(&skill.body, &args);
                self.emit(crate::AgentEvent::MemberNote(format!(
                    "running skill /{} ({} scope)",
                    skill.name, skill.scope
                )));
                let result = self.run_turn(&prompt).await;
                Ok(format!(
                    "[skill /{} {}] {}",
                    skill.name,
                    result.status.status_text(),
                    result.assistant_text
                ))
            }
            SlashCommand::Config { args } => Ok(self.run_config_command(args).await?),
            SlashCommand::New => {
                let paths = self.loon_paths();
                let mut fresh = Agent::new_session(
                    &paths,
                    self.cfg.clone(),
                    self.registry.clone(),
                    self.tools.clone(),
                    self.ctx.ws.root().to_path_buf(),
                    true,
                )
                .map_err(|e| format!("new session: {e}"))?;
                fresh.approval_hook = self.approval_hook.clone();
                fresh.summarizer = self.summarizer.clone();
                fresh.events = self.events.take();
                fresh.loops = self.loops.clone();
                *self = fresh;
                self.refresh_delegate();
                self.emit_session_loaded();
                Ok(
                    "new session started (the old one stays in the journal — /resume to return)"
                        .into(),
                )
            }
            SlashCommand::Resume { target } => {
                // An empty selector matches every id (`starts_with("")` is always true),
                // so it would "resume" whichever session happens to be open. That is a
                // usage error, not a selection.
                if target.as_deref().is_some_and(|t| t.trim().is_empty()) {
                    return Err("usage: /resume <number|id-prefix> (the selector is empty)".into());
                }
                let paths = self.loon_paths();
                let store = crate::open_store(&paths).map_err(|e| e.to_string())?;
                let sessions = store.list_sessions().map_err(|e| e.to_string())?;
                // This session may still be pending (nothing sent yet). Asking to resume
                // the session you are already in must succeed rather than reporting
                // "no sessions yet" — but it must not force it to be written either.
                // A prefix shared with another session is NOT a match: UUIDv7 ids
                // created moments apart share their leading characters, so treating a
                // bare prefix as "this session" would silently keep the user where they
                // are and report success for the session they asked to leave.
                if let Some(want) = target.as_deref() {
                    let current = self.session_id().to_string();
                    let others = sessions.iter().any(|s| {
                        s.id != self.session_id() && s.id.to_string().starts_with(want)
                    });
                    if !others && (want == current || current.starts_with(want)) {
                        return Ok(format!("resumed session {current}"));
                    }
                }
                if sessions.is_empty() {
                    return Ok("no sessions yet".into());
                }
                let Some(target) = target else {
                    if self.events.is_some() {
                        let items = sessions
                            .iter()
                            .map(|m| crate::catalog::MenuItem {
                                // Stored titles can predate the one-line rule, and a
                                // menu row is one line: flatten instead of trusting it.
                                label: one_line_title(&m.title),
                                // Same recency the text listing shows, so the picker
                                // and `/resume` without a UI agree about the order.
                                detail: format!(
                                    "{} · {} · {}",
                                    unique_id_prefix(
                                        m.id,
                                        &sessions.iter().map(|s| s.id).collect::<Vec<_>>()
                                    ),
                                    crate::worklog::relative_time(m.updated_ms),
                                    m.cwd
                                ),
                                command: format!("/resume {}", m.id),
                            })
                            .collect();
                        self.emit(crate::AgentEvent::OpenMenu {
                            title: "Sessions".into(),
                            items,
                        });
                        return Ok(String::new());
                    }
                    let mut out = String::from(
                        "sessions (most recently used first — /resume <n> to switch):\n",
                    );
                    for (i, meta) in sessions.iter().take(10).enumerate() {
                        out.push_str(&format!(
                            "  {}: {} · {} · {} · {}\n",
                            i + 1,
                            unique_id_prefix(
                                meta.id,
                                &sessions.iter().map(|s| s.id).collect::<Vec<_>>()
                            ),
                            one_line_title(&meta.title),
                            meta.cwd,
                            crate::worklog::relative_time(meta.updated_ms)
                        ));
                    }
                    return Ok(out);
                };
                let picked = match target.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= sessions.len() => sessions[n - 1].id,
                    _ => {
                        let found: Vec<_> = sessions
                            .iter()
                            .filter(|s| s.id.to_string().starts_with(&target))
                            .collect();
                        if found.len() != 1 {
                            return Err(format!("session id {target} is unknown or ambiguous"));
                        }
                        found[0].id
                    }
                };
                let mut switched = Agent::resume(
                    &paths,
                    picked,
                    self.cfg.clone(),
                    self.registry.clone(),
                    self.tools.clone(),
                )
                .map_err(|e| format!("resume: {e}"))?;
                switched.approval_hook = self.approval_hook.clone();
                switched.summarizer = self.summarizer.clone();
                switched.events = self.events.take();
                switched.loops = self.loops.clone();
                *self = switched;
                self.refresh_delegate();
                self.emit_session_loaded();
                Ok(format!("resumed session {picked}"))
            }
            SlashCommand::Rename { title } => {
                if title.trim().is_empty() {
                    return Err("usage: /rename <title>".into());
                }
                self.journal
                    .rename_session(title.trim())
                    .map_err(|e| e.to_string())?;
                Ok(format!("renamed to `{}`", title.trim()))
            }
            SlashCommand::Delete { target } => {
                let target = target.trim().to_string();
                if target.is_empty() {
                    return Err("usage: /delete <session-id|n> (see /resume)".into());
                }
                // Check this first: the current session may still be pending, so it is
                // not in the store and the lookup below would call it "unknown".
                // Only a real prefix counts — short prefixes are ambiguous between
                // sessions created moments apart (UUIDv7 shares a timestamp prefix).
                let current = self.session_id().to_string();
                if current == target || (target.len() >= 12 && current.starts_with(&target)) {
                    return Err(
                        "cannot delete the session you are in — /new first, then delete it".into(),
                    );
                }
                let paths = self.loon_paths();
                let store = crate::open_store(&paths).map_err(|e| e.to_string())?;
                let sessions = store.list_sessions().map_err(|e| e.to_string())?;
                // Accept an index from the /resume listing or an id/prefix.
                let picked = match target.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= sessions.len() => sessions[n - 1].id,
                    _ => {
                        let found: Vec<_> = sessions
                            .iter()
                            .filter(|s| s.id.to_string().starts_with(&target))
                            .collect();
                        if found.len() != 1 {
                            return Err(format!(
                                "session {target} is unknown or ambiguous — /resume lists them"
                            ));
                        }
                        found[0].id
                    }
                };
                store.delete_session(picked).map_err(|e| e.to_string())?;
                Ok(format!("deleted session {picked}"))
            }
            SlashCommand::Theme => {
                let items = [
                    "terminal", "dark", "light", "ocean", "violet", "ember", "graphite",
                ]
                .into_iter()
                .map(|n| crate::catalog::MenuItem {
                    label: n.into(),
                    detail: match n {
                        "terminal" => "Transparent · terminal foreground and background",
                        "dark" => "Graphite · cool blue accents",
                        "light" => "Paper · dark text on light surface",
                        _ => "Deep navy · teal accents",
                    }
                    .into(),
                    command: format!("/theme {n}"),
                })
                .collect();
                self.emit(crate::AgentEvent::OpenMenu {
                    title: "Appearance".into(),
                    items,
                });
                Ok("Choose /theme terminal|dark|light|ocean|violet|ember|graphite".into())
            }
            SlashCommand::ThemeSet { name } => self.config_apply("theme", &name),
            SlashCommand::Todos { args } => self.worklog_command(false, args),
            SlashCommand::Notes { args } => self.worklog_command(true, args),
            SlashCommand::Usage => Ok(self.usage_report()),
            SlashCommand::Init => {
                let prompt = "Survey this workspace (structure, language, build/test commands, conventions) and write an AGENTS.md at the repository root covering: project overview, build/test/lint commands, code style conventions, and gotchas. Read before you write; keep it under 60 lines.";
                let result = self.run_turn(prompt).await;
                Ok(format!(
                    "[init {}] {}",
                    result.status.status_text(),
                    result.assistant_text
                ))
            }
            SlashCommand::Provider { args } => self.provider_command(args).await,
            SlashCommand::Combo { args } => self.combo_command(args),
            SlashCommand::Export { path } => {
                let path = path.unwrap_or_else(|| {
                    // The full id, not a short slice: `export_markdown` overwrites
                    // without asking, and a fixed 8-char prefix is shared by every
                    // session started within ~65s — so two exports would silently
                    // destroy each other. A filename has no width budget.
                    format!("loon-session-{}.md", self.session_id)
                });
                let written = self.export_markdown(std::path::Path::new(&path))?;
                Ok(format!("exported to {written}"))
            }
            SlashCommand::Find { query } => {
                if query.trim().is_empty() {
                    return Err("usage: /find <query>".into());
                }
                let needle = query.to_lowercase();
                let mut hits = Vec::new();
                let mut turn = 0usize;
                let Ok(events) = self.journal.events() else {
                    return Err("journal unreadable".into());
                };
                for e in &events {
                    match &e.kind {
                        loon_events::model::EventKind::UserMessage { text } => {
                            turn += 1;
                            if text.to_lowercase().contains(&needle) {
                                hits.push(format!(
                                    "  turn {turn} you: {}",
                                    truncate_prompt(text.trim(), 70)
                                ));
                            }
                        }
                        loon_events::model::EventKind::TextDelta { text, .. }
                            if text.to_lowercase().contains(&needle) =>
                        {
                            hits.push(format!(
                                "  turn {turn} loon: {}",
                                truncate_prompt(text.trim(), 70)
                            ));
                        }
                        _ => {}
                    }
                }
                if hits.is_empty() {
                    Ok(format!("no matches for `{query}`"))
                } else {
                    Ok(format!(
                        "{} match(es) for `{query}`:\n{}",
                        hits.len(),
                        hits.into_iter().take(20).collect::<Vec<_>>().join("\n")
                    ))
                }
            }
            SlashCommand::UltraCode { task } => {
                self.config_apply("effort", "ultra")?;
                if task.is_empty() {
                    return Ok("Ultra Code enabled: maximum coding effort. /ultraloop GOAL adds independent completion review.".into());
                }
                let result = self.run_turn(&task).await;
                Ok(format!("ultracode: {}", result.status.status_text()))
            }
            SlashCommand::UltraLoop { args } => self.ultraloop_command(args).await,
            SlashCommand::Tools { name } => {
                if let Some(name) = name {
                    let tool = self
                        .tools
                        .get(&name)
                        .ok_or_else(|| format!("unknown tool {name}"))?;
                    let spec = tool.spec();
                    return Ok(format!(
                        "{} · {:?}\n{}\n\n{}",
                        spec.name,
                        spec.effects,
                        spec.description,
                        serde_json::to_string_pretty(&spec.parameters).unwrap_or_default()
                    ));
                }
                let specs = self.tools.specs();
                if self.events.is_some() {
                    self.emit(crate::AgentEvent::OpenMenu {
                        title: "Available tools".into(),
                        items: specs
                            .iter()
                            .map(|s| crate::catalog::MenuItem {
                                label: format!("{} · {:?}", s.name, s.effects),
                                detail: s.description.clone(),
                                command: format!("/tools {}", s.name),
                            })
                            .collect(),
                    });
                    Ok(String::new())
                } else {
                    Ok(specs
                        .iter()
                        .map(|s| {
                            format!(
                                "{} ({:?}) — {}\n{}",
                                s.name, s.effects, s.description, s.parameters
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"))
                }
            }
            SlashCommand::Doctor => {
                let mut out = String::from("doctor (offline):\n");
                match crate::open_store(&self.loon_paths()) {
                    Ok(store) => match store.integrity_check() {
                        Ok(r) => out.push_str(&format!("  store: {r}\n")),
                        Err(e) => out.push_str(&format!("  store: ERROR {e}\n")),
                    },
                    Err(e) => out.push_str(&format!("  store: ERROR {e}\n")),
                }
                let paths = self.loon_paths();
                match loon_config::Config::load_or_builtin(&paths) {
                    Ok((_, findings)) if findings.is_empty() => out.push_str("  config: valid\n"),
                    Ok((_, findings)) => {
                        out.push_str(&format!("  config: {} finding(s)\n", findings.len()))
                    }
                    Err(e) => out.push_str(&format!("  config: ERROR {e}\n")),
                }
                out.push_str(&format!(
                    "  session: {} events journaled\n  model: {} (effort {})\n  permission mode: {}\n  loops: {}\n  skills: {}\n",
                    self.journal.events().map(|e| e.len()).unwrap_or(0),
                    self.cfg.model_ref,
                    self.effort.as_deref().unwrap_or("default"),
                    format!("{:?}", self.cfg.permissions.mode).to_lowercase(),
                    self.loops.list().len(),
                    self.skills.len()
                ));
                out.push_str(
                    "  (offline checks only — network probes are `loon doctor --connect`)",
                );
                Ok(out)
            }
            SlashCommand::SessionsSearch { query } => {
                if query.trim().is_empty() {
                    return Err("usage: /sessions search <query>".into());
                }
                let store = crate::open_store(&self.loon_paths()).map_err(|e| e.to_string())?;
                let sessions = store.list_sessions().map_err(|e| e.to_string())?;
                let needle = query.to_lowercase();
                let mut hits = Vec::new();
                for meta in &sessions {
                    let Ok(events) = store.events(meta.id) else {
                        continue;
                    };
                    for e in &events {
                        if let loon_events::model::EventKind::UserMessage { text } = &e.kind {
                            if text.to_lowercase().contains(&needle) {
                                hits.push(format!(
                                    "  {} · {} · {}",
                                    // Same uniqueness rule as /resume: a fixed 8-char
                                    // slice collides for sessions started moments apart.
                                    unique_id_prefix(
                                        meta.id,
                                        &sessions.iter().map(|s| s.id).collect::<Vec<_>>()
                                    ),
                                    // A hit is one line: legacy titles may still hold
                                    // newlines, which would forge extra results.
                                    one_line_title(&meta.title),
                                    truncate_prompt(text.trim(), 60)
                                ));
                                break;
                            }
                        }
                    }
                    if hits.len() >= 20 {
                        break;
                    }
                }
                if hits.is_empty() {
                    Ok(format!("no sessions match `{query}`"))
                } else {
                    Ok(format!(
                        // The hits are shown as id prefixes, not as a numbered list, and
                        // `/resume <n>` indexes the *unfiltered* session list — pointing
                        // at `<n>` here would send the user to a different session.
                        "{} session(s) matching `{query}` (most recently used first, /resume <id-prefix> to open):\n{}",
                        hits.len(),
                        hits.join("\n")
                    ))
                }
            }
            SlashCommand::Shadow { path } => {
                if path.trim().is_empty() {
                    return Err("usage: /shadow <path>".into());
                }
                let events = self.events()?;
                let mut out = format!("mutation history for {path}:\n");
                let mut found = 0usize;
                for e in &events {
                    if let loon_events::model::EventKind::FileMutated {
                        path: p,
                        tool,
                        before_sha256,
                        after_sha256,
                        ..
                    } = &e.kind
                    {
                        if !p.ends_with(path.trim()) {
                            continue;
                        }
                        found += 1;
                        let action = if before_sha256.is_empty() {
                            "created"
                        } else {
                            "modified"
                        };
                        out.push_str(&format!(
                            "  seq {} {action} by {tool} (before {}, after {})\n",
                            e.seq,
                            &before_sha256[..8.min(before_sha256.len())],
                            &after_sha256[..8.min(after_sha256.len())]
                        ));
                    }
                }
                if found == 0 {
                    return Ok(format!("no mutations recorded for `{path}`"));
                }
                out.push_str("restore with /undo (newest first) or /rewind — every state above is CAS-snapshotted");
                Ok(out)
            }
            SlashCommand::ShadowRestore { seq, path } => {
                if path.trim().is_empty() || seq == 0 {
                    return Err("usage: /shadow-restore <seq> <path> — see /shadow <path>".into());
                }
                let events = self.events()?;
                // find the mutation at/before seq for this path, take its before-blob
                let target = events.iter().rev().find_map(|e| match &e.kind {
                    loon_events::model::EventKind::FileMutated {
                        path: p,
                        before_blob,
                        before_sha256,
                        ..
                    } if p.ends_with(path.trim()) && e.seq <= seq => {
                        Some((before_blob.clone(), before_sha256.clone()))
                    }
                    _ => None,
                });
                let Some((Some(blob), before_sha)) = target else {
                    return Err(format!(
                        "no snapshotted state before seq {seq} for `{path}` (creations have no prior state)"
                    ));
                };
                let content = self
                    .journal
                    .read_blob(&loon_events::model::BlobRef {
                        sha256: blob.sha256.clone(),
                        bytes: blob.bytes,
                    })
                    .map_err(|e| format!("blob read: {e}"))?;
                let p = self.ctx.ws.root().join(path.trim());
                std::fs::write(&p, &content).map_err(|e| format!("write: {e}"))?;
                Ok(format!(
                    "restored `{}` to its pre-seq-{seq} state (sha {}, {} bytes) — journaled on your next edit",
                    path.trim(),
                    &before_sha[..8.min(before_sha.len())],
                    content.len()
                ))
            }
            SlashCommand::Mcp { args } => self.mcp_command(args).await,
            SlashCommand::Workflows => {
                self.skills = crate::skills::load_with_builtins(&self.data_dir, self.ctx.ws.root());
                if self.skills.is_empty() {
                    return Ok(
                        "no workflows yet — drop a <name>.md into .loon/skills/ or the global skills dir"
                            .into(),
                    );
                }
                if self.events.is_some() {
                    let items = self
                        .skills
                        .iter()
                        .map(|skill| crate::catalog::MenuItem {
                            label: format!("/{}", skill.name),
                            detail: format!("{} · {}", skill.scope, skill.path.display()),
                            command: format!("/{}", skill.name),
                        })
                        .collect();
                    self.emit(crate::AgentEvent::OpenMenu {
                        title: "Workflows".into(),
                        items,
                    });
                    return Ok(String::new());
                }
                let mut out = String::from("workflows (skills):\n");
                for skill in &self.skills {
                    out.push_str(&format!(
                        "  /{} ({}) — {}\n",
                        skill.name,
                        skill.scope,
                        skill.path.display()
                    ));
                }
                Ok(out)
            }
            SlashCommand::AutoCompact { setting } => match setting.as_str() {
                "on" | "off" => self.config_apply("autocompact", &setting),
                "toggle" => {
                    let mode = if !self.autocompact {
                        "on"
                    } else {
                        "off"
                    };
                    self.config_apply("autocompact", mode)
                }
                "auto" => {
                    self.autocompact_percent = None;
                    self.autocompact_tokens = None;
                    let _ = self.config_apply("autocompact_percent", "0");
                    let _ = self.config_apply("autocompact_tokens", "0");
                    Ok(format!(
                        "autocompact back to the default {}% of the window (~{} tokens)",
                        (crate::compaction::COMPACT_THRESHOLD * 100.0) as u32,
                        self.compaction_trigger_tokens()
                    ))
                }
                _ => {
                    use loon_config::tokens::{format_tokens, parse_token_spec, TokenSpec};
                    let spec = parse_token_spec(setting.as_str()).ok_or(
                        "usage: /autocompact 50000|50k|1m|90%|on|off|auto (minimum 1024 tokens)",
                    )?;
                    match spec {
                        TokenSpec::Percent(p) => {
                            let result = self.config_apply("autocompact_percent", &p.to_string())?;
                            self.config_apply("autocompact", "on")?;
                            Ok(format!(
                                "{result}; effective trigger ~{} tokens",
                                format_tokens(self.compaction_trigger_tokens())
                            ))
                        }
                        TokenSpec::Tokens(tokens) => {
                            if tokens < 1024 {
                                return Err(
                                    "usage: /autocompact 50000|50k|1m|90%|on|off|auto (minimum 1024 tokens)"
                                        .into(),
                                );
                            }
                            let result =
                                self.config_apply("autocompact_tokens", &tokens.to_string())?;
                            if !self.autocompact {
                                self.config_apply("autocompact", "on")?;
                            }
                            Ok(format!(
                                "{result}; effective trigger ~{} estimated tokens",
                                format_tokens(self.compaction_trigger_tokens())
                            ))
                        }
                    }
                }
            },
            SlashCommand::Effort { level } => {
                if level.is_empty() {
                    // bare /effort: interactive hosts open the picker; headless lists
                    if self.events.is_some() {
                        self.emit(crate::AgentEvent::OpenEffortPicker);
                        return Ok(String::new());
                    }
                    return Ok("usage: /effort low|medium|high|xhigh|ultra".into());
                }
                let effort = loon_config::Effort::parse(&level)
                    .ok_or("effort must be low, medium, high, xhigh or ultra")?;
                let applied = self.config_apply("effort", &level)?;
                Ok(format!("{applied}; output budget {} tokens (capped by configured model); provider reasoning only when supported",effort.output_tokens()))
            }

            SlashCommand::Goal { text } => {
                if text.trim().is_empty() || text.trim() == "clear" {
                    self.worklog
                        .update(|s| {
                            s.goal = None;
                            Ok(())
                        })
                        .map_err(|e| e.to_string())?;
                    let had = self.session_goal.take();
                    return Ok(match had {
                        Some(g) => format!("goal cleared (was: {g})"),
                        None => "no goal set".into(),
                    });
                }
                self.worklog
                    .update(|s| {
                        s.goal = Some(text.clone());
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                self.session_goal = Some(text.clone());
                Ok(format!("goal set: {text}"))
            }
            SlashCommand::Btw { text } => {
                if text.trim().is_empty() {
                    return Err("usage: /btw <context the model should know>".into());
                }
                let _ = self.journal.append(
                    None,
                    loon_events::model::EventKind::UserMessage {
                        text: format!("[btw] {text}"),
                    },
                );
                self.flush_worklog_if_durable();
                self.history
                    .push(loon_providers::ChatMessage::user(format!("[btw] {text}")));
                Ok(format!("noted (will be part of context): {text}"))
            }
            SlashCommand::Recap => {
                if self.history.is_empty() {
                    return Ok("nothing to recap yet".into());
                }
                let transcript = crate::compaction::render_transcript(&self.history);
                let summarizer = match &self.summarizer {
                    Some(s) => s.clone(),
                    None => crate::compaction::model_summarizer(
                        self.registry.clone(),
                        self.cfg.model_ref.clone(),
                    ),
                };
                let summary = match summarizer(transcript).await {
                    Ok(summary) if !summary.trim().is_empty() => summary,
                    Ok(_) => {
                        return Ok(
                            "recap failed: the summarizer returned no text — nothing compacted"
                                .into(),
                        );
                    }
                    Err(reason) => {
                        return Ok(format!(
                            "recap failed: {reason} — nothing compacted (/model REF picks another summarizer)"
                        ));
                    }
                };
                Ok(format!("session recap (nothing compacted):\n{summary}"))
            }
            SlashCommand::Loop { interval, prompt } => {
                if prompt.trim().is_empty() {
                    return Err("usage: /loop <interval> <prompt> — e.g. /loop 1m check CI".into());
                }
                let dur = crate::loops::parse_interval(&interval)
                    .map_err(|e| format!("bad interval `{interval}`: {e}"))?;
                let id = self.loops.create(dur, prompt);
                Ok(format!(
                    "loop #{id} scheduled every {interval} — /loops lists, /loopcancel {id} stops it"
                ))
            }
            SlashCommand::Loops => {
                if self.loops.is_empty() {
                    return Ok("no loop tasks (create with /loop <interval> <prompt>)".into());
                }
                if self.events.is_some() {
                    let items = self
                        .loops
                        .list()
                        .into_iter()
                        .map(
                            |(id, interval, remaining, prompt)| crate::catalog::MenuItem {
                                label: format!("#{id} every {}s", interval.as_secs()),
                                detail: format!("{remaining}s · {}", truncate_prompt(&prompt, 60)),
                                command: format!("/loopcancel {id}"),
                            },
                        )
                        .collect();
                    self.emit(crate::AgentEvent::OpenMenu {
                        title: "Loop tasks".into(),
                        items,
                    });
                    return Ok(String::new());
                }
                let mut out = String::from("loop tasks:\n");
                for (id, interval, remaining, prompt) in self.loops.list() {
                    out.push_str(&format!(
                        "  #{id}: every {}s (next in {remaining}s) — {}\n",
                        interval.as_secs(),
                        truncate_prompt(&prompt, 60)
                    ));
                }
                Ok(out)
            }
            SlashCommand::LoopCancel { id } => {
                if self.loops.cancel(id) {
                    Ok(format!("loop #{id} cancelled"))
                } else {
                    Err(format!("no loop task #{id} — /loops lists live ones"))
                }
            }
            SlashCommand::Quit => unreachable!("quit handled by the caller"),
        }
    }

    fn open_settings_menu(&self) {
        let mut items: Vec<_> = settings_registry()
            .iter()
            .map(|d| crate::catalog::MenuItem {
                label: format!("{} = {}", d.key, self.setting_value(d.key)),
                detail: d.description.into(),
                command: if d.key == "model" {
                    "/model".into()
                } else if d.key == "theme" {
                    "/theme".into()
                } else {
                    format!("/config edit {}", d.key)
                },
            })
            .collect();
        items.insert(0, crate::catalog::MenuItem { label: "All configuration".into(), detail: "Browse every config field, lists and named entries; stage, validate and save changes".into(), command: "/config ui open".into() });
        items.insert(
            1,
            crate::catalog::MenuItem {
                label: "Context7".into(),
                detail: self.context7_status(),
                command: format!("/config ui open {}", crate::config_ui::encode("/context7")),
            },
        );
        self.emit(crate::AgentEvent::OpenMenu {
            title: "Settings".into(),
            items,
        });
    }

    /// /config — bare opens the settings menu; `/config <key>` shows one setting;
    /// `/config <key>=<value>`, `/config <key> <value>` and `/config set <key> <value>`
    /// validate against the registry, apply live and persist. Unknown keys are refused
    /// with the list of valid ones.
    async fn run_config_command(&mut self, args: Vec<String>) -> Result<String, String> {
        let mut args = args;
        if args.first().is_some_and(|a| a == "ui") {
            return self
                .config_ui_command(args.into_iter().skip(1).collect())
                .await;
        }
        let from_menu = args.first().map(String::as_str) == Some("menu-set");
        if from_menu {
            args.remove(0);
        }
        if args.first().map(String::as_str) == Some("set") {
            args.remove(0);
        }
        if args.is_empty() {
            if self.events.is_none() {
                return Ok(self.config_report());
            }
            self.open_settings_menu();
            return Ok(String::new());
        }
        if args[0] == "edit" {
            let key = args.get(1).ok_or("missing setting")?;
            let def = setting_def(key).ok_or("unknown setting")?;
            let choices = def.kind.suggestions();
            self.emit(crate::AgentEvent::OpenEditor(crate::catalog::Editor {
                kind: "setting".into(),
                values: vec![key.clone(), self.setting_value(key)],
                editing: true,
                fields: vec![
                    crate::catalog::FieldSpec::text(),
                    if choices.is_empty() {
                        crate::catalog::FieldSpec::text()
                    } else {
                        crate::catalog::FieldSpec::choice(choices)
                    },
                ],
            }));
            return Ok(String::new());
        }
        let (key, inline) = match args[0].split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (args[0].clone(), None),
        };
        let def = setting_def(&key).ok_or_else(|| {
            format!(
                "unknown setting `{key}` — valid keys: {}",
                settings_registry()
                    .iter()
                    .map(|d| d.key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let rest = &args[1..];
        if inline.is_none() && rest.is_empty() {
            return Ok(format!(
                "{key} = {} ({})",
                self.setting_value(&key),
                def.description
            ));
        }
        let value = match inline {
            Some(v) if rest.is_empty() => v,
            Some(v) => format!("{v} {}", rest.join(" ")),
            None => {
                let joined = rest.join(" ");
                joined
                    .strip_prefix('=')
                    .unwrap_or(&joined)
                    .trim()
                    .to_string()
            }
        };
        let result = self.config_apply(&key, &value)?;
        if from_menu {
            self.open_settings_menu();
            Ok(String::new())
        } else {
            Ok(result)
        }
    }

    /// Current value of one setting, as text.
    pub fn setting_value(&self, key: &str) -> String {
        match key {
            "ultraloop" => self
                .worklog
                .read()
                .map(|s| s.ultraloop)
                .unwrap_or(false)
                .to_string(),
            "ultraloop_limit" => self
                .worklog
                .read()
                .map(|s| s.ultraloop_limit)
                .unwrap_or(0)
                .to_string(),
            "supervisor_model" => self
                .worklog
                .read()
                .ok()
                .and_then(|s| s.supervisor_model)
                .unwrap_or_default(),
            "plan_mode" => self
                .worklog
                .read()
                .map(|s| s.plan_mode)
                .unwrap_or(false)
                .to_string(),
            "session_goal" => self.session_goal.clone().unwrap_or_default(),
            "context7_key_env" => {
                loon_config::Config::load_effective(&self.loon_paths(), self.ctx.ws.root())
                    .map(|(c, _)| c.context7.api_key_env)
                    .unwrap_or("CONTEXT7_API_KEY".into())
            }
            "check_command" => self.cfg.check_command.clone().unwrap_or_default(),
            "ultra_rounds" => self.cfg.ultra_rounds.to_string(),
            "max_retries" => self.cfg.max_retries.to_string(),
            "animations" => loon_config::Config::load_or_builtin(&self.loon_paths())
                .ok()
                .map(|(c, _)| c.ui.animations)
                .unwrap_or(true)
                .to_string(),
            "popup_rows" => loon_config::Config::load_or_builtin(&self.loon_paths())
                .ok()
                .map(|(c, _)| c.ui.popup_rows)
                .unwrap_or(6)
                .to_string(),
            "tips" => self.tips.to_string(),
            "theme" => loon_config::Config::load_or_builtin(&self.loon_paths())
                .ok()
                .map(|(c, _)| c.appearance.palette)
                .unwrap_or("terminal".into()),
            "permission_mode" => format!("{:?}", self.cfg.permissions.mode).to_lowercase(),
            "effort" => self.effort.clone().unwrap_or_else(|| "medium".into()),
            "model" => self.cfg.model_ref.clone(),
            "context_window" => self.cfg.context_window.to_string(),
            "tool_output_bytes" => self.cfg.tool_output_bytes.to_string(),
            "autocompact_tokens" => self.autocompact_tokens.unwrap_or(0).to_string(),
            "autocompact_percent" => self.autocompact_percent.unwrap_or(0).to_string(),
            "autocompact" => if !self.autocompact { "off" } else { "on" }.to_string(),
            _ => String::new(),
        }
    }

    /// Validate against the registry, apply to the live session, persist.
    pub fn config_apply(&mut self, key: &str, value: &str) -> Result<String, String> {
        let def = setting_def(key)
            .ok_or_else(|| format!("unknown setting `{key}` — /config lists valid keys"))?;
        let value = value.trim();
        // Token-sized settings accept the same shorthand as the CLI: `1m`, `100k`.
        let normalized;
        let value = if matches!(key, "context_window" | "autocompact_tokens") {
            match loon_config::tokens::parse_tokens(value) {
                Some(n) => {
                    normalized = n.to_string();
                    normalized.as_str()
                }
                None => {
                    return Err(format!(
                        "{key} expects a token count: 200000, 200k, 1m, or 0 to disable"
                    ))
                }
            }
        } else {
            value
        };
        match &def.kind {
            SettingKind::Bool => {
                if !["true", "false"].contains(&value) {
                    return Err(format!("{key} expects true|false, got `{value}`"));
                }
            }
            SettingKind::Enum(vals) => {
                if !vals.contains(&value) {
                    return Err(format!("{key} expects one of: {}", vals.join(" | ")));
                }
            }
            SettingKind::Int { min, max } => {
                let n: u64 = value
                    .parse()
                    .map_err(|_| format!("{key} expects an integer"))?;
                if n < *min || n > *max {
                    return Err(format!("{key} must be between {min} and {max}"));
                }
            }
            SettingKind::Text => {}
        }
        let applied = match key {
            "ultraloop" | "ultraloop_limit" | "supervisor_model" | "plan_mode" | "session_goal" => {
                if key == "supervisor_model" && !value.is_empty() {
                    self.registry.resolve(value)?;
                }
                let state = self
                    .worklog
                    .update(|s| {
                        match key {
                            "ultraloop" => s.ultraloop = value == "true",
                            "ultraloop_limit" => {
                                s.ultraloop_limit = value.parse().map_err(|_| {
                                    loon_tools::ToolError::InvalidArgs(
                                        "Invalid review limit".into(),
                                    )
                                })?
                            }
                            "supervisor_model" => {
                                s.supervisor_model = (!value.is_empty()).then(|| value.to_string())
                            }
                            "plan_mode" => s.plan_mode = value == "true",
                            "session_goal" => {
                                s.goal = (!value.is_empty()).then(|| value.to_string())
                            }
                            _ => {}
                        }
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                self.session_goal = state.goal.clone();
                self.emit(crate::AgentEvent::WorkStateChanged { state });
                return Ok(format!("{key} updated; saved to this session"));
            }
            "context7_key_env" => {
                if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return Err("use an environment variable name".into());
                }
                let mut cfg = loon_config::Config::load_or_builtin(&self.loon_paths())
                    .map_err(|e| e.to_string())?
                    .0;
                cfg.context7.api_key_env = value.into();
                cfg.save(&self.loon_paths()).map_err(|e| e.to_string())?;
                let effective =
                    loon_config::Config::load_effective(&self.loon_paths(), self.ctx.ws.root())
                        .map_err(|e| e.to_string())?
                        .0;
                self.refresh_context7(&effective.context7);
                return Ok(format!("Context7 key environment = {value}; saved. A key can also be entered in /settings → Context7."));
            }
            "autocompact_tokens" => {
                let n = value.parse::<u64>().map_err(|_| "bad token count")?;
                if n > 0 && n < 1024 {
                    return Err("autocompact_tokens must be 0 or at least 1024".into());
                }
                self.autocompact_tokens = (n > 0).then_some(n);
                // An absolute threshold replaces a percentage trigger.
                if n > 0 {
                    self.autocompact_percent = None;
                }
                format!("autocompact_tokens = {n}")
            }
            "autocompact_percent" => {
                let p: u8 = value
                    .trim_end_matches('%')
                    .parse()
                    .map_err(|_| "autocompact_percent expects 0-99")?;
                if p > 99 {
                    return Err("autocompact_percent must be between 0 and 99".into());
                }
                self.autocompact_percent = (p > 0).then_some(p);
                if p > 0 {
                    self.autocompact_tokens = None;
                }
                format!(
                    "autocompact at {}% of the window ({} tokens)",
                    if p == 0 { 80 } else { p },
                    self.compaction_trigger_tokens()
                )
            }
            "check_command" => {
                self.cfg.check_command = (!value.is_empty()).then(|| value.to_string());
                format!("check_command = {value}")
            }
            "ultra_rounds" => {
                self.cfg.ultra_rounds = value.parse().unwrap();
                format!("ultra_rounds = {value}")
            }
            "max_retries" => {
                self.cfg.max_retries = value.parse().unwrap();
                format!("max_retries = {value}")
            }
            "animations" => {
                self.emit(crate::AgentEvent::UiPreference {
                    key: key.into(),
                    value: value.into(),
                });
                format!("{key} = {value}")
            }
            "popup_rows" => {
                self.emit(crate::AgentEvent::UiPreference {
                    key: key.into(),
                    value: value.into(),
                });
                format!("{key} = {value}")
            }
            "tips" => {
                self.tips = value == "true";
                self.emit(crate::AgentEvent::UiPreference {
                    key: "tips".into(),
                    value: value.into(),
                });
                format!("tips = {}", self.tips)
            }
            "theme" => {
                let name = match value {
                    "transparent" => "terminal",
                    "opaque" => "dark",
                    name => name,
                };
                self.transparent_request = name == "terminal";
                self.emit(crate::AgentEvent::SetPalette { name: name.into() });
                format!("theme = {value}")
            }
            "permission_mode" => {
                let mode = match value {
                    "bypass" => loon_config::PermissionMode::Bypass,
                    "auto" => loon_config::PermissionMode::Auto,
                    "ask" => loon_config::PermissionMode::Ask,
                    _ => loon_config::PermissionMode::Allowlist,
                };
                self.set_permission_mode(mode);
                self.emit(crate::AgentEvent::ModeChanged { mode: value.into() });
                format!("permission_mode = {value}")
            }
            "effort" => {
                self.effort = Some(value.to_string());
                self.worklog
                    .update(|s| {
                        s.effort = Some(value.into());
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                self.emit(crate::AgentEvent::EffortChanged {
                    level: value.into(),
                });
                format!("effort = {value}")
            }
            "model" => {
                self.registry.resolve(value)?;
                return self.choose_model(value);
            }
            "context_window" => {
                self.cfg.context_window = value.parse().unwrap_or(0);
                format!("context_window = {}", self.cfg.context_window)
            }
            "tool_output_bytes" => {
                let n: usize = value.parse().unwrap_or(65_536);
                self.cfg.tool_output_bytes = n;
                self.ctx.output_limit = n;
                format!("tool_output_bytes = {n}")
            }
            "autocompact" => {
                let enable = value == "on";
                self.autocompact = enable;
                self.emit(crate::AgentEvent::UiPreference {
                    key: "autocompact".into(),
                    value: enable.to_string(),
                });
                format!("autocompact = {value}")
            }
            other => return Err(format!("unknown setting `{other}`")),
        };
        if matches!(key, "context_window" | "autocompact") {
            self.emit(crate::AgentEvent::UiPreference {
                key: "context_window".into(),
                value: self.cfg.context_window.to_string(),
            });
        }
        let persisted = self.persist_setting(key, value);
        Ok(match persisted {
            Ok(()) => format!("{applied}; saved to config.toml"),
            Err(e) => format!("{applied}; NOT persisted: {e}"),
        })
    }

    /// Persist one setting through the comment-preserving config save.
    fn persist_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let paths = self.loon_paths();
        let (mut cfg, findings) =
            loon_config::Config::load_or_builtin(&paths).map_err(|e| e.to_string())?;
        if findings.iter().any(|f| f.is_error) {
            return Err("existing config has validation errors; not overwriting".into());
        }
        match key {
            "autocompact_tokens" => {
                let n = value.parse::<u64>().map_err(|_| "bad token count")?;
                cfg.session.autocompact_tokens = (n > 0).then_some(n);
                if n > 0 {
                    cfg.session.autocompact_percent = None;
                }
            }
            "autocompact_percent" => {
                let p: u8 = value
                    .trim_end_matches('%')
                    .parse()
                    .map_err(|_| "bad percent")?;
                cfg.session.autocompact_percent = (p > 0).then_some(p);
                if p > 0 {
                    cfg.session.autocompact_tokens = None;
                }
            }
            "check_command" => {
                cfg.session.check_command = (!value.is_empty()).then(|| value.to_string())
            }
            "ultra_rounds" => {
                cfg.session.ultra_rounds = value.parse().map_err(|_| "bad integer")?
            }
            "max_retries" => cfg.session.max_retries = value.parse().map_err(|_| "bad integer")?,
            "animations" => cfg.ui.animations = value == "true",
            "popup_rows" => cfg.ui.popup_rows = value.parse().map_err(|_| "bad integer")?,
            "tips" => cfg.ui.tips = value == "true",
            "theme" => {
                cfg.appearance.palette = match value {
                    "transparent" => "terminal",
                    "opaque" => "dark",
                    name => name,
                }
                .into();
                cfg.appearance.mode = if value == "transparent" || value == "terminal" {
                    loon_config::AppearanceMode::Transparent
                } else {
                    loon_config::AppearanceMode::Opaque
                }
            }
            "permission_mode" => {
                cfg.permissions.mode = match value {
                    "bypass" => loon_config::PermissionMode::Bypass,
                    "auto" => loon_config::PermissionMode::Auto,
                    "ask" => loon_config::PermissionMode::Ask,
                    _ => loon_config::PermissionMode::Allowlist,
                }
            }
            "model" => cfg.session.model = value.to_string(),
            "context_window" => {
                if let Some(def) = cfg.models.get_mut(&self.cfg.model_ref) {
                    def.context_window = Some(value.parse().map_err(|_| "bad integer")?);
                } else {
                    cfg.session.context_window = Some(value.parse().map_err(|_| "bad integer")?);
                }
            }
            "tool_output_bytes" => {
                cfg.session.tool_output_bytes = value.parse().map_err(|_| "bad integer")?
            }
            "effort" => cfg.session.effort = Some(value.into()),
            "autocompact" => cfg.session.autocompact = value != "off",
            other => return Err(format!("unknown setting {other}")),
        }
        cfg.save(&paths).map_err(|e| e.to_string())
    }

    fn config_report(&self) -> String {
        let p = &self.cfg.permissions;
        format!(
            "effective configuration\n  model: {}\n  context window: {} tokens (autocompact at {})\n  permission mode: {} ({} allow / {} deny rules)\n  tool output cap: {} bytes\n  appearance dir: config at {}",
            self.cfg.model_ref,
            loon_config::tokens::format_tokens(self.cfg.context_window),
            match self.autocompact_percent {
                Some(percent) => format!("{percent}% of the window"),
                None => format!(
                    "{}% of the window",
                    (crate::compaction::COMPACT_THRESHOLD * 100.0) as u32
                ),
            },
            match p.mode {
                loon_config::PermissionMode::Bypass => "bypass",
                loon_config::PermissionMode::Ask => "ask",
                loon_config::PermissionMode::Auto => "auto (reviewer checks calls)",
                loon_config::PermissionMode::Allowlist => "allowlist",
            },
            p.allow_rules.len(),
            p.deny_rules.len(),
            self.cfg.tool_output_bytes,
            self.config_dir.join("config.toml").display(),
        )
    }

    #[allow(dead_code)] // retained: the settings menu will persist through these
    fn config_set(&mut self, key: &str, value: &str) -> Result<String, String> {
        let applied = match key {
            "model" => {
                self.registry.resolve(value)?;
                self.cfg.model_ref = value.to_string();
                "model switched (next request uses it)".to_string()
            }
            "context_window" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| "context_window must be an integer (0 disables compaction)")?;
                self.cfg.context_window = v;
                format!("context window set to {v}")
            }
            "permission_mode" => {
                let mode = match value {
                    "bypass" => loon_config::PermissionMode::Bypass,
                    "ask" => loon_config::PermissionMode::Ask,
                    "auto" => loon_config::PermissionMode::Auto,
                    "allowlist" => loon_config::PermissionMode::Allowlist,
                    other => return Err(format!("unknown permission mode `{other}` (bypass|auto|ask|allowlist)")),
                };
                self.cfg.permissions.mode = mode;
                format!("permission mode set to {value} (rules unchanged)")
            }
            "tool_output_bytes" => {
                let v: usize = value
                    .parse()
                    .map_err(|_| "tool_output_bytes must be an integer")?;
                self.cfg.tool_output_bytes = v;
                self.ctx.output_limit = v;
                format!("tool output cap set to {v} bytes")
            }
            other => {
                return Err(format!(
                    "unknown key `{other}` — keys: model, context_window, permission_mode, tool_output_bytes"
                ))
            }
        };
        let persisted = self.persist_config_value(key, value);
        match persisted {
            Ok(()) => Ok(format!(
                "{applied}; saved to {}",
                self.config_dir.join("config.toml").display()
            )),
            Err(e) => Ok(format!("{applied}; NOT persisted: {e}")),
        }
    }

    /// Persist one scalar to the user's config.toml, preserving comments and unknown
    /// tables (save goes through toml_edit overlay). Session-effective even if this
    /// fails, and the result says which happened.
    #[allow(dead_code)] // retained: the settings menu will persist through these
    fn persist_config_value(&self, key: &str, value: &str) -> Result<(), String> {
        let paths = loon_config::LoonPaths {
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.clone(),
            state_dir: self
                .data_dir
                .parent()
                .map(|p| p.join("state"))
                .unwrap_or_default(),
            cache_dir: self
                .data_dir
                .parent()
                .map(|p| p.join("cache"))
                .unwrap_or_default(),
            runtime_dir: self
                .data_dir
                .parent()
                .map(|p| p.join("runtime"))
                .unwrap_or_default(),
        };
        let (mut cfg, findings) =
            loon_config::Config::load_or_builtin(&paths).map_err(|e| e.to_string())?;
        if !findings.is_empty() {
            // do not overwrite a config the validator flags; report instead
            let msgs: Vec<String> = findings.iter().map(|f| format!("{f:?}")).collect();
            return Err(format!(
                "existing config has validation findings; fix them before persisting: {}",
                msgs.join("; ")
            ));
        }
        match key {
            "model" => cfg.session.model = value.to_string(),
            "context_window" => {
                if let Some(def) = cfg.models.get_mut(&self.cfg.model_ref) {
                    def.context_window = Some(value.parse().map_err(|_| "bad integer")?);
                } else {
                    return Err(format!(
                        "model `{}` is not in [models]; nothing to persist it to",
                        self.cfg.model_ref
                    ));
                }
            }
            "permission_mode" => {
                cfg.permissions.mode = match value {
                    "bypass" => loon_config::PermissionMode::Bypass,
                    "ask" => loon_config::PermissionMode::Ask,
                    "auto" => loon_config::PermissionMode::Auto,
                    "allowlist" => loon_config::PermissionMode::Allowlist,
                    _ => return Err("bad mode".into()),
                };
            }
            "tool_output_bytes" => {
                cfg.session.tool_output_bytes = value.parse().map_err(|_| "bad integer")?
            }
            other => return Err(format!("unknown key {other}")),
        }
        cfg.save(&paths).map_err(|e| e.to_string())
    }

    fn events(&self) -> Result<Vec<loon_events::model::Event>, String> {
        self.journal.events().map_err(|e| format!("journal: {e}"))
    }

    fn memory_paths(&self) -> Result<crate::memory::MemoryPaths, String> {
        Ok(crate::memory::MemoryPaths::resolve(
            &self.paths_data_dir()?,
            self.ctx.ws.root(),
        ))
    }

    fn paths_data_dir(&self) -> Result<std::path::PathBuf, String> {
        Ok(self.data_dir.clone())
    }

    /// /usage: per-day request and token totals from RequestFinished events, rendered
    /// GitHub-activity-style (last 8 weeks; intensity by quartile). Empty days are
    /// ░ — shown as-is, not hidden.
    pub fn usage_report(&self) -> String {
        let Ok(events) = self.journal.events() else {
            return "journal unreadable".into();
        };
        let mut per_day: std::collections::BTreeMap<String, (u64, u64)> =
            std::collections::BTreeMap::new(); // date → (requests, tokens)
        for e in &events {
            if let loon_events::model::EventKind::RequestFinished {
                usage,
                usage_reported,
                ..
            } = &e.kind
            {
                let date = chrono::DateTime::from_timestamp_millis(e.ts_ms)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let tokens = if *usage_reported {
                    usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0)
                } else {
                    0
                };
                per_day
                    .entry(date)
                    .and_modify(|r| {
                        r.0 += 1;
                        r.1 += tokens;
                    })
                    .or_insert((1, tokens));
            }
        }
        if per_day.is_empty() {
            return "no usage recorded yet (usage appears after provider requests)".into();
        }
        let max: u64 = per_day.values().map(|(_, t)| *t).max().unwrap_or(1);
        let shades = ['░', '▒', '▓', '█'];
        let tile = |tokens: u64| -> char {
            if tokens == 0 {
                shades[0]
            } else {
                let q = ((tokens as f64 / max as f64) * 3.0).ceil() as usize;
                shades[q.clamp(1, 3)]
            }
        };
        // last 8 weeks as columns of 7 days (weeks end today)
        let today = chrono::Utc::now().date_naive();
        let mut out = String::from("usage by day (░▒▓█ by quartile of the busiest day):\n\n");
        for week in (0..8).rev() {
            let mut row = String::new();
            for day in (0..7).rev() {
                let date = today - chrono::Duration::weeks(week) - chrono::Duration::days(day);
                let key = date.format("%Y-%m-%d").to_string();
                let (requests, tokens) = per_day.get(&key).copied().unwrap_or((0, 0));
                row.push(if requests > 0 { tile(tokens) } else { '·' });
            }
            let week_start = today - chrono::Duration::weeks(week) - chrono::Duration::days(6);
            out.push_str(&format!("  {} {}\n", week_start.format("%m-%d"), row));
        }
        out.push_str("\nday totals (requests · est./reported tokens):\n");
        for (date, (requests, tokens)) in per_day.iter().rev().take(7) {
            out.push_str(&format!("  {date}: {requests} · {tokens}\n"));
        }
        out
    }

    /// `/context <size>`: one command to change the window. Accepts `1m`, `100k`,
    /// `128000`, `off` (disables compaction) and `90%` (compact at 90% of the window).
    pub fn set_context_size(&mut self, size: &str) -> Result<String, String> {
        use loon_config::tokens::{format_tokens, parse_token_spec, TokenSpec};
        match size.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => {
                self.config_apply("autocompact", "off")?;
                return Ok("automatic compaction off; context window retained".into());
            }
            "auto" => {
                self.config_apply("autocompact_percent", "0")?;
                self.config_apply("autocompact_tokens", "0")?;
                return Ok(format!(
                    "autocompact back to the default {}% of the window",
                    (crate::compaction::COMPACT_THRESHOLD * 100.0) as u32
                ));
            }
            _ => {}
        }
        match parse_token_spec(size) {
            Some(TokenSpec::Percent(p)) => {
                self.config_apply("autocompact_percent", &p.to_string())?;
                Ok(format!(
                    "autocompact at {p}% of the window (now ~{} of {} tokens)",
                    format_tokens(self.compaction_trigger_tokens()),
                    format_tokens(self.cfg.context_window)
                ))
            }
            Some(TokenSpec::Tokens(n)) => {
                if n > 0 && n < 1024 {
                    return Err("context window must be 0 (off) or at least 1024 tokens".into());
                }
                self.config_apply("context_window", &n.to_string())?;
                Ok(format!(
                    "context window = {} ({n} tokens); autocompact at ~{}",
                    format_tokens(n),
                    format_tokens(self.compaction_trigger_tokens())
                ))
            }
            None => Err(
                "usage: /context [1m|200k|128000|90%|off|auto] — no argument shows the report"
                    .into(),
            ),
        }
    }

    /// /context: where the window stands, with a usage bar. Estimates are labeled as
    /// estimates everywhere they appear.
    pub fn context_report(&self) -> String {
        use loon_config::tokens::format_tokens;
        let est = self.estimated_context_tokens();
        let window = self.cfg.context_window;
        let bar = if window > 0 {
            let width = 24usize;
            let frac = (est as f64 / window as f64).clamp(0.0, 1.0);
            let filled = (frac * width as f64).round() as usize;
            format!(
                "[{}{}] {:.0}%",
                "█".repeat(filled),
                "░".repeat(width - filled),
                frac * 100.0
            )
        } else {
            String::new()
        };
        let pct = if window > 0 {
            format!(" ({:.*}% of window)", 1, 100.0 * est as f64 / window as f64)
        } else {
            String::new()
        };
        let mut roles = std::collections::BTreeMap::new();
        for m in &self.history {
            *roles.entry(format!("{:?}", m.role)).or_insert(0usize) += 1;
        }
        let roles: Vec<String> = roles.into_iter().map(|(k, v)| format!("{k}×{v}")).collect();
        let trigger = match self.autocompact_percent {
            Some(p) if (1..=99).contains(&p) => format!("{p}% of the window"),
            _ => format!(
                "{}% of the window",
                (crate::compaction::COMPACT_THRESHOLD * 100.0) as u32
            ),
        };
        format!(
            "context: ~{} est. tokens {bar}{pct}\n{} message(s) [{}]\nwindow {} · autocompact at {trigger} = {} tokens; {}\nchange it with /context 1m, /context 200k or /context 90%",
            format_tokens(est),
            self.history.len(),
            roles.join(" "),
            if window > 0 { format_tokens(window) } else { "off".into() },
            format_tokens(self.compaction_trigger_tokens()),
            if !self.autocompact || window == 0 {
                "compaction disabled".to_string()
            } else if self.should_compact() {
                "compaction will trigger before the next request".to_string()
            } else {
                "no compaction needed".to_string()
            },
        )
    }

    /// /diff: files this session mutated, from FileMutated events (latest entry per
    /// path wins). Byte-level before/after sizes; contents live in the blob store.
    pub fn diff_report(&self) -> String {
        let Ok(events) = self.journal.events() else {
            return "journal unreadable".into();
        };
        let mut latest: std::collections::BTreeMap<String, &loon_events::model::Event> =
            std::collections::BTreeMap::new();
        for e in &events {
            if let EventKind::FileMutated { path, .. } = &e.kind {
                latest.insert(path.clone(), e);
            }
        }
        if latest.is_empty() {
            return "no files changed in this session".into();
        }
        let mut out = String::from("changed this session:\n");
        for (path, e) in latest {
            if let EventKind::FileMutated { before_sha256, .. } = &e.kind {
                let action = if before_sha256 == &loon_workspace::EMPTY_HASH.to_string() {
                    "created"
                } else {
                    "modified"
                };
                out.push_str(&format!("  {action} {path}\n"));
            }
        }
        out.push_str("(use /undo n or /rewind to revert; hashes in the journal)");
        out
    }
}

use loon_events::model::EventKind;

impl Agent {
    async fn run_terminal_command(
        &mut self,
        tool: &str,
        args: Vec<String>,
    ) -> Result<String, String> {
        use serde_json::json;
        let action = args.first().map(String::as_str).unwrap_or("list");
        let value = match (tool, action) {
            ("shell_session", "list") => json!({"action":"list"}),
            ("shell_session", "start" | "close" | "cwd") => json!({"action":action,"name":args.get(1).map(String::as_str).unwrap_or("main")}),
            ("shell_session", "run") if args.len() >= 3 => json!({"action":"run","name":args[1],"command":args[2..].join(" ")}),
            ("monitor", "list") => json!({"action":"list"}),
            ("monitor", "start") if args.len() > 1 => json!({"action":"start","command":args[1..].join(" ")}),
            ("monitor", "logs" | "poll" | "stop" | "remove") => json!({"action":action,"monitor_id":args.get(1).and_then(|s|s.parse::<u64>().ok()).ok_or("provide a numeric monitor ID")?}),
            ("process", "list") => json!({"action":"list"}),
            ("process", "poll" | "stop") => json!({"action":action,"job_id":args.get(1).and_then(|s|s.parse::<u64>().ok()).ok_or("provide a numeric process ID")?}),
            _ => return Err("usage: /shell list|start NAME|run NAME COMMAND|cwd NAME|close NAME; /monitors list|start COMMAND|logs ID|poll ID|stop ID|remove ID; /jobs list|poll ID|stop ID".into()),
        };
        let spec = self
            .tools
            .get(tool)
            .ok_or("terminal tool is unavailable")?
            .spec();
        loon_tools::validate_args(&spec, &value).map_err(|e| e.to_string())?;
        // An idle cancel signal must not cancel this explicitly requested command.
        let _ = self.cancel_tx.send(false);
        let call = loon_events::EvId::now_v7();
        let request = loon_events::EvId::now_v7();
        let _ = self.journal.append(
            Some(request),
            loon_events::model::EventKind::ToolCallObserved {
                request,
                call,
                wire_id: call.to_string(),
                name: tool.into(),
                arguments_json: value.to_string(),
            },
        );
        let output = self
            .execute_tool(
                &call,
                &loon_providers::ToolCallReq {
                    id: call.to_string(),
                    name: tool.into(),
                    arguments_json: value.to_string(),
                },
            )
            .await;
        if let Ok(events) = self.journal.events() {
            self.history = crate::replay::replay_history(&events, None);
        }
        Ok(output.model_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_args() {
        assert_eq!(parse("/help"), Some(Ok(SlashCommand::Help)));
        assert_eq!(parse("  /undo  "), Some(Ok(SlashCommand::Undo)));
        assert_eq!(
            parse("/rewind 3 both"),
            Some(Ok(SlashCommand::Rewind {
                n: 3,
                mode: Some("both".into())
            }))
        );
        assert_eq!(
            parse("/rewind workspace"),
            Some(Ok(SlashCommand::Rewind {
                n: 1,
                mode: Some("workspace".into())
            }))
        );
        assert_eq!(
            parse("/memory global remember this"),
            Some(Ok(SlashCommand::Memory {
                scope: Some("global".into()),
                text: Some("remember this".into())
            }))
        );
        assert_eq!(
            parse("/memory"),
            Some(Ok(SlashCommand::Memory {
                scope: None,
                text: None
            }))
        );
        assert_eq!(
            parse("/model"),
            Some(Ok(SlashCommand::Model { reference: None }))
        );
        // unknown names parse as Skill and are refused at execution time
        assert_eq!(
            parse("/nope"),
            Some(Ok(SlashCommand::Skill {
                name: "nope".into(),
                args: String::new()
            }))
        );
        assert_eq!(
            parse("/ship now please"),
            Some(Ok(SlashCommand::Skill {
                name: "ship".into(),
                args: "now please".into()
            }))
        );
        assert_eq!(parse("plain text"), None);
    }

    #[test]
    fn help_lists_every_implemented_command() {
        let h = help_text();
        for cmd in [
            "help",
            "undo",
            "redo",
            "rewind",
            "checkpoint",
            "fork",
            "compact",
            "context",
            "diff",
            "memory",
            "model",
            "quit",
        ] {
            assert!(h.contains(&format!("/{cmd}")), "help missing /{cmd}");
        }
    }
}
