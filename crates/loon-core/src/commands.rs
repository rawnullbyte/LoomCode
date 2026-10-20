//! Slash commands: one parser shared by the TUI and headless surfaces, and one
//! executor on [`Agent`]. Commands with real machinery execute it; anything not built
//! says so explicitly instead of pretending.

use crate::rewind::RewindMode;
use crate::Agent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
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
    Context,
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
    /// /new — abandon this session, start fresh (journal kept).
    New,
    /// /resume [n|id] — list sessions or switch to one.
    Resume {
        target: Option<String>,
    },
    /// /rename <title> — rename this session.
    Rename {
        title: String,
    },
    /// /theme — cycle transparent/opaque.
    Theme,
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
    /// /mcp — list connected MCP tools.
    Mcp,
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
        "context" => SlashCommand::Context,
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
        "config" => SlashCommand::Config {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "new" => SlashCommand::New,
        "resume" => SlashCommand::Resume {
            target: (!args.is_empty()).then(|| args.join(" ")),
        },
        "rename" => SlashCommand::Rename {
            title: args.join(" "),
        },
        "theme" => SlashCommand::Theme,
        "usage" => SlashCommand::Usage,
        "init" => SlashCommand::Init,
        "provider" => SlashCommand::Provider {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "combo" => SlashCommand::Combo {
            args: args.iter().map(|s| s.to_string()).collect(),
        },
        "mcp" => SlashCommand::Mcp,
        "workflows" => SlashCommand::Workflows,
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

/// The canonical command table: (name, one-line description). Single source of truth
/// for `/help` and the TUI autocomplete — a command listed here must parse.
pub fn command_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("help", "this list"),
        ("undo", "undo the newest file mutation"),
        ("redo", "redo the newest undone mutation"),
        (
            "rewind [n] [mode]",
            "rewind to n-th previous request boundary",
        ),
        ("checkpoint NAME", "mark a restore point in the journal"),
        ("fork NAME", "fork the current session"),
        ("compact", "summarize older context now"),
        ("context", "context window usage report"),
        ("diff", "files this session changed (from the journal)"),
        (
            "memory [global|workspace] [text]",
            "show or save persistent memory",
        ),
        ("model [REF]", "show or switch model/combos:NAME"),
        ("config [set K V]", "show or change + persist configuration"),
        (
            "provider add NAME URL [api] [key_env]",
            "add an OpenAI-compatible provider",
        ),
        ("combo add NAME provider:model ...", "define a model combo"),
        ("mcp", "list connected MCP tools"),
        ("workflows", "list user-defined skills (workflows)"),
        (
            "autocompact on|off|toggle",
            "enable/disable context compaction",
        ),
        ("effort low|medium|high", "output budget per request"),
        ("goal <text|clear>", "session goal, kept in context"),
        ("btw <text>", "record context without triggering a turn"),
        ("recap", "summarize the session so far"),
        (
            "loop <interval> <prompt>",
            "run a prompt on an interval, e.g. /loop 1m check CI",
        ),
        ("loops", "list loop tasks"),
        ("loopcancel <id>", "cancel a loop task"),
        ("quit", "exit"),
    ]
}

