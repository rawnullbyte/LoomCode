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
        ("quit", "exit"),
    ]
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
            SlashCommand::Quit => unreachable!("quit handled by the caller"),
        }
    }

    /// /config — `show` (default) renders the effective configuration; `set <key>
    /// <value>` changes the live session AND persists to the config file through the
    /// comment-preserving save path, reported separately so the user knows what stuck.
    async fn run_config_command(&mut self, args: Vec<String>) -> Result<String, String> {
        match args.first().map(|s| s.as_str()) {
            None | Some("show") => Ok(self.config_report()),
            Some("set") => {
                let key = args
                    .get(1)
                    .ok_or("usage: /config set <key> <value> — keys: model, max_steps, context_window, permission_mode, tool_output_bytes")?;
                let value = args.get(2).ok_or("missing <value>")?;
                self.config_set(key, value)
            }
            Some(other) => Err(format!(
                "unknown /config subcommand `{other}` — use /config or /config set <key> <value>"
            )),
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
                loon_config::PermissionMode::Allowlist => "allowlist",
            },
            p.allow_rules.len(),
            p.deny_rules.len(),
            self.cfg.tool_output_bytes,
            self.config_dir.join("config.toml").display(),
        )
    }

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
                    "allowlist" => loon_config::PermissionMode::Allowlist,
                    other => return Err(format!("unknown permission mode `{other}` (bypass|ask|allowlist)")),
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

    /// /context: where the window stands. Estimates are labeled as estimates.
    pub fn context_report(&self) -> String {
        let est = self.estimated_context_tokens();
        let window = self.cfg.context_window;
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
            "context: ~{est} est. tokens{pct}, {} message(s) [{}]\ncompaction at {}% of window ({} tokens); {}",
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