fn truncate_prompt(p: &str, max: usize) -> String {
    if p.chars().count() <= max {
        p.to_string()
    } else {
        p.chars().take(max).collect::<String>() + "…"
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

impl Agent {
    /// Execute one slash command, returning a human-readable result block.
    /// `/quit` is handled by the caller (it owns the event loop).
    pub async fn run_slash(&mut self, cmd: SlashCommand) -> Result<String, String> {
        match cmd {
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
                Ok(format!(
                    "rewound {n} boundary(ies) ({}): history now {} message(s){}",
                    mode_str,
                    outcome.history_len_after,
                    outcome
                        .restore
                        .as_ref()
                        .map(|r| format!(", {r}"))
                        .unwrap_or_default()
                ))
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
            SlashCommand::Compact => {
                if self.compact_now().await {
                    Ok("context compacted".into())
                } else {
                    Ok("nothing to compact: only recent exchanges in context".into())
                }
            }
            SlashCommand::Context => Ok(self.context_report()),
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
                    self.emit(crate::AgentEvent::OpenModelPicker {
                        refs: self.registry.list_refs(),
                    });
                    Ok(String::new())
                }
                None => Ok(format!(
                    "current model: {} (context window: {} tokens)",
                    self.cfg.model_ref, self.cfg.context_window
                )),
                Some(r) => {
                    self.registry.resolve(&r)?;
                    self.cfg.model_ref = r.clone();
                    Ok(format!(
                        "model switched to {r} (takes effect on the next request)"
                    ))
                }
            },
            SlashCommand::Skill { name, args } => {
                let Some(skill) = self.skills.iter().find(|s| s.name == name).cloned() else {
                    return Err(format!("unknown command `/{name}` — try /help"));
                };
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
                Ok(
                    "new session started (the old one stays in the journal — /resume to return)"
                        .into(),
                )
            }
            SlashCommand::Resume { target } => {
                let paths = self.loon_paths();
                let store = crate::open_store(&paths).map_err(|e| e.to_string())?;
                let sessions = store.list_sessions().map_err(|e| e.to_string())?;
                if sessions.is_empty() {
                    return Ok("no sessions yet".into());
                }
                let Some(target) = target else {
                    let mut out =
                        String::from("sessions (newest first — /resume <n> to switch):\n");
                    for (i, meta) in sessions.iter().take(10).enumerate() {
                        out.push_str(&format!(
                            "  {}: {} · {} · {}\n",
                            i + 1,
                            &meta.id.to_string()[..8],
                            meta.title,
                            meta.cwd
                        ));
                    }
                    return Ok(out);
                };
                let picked = match target.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= sessions.len() => sessions[n - 1].id,
                    _ => target
                        .parse()
                        .map_err(|_| format!("no session matching `{target}` — /resume lists"))?,
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
            SlashCommand::Theme => {
                self.emit(crate::AgentEvent::ToggleTheme);
                Ok("theme cycled".into())
            }
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
            SlashCommand::Provider { args } => match args.first().map(|s| s.as_str()) {
                Some("list") => {
                    let mut out = String::new();
                    for (name, def) in self.providers_snapshot() {
                        out.push_str(&format!(
                            "  {name}: {} ({})\n",
                            def.base_url,
                            def.api.as_str()
                        ));
                    }
                    Ok(if out.is_empty() {
                        "no providers configured".into()
                    } else {
                        out
                    })
                }
                Some("add") => {
                    let name = args
                        .get(1)
                        .ok_or("usage: /provider add NAME URL [api] [key_env]")?;
                    let url = args.get(2).ok_or("missing BASE_URL")?;
                    let api = args.get(3).map(|s| s.as_str()).unwrap_or("chat");
                    let key_env = args.get(4).cloned();
                    self.persist_provider(name, url, api, key_env.as_deref())
                }
                _ => Err("usage: /provider add NAME URL [api] [key_env] | /provider list".into()),
            },
            SlashCommand::Combo { args } => match args.first().map(|s| s.as_str()) {
                Some("list") => {
                    let mut out = String::new();
                    for (name, members) in self.combos_snapshot() {
                        out.push_str(&format!("  {name}: {members}\n"));
                    }
                    Ok(if out.is_empty() {
                        "no combos configured".into()
                    } else {
                        out
                    })
                }
                Some("add") => {
                    let name = args
                        .get(1)
                        .ok_or("usage: /combo add NAME provider:model ...")?;
                    let members: Vec<&str> = args
                        .get(2..)
                        .map(|a| a.iter().map(|s| s.as_str()).collect())
                        .unwrap_or_default();
                    if members.is_empty() {
                        return Err("at least one provider:model member required".into());
                    }
                    self.persist_combo(name, &members)
                }
                _ => Err("usage: /combo add NAME provider:model ... | /combo list".into()),
            },
            SlashCommand::Mcp => {
                let mut out = String::new();
                for spec in self.tools.specs() {
                    if spec.name.starts_with("mcp_") {
                        out.push_str(&format!("  {} — {}\n", spec.name, spec.description));
                    }
                }
                if out.is_empty() {
                    Ok("no MCP tools connected (configure [mcp.servers] and restart)".into())
                } else {
                    Ok(format!("connected MCP tools:\n{out}"))
                }
            }
            SlashCommand::Workflows => {
                if self.skills.is_empty() {
                    return Ok(
                        "no workflows yet — drop a <name>.md into .loon/skills/ or the global skills dir"
                            .into(),
                    );
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
            SlashCommand::AutoCompact { setting } => {
                let enable = match setting.as_str() {
                    "on" => true,
                    "off" => false,
                    "toggle" => self.cfg.context_window == 0,
                    other => return Err(format!("unknown setting `{other}` — on|off|toggle")),
                };
                self.cfg.context_window = if enable { 128_000 } else { 0 };
                Ok(format!(
                    "autocompact {} (session only — /config set context_window N to persist)",
                    if enable { "on" } else { "off" }
                ))
            }
            SlashCommand::Effort { level } => {
                let tokens = match level.as_str() {
                    "low" => 2_048,
                    "medium" => 8_192,
                    "high" => 32_768,
                    other => return Err(format!("unknown effort `{other}` — low|medium|high")),
                };
                self.effort = Some(level);
                Ok(format!(
                    "effort set — max output {tokens} tokens per request"
                ))
            }
            SlashCommand::Goal { text } => {
                if text.trim().is_empty() || text.trim() == "clear" {
                    let had = self.session_goal.take();
                    return Ok(match had {
                        Some(g) => format!("goal cleared (was: {g})"),
                        None => "no goal set".into(),
                    });
                }
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
                let summary = summarizer(transcript).await;
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

    /// /config — interactive hosts ALWAYS open the settings menu (no key=value
    /// typing). Headless hosts get the text report.
    async fn run_config_command(&mut self, _args: Vec<String>) -> Result<String, String> {
        if self.events.is_some() {
            self.emit(crate::AgentEvent::OpenConfigMenu);
            Ok(String::new())
        } else {
            Ok(self.config_report())
        }
    }

    fn config_report(&self) -> String {
        let p = &self.cfg.permissions;
        format!(
            "effective configuration\n  model: {}\n  context window: {} tokens (compaction at {}%)\n  max steps: {}\n  permission mode: {} ({} allow / {} deny rules)\n  tool output cap: {} bytes\n  appearance dir: config at {}",
            self.cfg.model_ref,
            self.cfg.context_window,
            (crate::compaction::COMPACT_THRESHOLD * 100.0) as u32,
            self.cfg.max_steps,
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
            "max_steps" => {
                let v: u32 = value
                    .parse()
                    .map_err(|_| "max_steps must be a positive integer")?;
                self.cfg.max_steps = v;
                format!("max steps set to {v}")
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
                    "unknown key `{other}` — keys: model, max_steps, context_window, permission_mode, tool_output_bytes"
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
            "max_steps" => cfg.session.max_steps = value.parse().map_err(|_| "bad integer")?,
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

    fn providers_snapshot(&self) -> Vec<(String, loon_config::Provider)> {
        // Config-driven listing; the live registry does not expose provider defs.
        let paths = self.loon_paths();
        let Ok((cfg, _)) = loon_config::Config::load_or_builtin(&paths) else {
            return Vec::new();
        };
        cfg.providers.into_iter().collect()
    }

    fn combos_snapshot(&self) -> Vec<(String, String)> {
        let paths = self.loon_paths();
        let Ok((cfg, _)) = loon_config::Config::load_or_builtin(&paths) else {
            return Vec::new();
        };
        cfg.combos
            .into_iter()
            .map(|(name, combo)| {
                let members: Vec<String> = combo
                    .members
                    .iter()
                    .map(|m| format!("{}:{}", m.provider, m.model))
                    .collect();
                (name, members.join(", "))
            })
            .collect()
    }

    /// Persist a new provider to config.toml. The LIVE registry is immutable, so new
    /// entries load next session — stated plainly, never implied otherwise.
    fn persist_provider(
        &self,
        name: &str,
        url: &str,
        api: &str,
        key_env: Option<&str>,
    ) -> Result<String, String> {
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("provider name must be alphanumeric/-/_".into());
        }
        let paths = self.loon_paths();
        let (mut cfg, _) =
            loon_config::Config::load_or_builtin(&paths).map_err(|e| e.to_string())?;
        cfg.providers.insert(
            name.to_string(),
            loon_config::Provider {
                base_url: url.to_string(),
                api: match api {
                    "responses" => loon_config::ApiFamily::Responses,
                    _ => loon_config::ApiFamily::Chat,
                },
                api_key_env: key_env.filter(|k| !k.is_empty()).map(str::to_string),
                ..loon_config::Provider::default()
            },
        );
        cfg.save(&paths).map_err(|e| e.to_string())?;
        Ok(format!(
            "provider `{name}` saved to config.toml — restart (or /new after adding models) to use it"
        ))
    }

    fn persist_combo(&self, name: &str, members: &[&str]) -> Result<String, String> {
        let paths = self.loon_paths();
        let (mut cfg, _) =
            loon_config::Config::load_or_builtin(&paths).map_err(|e| e.to_string())?;
        let mut parsed = Vec::new();
        for m in members {
            let (provider, model) = m
                .split_once(':')
                .ok_or_else(|| format!("member `{m}` must be provider:model"))?;
            // accept either a models-key name or provider:model-id spelling
            if cfg.models.contains_key(model) {
                let def = &cfg.models[model];
                parsed.push(loon_config::ComboMember {
                    provider: def.provider.clone(),
                    model: def.id.clone(),
                    max_retries: None,
                });
            } else {
                parsed.push(loon_config::ComboMember {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    max_retries: None,
                });
            }
        }
        cfg.combos.insert(
            name.to_string(),
            loon_config::Combo {
                mode: loon_config::ComboMode::Fallback,
                members: parsed,
                requires: loon_config::ComboRequires::default(),
                cost_ceiling: None,
                persistent_rotation: false,
            },
        );
        cfg.save(&paths).map_err(|e| e.to_string())?;
        Ok(format!(
            "combo `{name}` saved to config.toml — usable as combo:{name} next session"
        ))
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

    /// /context: where the window stands, with a usage bar. Estimates are labeled as
    /// estimates everywhere they appear.
    pub fn context_report(&self) -> String {
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
        format!(
            "context: ~{est} est. tokens {bar}{pct}\n{} message(s) [{}]\ncompaction at {}% of window ({} tokens); {}",
            self.history.len(),
            roles.join(" "),
            (crate::compaction::COMPACT_THRESHOLD * 100.0) as u32,
            (window as f64 * crate::compaction::COMPACT_THRESHOLD) as u64,
            if self.should_compact() {
                "compaction will trigger before the next request"
            } else {
                "no compaction needed"
            }
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
