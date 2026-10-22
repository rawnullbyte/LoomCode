//! `loon` — terminal-native coding agent.
//!
//! Exit codes (documented contract):
//!   0 success · 1 task failed · 2 cancelled · 64 usage/config error

use clap::{Parser, Subcommand};
use loon_config::{Config, LoonPaths};
use loon_core::{Agent, AgentConfig, AgentEvent, TurnStatus};
use loon_providers::ProviderRegistry;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "loom",
    version,
    about = "A transparent, inspectable, terminal-native coding agent",
    after_help = "Exit codes: 0 success · 1 failed · 2 cancelled · 64 usage/config error"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one task headlessly (scripts, CI).
    Run {
        /// Task prompt (omit with --prompt-file).
        prompt: Option<String>,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Model alias or combo:NAME (overrides config).
        #[arg(long)]
        model: Option<String>,
        /// Output mode: text (default) | jsonl | json.
        #[arg(long)]
        output: Option<String>,
        /// Coding effort; wire reasoning is sent only to opted-in providers.
        #[arg(long, value_parser = ["low","medium","high","xhigh","ultra"])]
        effort: Option<String>,
        /// Keep an independent supervisor reviewing every agent stop.
        #[arg(long)]
        ultraloop: bool,
        /// Optional separate provider/model or combo for the supervisor.
        #[arg(long, requires = "ultraloop")]
        supervisor_model: Option<String>,
        /// Supervisor review budget; 0 means unlimited until complete/cancelled.
        #[arg(long, default_value_t = 0, requires = "ultraloop")]
        ultraloop_limit: u32,
        /// Run the opaque TUI palette instead of transparent mode (interactive only).
        #[arg(long)]
        opaque: bool,
    },
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCmd,
    },
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Environment and dependency diagnostics (fully offline unless --connect).
    Doctor {
        #[arg(long)]
        connect: bool,
    },
}

#[derive(Subcommand)]
enum SessionsCmd {
    /// List sessions, newest first.
    List,
    /// Export a session as markdown.
    Export { id: String },
    /// Delete a session and its journal. Requires --force or a matching confirmation.
    Delete {
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the config file path.
    Path,
    /// Validate config, showing every finding.
    Validate,
    /// Write the commented example config (never overwrites).
    Init,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let code = match &cli.cmd {
        None => tui_main().await,
        Some(Command::Run { .. }) => run_headless(cli).await,
        Some(Command::Sessions { cmd }) => sessions_cmd(cmd),
        Some(Command::Config { cmd }) => config_cmd(cmd),
        Some(Command::Doctor { connect }) => doctor(*connect),
    };
    std::process::exit(code);
}

fn load_env() -> anyhow::Result<(LoonPaths, Config)> {
    let paths = LoonPaths::resolve();
    paths.ensure()?;
    let (cfg, findings) = Config::load_effective(&paths, &std::env::current_dir()?)?;
    for f in &findings {
        let tag = if f.is_error { "error" } else { "warning" };
        eprintln!("config {tag}: {}: {}", f.path, f.message);
    }
    if findings.iter().any(|f| f.is_error) {
        anyhow::bail!("configuration has errors; fix ~/.config/loon/config.toml or run `loon config validate`");
    }
    Ok((paths, cfg))
}

async fn tui_main() -> i32 {
    let (paths, cfg) = match load_env() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("loon: {e}");
            return 64;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = match ProviderRegistry::from_config(&cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("loon: provider registry: {e}");
            return 64;
        }
    };
    let mut agent_cfg = AgentConfig::from(&cfg.session);
    agent_cfg.permissions = cfg.permissions.clone();
    agent_cfg.context_window = effective_context_window(&cfg, &cfg.session.model);
    let transparent = cfg.appearance.mode == loon_config::AppearanceMode::Transparent;
    // MCP servers connect before the TUI session (errors are visible; never silent).
    let (mcp_tools, mcp_errors) = loon_core::mcp::connect_all(&cfg.mcp.servers).await;
    for e in &mcp_errors {
        eprintln!("loon: {e}");
    }
    agent_cfg.extra_tools = mcp_tools;
    match loon_tui::run(&paths, agent_cfg, registry, transparent, cwd).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("loon: terminal error: {e}");
            1
        }
    }
}

async fn run_headless(cli: Cli) -> i32 {
    let Some(Command::Run {
        prompt,
        prompt_file,
        cwd,
        model,
        output,
        effort,
        ultraloop,
        supervisor_model,
        ultraloop_limit,
        opaque: _,
    }) = &cli.cmd
    else {
        return 64;
    };
    let text = match (prompt, prompt_file) {
        (Some(p), None) => p.clone(),
        (None, Some(f)) => match std::fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("loon: cannot read prompt file: {e}");
                return 64;
            }
        },
        (Some(_), Some(_)) => {
            eprintln!("loon: pass either a prompt or --prompt-file, not both");
            return 64;
        }
        (None, None) => {
            eprintln!("loon: `loon run` needs a prompt or --prompt-file");
            return 64;
        }
    };
    if text.trim().is_empty() {
        eprintln!("loon: refusing an empty prompt — nothing to do");
        return 64;
    }

    let (paths, mut cfg) = match load_env() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("loon: {e}");
            return 64;
        }
    };
    if let Some(m) = model {
        cfg.session.model = m.clone();
    }
    let workspace = cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if !workspace.is_dir() {
        eprintln!("loon: --cwd {} is not a directory", workspace.display());
        return 64;
    }

    let registry = match ProviderRegistry::from_config(&cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("loon: provider registry: {e}");
            return 64;
        }
    };
    // resolve the model reference up front so typos fail before any work
    if let Err(e) = registry.resolve(&cfg.session.model) {
        eprintln!("loon: {e}");
        return 64;
    }

    let mut agent_cfg = AgentConfig::from(&cfg.session);
    agent_cfg.permissions = cfg.permissions.clone();
    agent_cfg.context_window = effective_context_window(&cfg, &cfg.session.model);
    // MCP servers: connect before the session; failures are visible, never silent.
    let (mcp_tools, mcp_errors) = loon_core::mcp::connect_all(&cfg.mcp.servers).await;
    for e in &mcp_errors {
        eprintln!("loon: {e}");
    }
    if !mcp_tools.is_empty() {
        eprintln!(
            "loon: {} MCP tool(s) registered from {} server(s)",
            mcp_tools.len(),
            cfg.mcp.servers.len()
        );
    }
    let loop_registry = std::sync::Arc::new(loon_core::loops::LoopRegistry::new());
    let tools = loon_core::subagent::default_tools(
        &paths,
        &workspace,
        &agent_cfg,
        registry.clone(),
        Some(loop_registry.clone()),
    )
    .with_extra(mcp_tools);
    let mut agent =
        match Agent::new_session(&paths, agent_cfg, registry.clone(), tools, workspace, true) {
            Ok(mut a) => {
                a.loops = loop_registry.clone();
                a
            }
            Err(e) => {
                eprintln!("loon: session: {e}");
                return 1;
            }
        };

    if let Some(effort) = effort {
        agent.effort = Some(effort.clone());
    }
    if *ultraloop {
        if let Some(model) = supervisor_model {
            if let Err(e) = agent.registry.resolve(model) {
                eprintln!("supervisor model: {e}");
                return 64;
            }
        }
        agent.session_goal = Some(text.clone());
        if let Err(e) = agent.worklog.update(|s| {
            s.ultraloop = true;
            s.goal = Some(text.clone());
            s.supervisor_model = supervisor_model.clone();
            s.ultraloop_limit = *ultraloop_limit;
            Ok(())
        }) {
            eprintln!("ultraloop: {e}");
            return 1;
        }
    }
    let mode = output.clone().unwrap_or_else(|| "text".into());
    let jsonl = mode == "jsonl" || mode == "json";
    if !jsonl && mode != "text" {
        eprintln!("loon: --output must be text, jsonl, or json");
        return 64;
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(1024);
    agent.set_event_sink(tx);
    let session_id = agent.session_id();

    let cancel = agent.cancel_handle();
    let mut turn = tokio::spawn(async move {
        let result = agent.run_turn(&text).await;
        agent.finish_session().await;
        result
    });

    if jsonl {
        let mut w = std::io::stdout();
        let _ = writeln!(
            w,
            "{}",
            serde_json::json!({"v": 1, "type": "session", "session_id": session_id.to_string(), "model": cfg.session.model})
        );
    }
    let printer = tokio::spawn(async move {
        let mut stdout = std::io::stdout();
        while let Some(ev) = rx.recv().await {
            match (&ev, jsonl) {
                (AgentEvent::TextDelta(t), false) => {
                    let _ = write!(stdout, "{t}");
                    let _ = stdout.flush();
                }
                (ev, true) => {
                    let line = serde_json::json!({"v": 1, "type": event_type(ev), "data": ev});
                    let _ = writeln!(stdout, "{line}");
                }
                (AgentEvent::Warning(w), false) => {
                    let _ = writeln!(std::io::stderr(), "warning: {w}");
                }
                (AgentEvent::MemberNote(m), false) => {
                    let _ = writeln!(std::io::stderr(), "{m}");
                }
                _ => {}
            }
        }
    });
    let outcome = tokio::select! {
        result=&mut turn=>result,
        _=tokio::signal::ctrl_c()=>{let _=cancel.send(true);turn.await}
    };
    let result = match outcome {
        Ok(r) => r,
        Err(e) => {
            eprintln!("loon: turn task: {e}");
            return 1;
        }
    };
    let _ = printer.await;
    if jsonl {
        let mut stdout = std::io::stdout();
        let _ = writeln!(
            stdout,
            "{}",
            serde_json::json!({
                "v": 1,
                "type": "turn",
                "status": status_name(&result.status),
                "steps": result.steps,
                "usage": result.usage,
                "assistant_text": result.assistant_text,
            })
        );
    } else {
        println!();
        match &result.status {
            TurnStatus::Completed => {}
            other => eprintln!("loon: turn ended: {other:?}"),
        }
    }
    match result.status {
        TurnStatus::Completed => 0,
        TurnStatus::Failed(_) => 1,
        TurnStatus::Cancelled => 2,
    }
}

/// Effective context window for a model reference: the model def's window, a combo's
/// minimum across members, or a conservative 128k default when unknown.
fn effective_context_window(cfg: &loon_config::Config, model_ref: &str) -> u64 {
    if let Some(window) = cfg.session.context_window {
        return window;
    }
    if let Some(def) = cfg.models.get(model_ref) {
        if let Some(w) = def.context_window {
            return w;
        }
    }
    if let Some(name) = model_ref.strip_prefix("combo:") {
        if let Some(combo) = cfg.combos.get(name) {
            if let Some(w) = combo.requires.min_context_window {
                return w;
            }
            // Fall back to the smallest declared member window.
            let windows: Vec<u64> = combo
                .members
                .iter()
                .filter_map(|m| {
                    cfg.models
                        .values()
                        .find(|d| d.provider == m.provider && d.id == m.model)
                        .and_then(|d| d.context_window)
                })
                .collect();
            if let Some(min) = windows.iter().min() {
                return *min;
            }
        }
    }
    128_000
}

fn event_type(ev: &AgentEvent) -> &'static str {
    match ev {
        AgentEvent::UiPreference { .. } => "ui_preference",
        AgentEvent::CatalogUpdated { .. } => "catalog_updated",
        AgentEvent::OpenCatalog { .. } => "open_catalog",
        AgentEvent::OpenEditor(_) => "open_editor",
        AgentEvent::OpenMenu { .. } => "open_menu",
        AgentEvent::SetPalette { .. } => "set_palette",
        AgentEvent::ContextUpdated { .. } => "context_updated",
        AgentEvent::ModelChanged { .. } => "model_changed",
        AgentEvent::EffortChanged { .. } => "effort_changed",
        AgentEvent::SessionLoaded { .. } => "session_loaded",
        AgentEvent::ConversationRewound { .. } => "conversation_rewound",
        AgentEvent::WorkStateChanged { .. } => "work_state_changed",
        AgentEvent::TurnStarted => "turn_started",
        AgentEvent::TextDelta(_) => "text_delta",
        AgentEvent::StreamReset { .. } => "stream_reset",
        AgentEvent::ReasoningDelta(_) => "reasoning_delta",
        AgentEvent::RequestStarted => "request_started",
        AgentEvent::RequestFinished => "request_finished",
        AgentEvent::RequestUsage { .. } => "request_usage",
        AgentEvent::OutputProgress { .. } => "output_progress",
        AgentEvent::ReasoningUsage { .. } => "reasoning_usage",
        AgentEvent::ToolStarted { .. } => "tool_started",
        AgentEvent::ToolFinished { .. } => "tool_finished",
        AgentEvent::MemberNote(_) => "member_note",
        AgentEvent::ApprovalRequested { .. } => "approval_requested",
        AgentEvent::ModeChanged { .. } => "mode_changed",
        AgentEvent::ToggleTheme => "toggle_theme",
        AgentEvent::SetTransparent { .. } => "set_transparent",
        AgentEvent::OpenConfigMenu => "open_config_menu",
        AgentEvent::OpenModelPicker { .. } => "open_model_picker",
        AgentEvent::OpenEffortPicker => "open_effort_picker",
        AgentEvent::OpenProviderForm => "open_provider_form",
        AgentEvent::ClearTranscript => "clear_transcript",
        AgentEvent::Warning(_) => "warning",
        AgentEvent::TurnFinished { .. } => "turn_finished",
    }
}

fn status_name(s: &TurnStatus) -> &'static str {
    match s {
        TurnStatus::Completed => "completed",
        TurnStatus::Failed(_) => "failed",
        TurnStatus::Cancelled => "cancelled",
    }
}

fn sessions_cmd(cmd: &SessionsCmd) -> i32 {
    let paths = LoonPaths::resolve();
    match cmd {
        SessionsCmd::List => {
            let store = match loon_core::open_store(&paths) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("loon: store: {e}");
                    return 1;
                }
            };
            let list =
                loon_events::list_all_sessions(&store, &paths.sessions_dir()).unwrap_or_default();
            if list.is_empty() {
                println!("no sessions yet");
                return 0;
            }
            println!("{:<38} {:<20} {:<10} TITLE", "SESSION", "CREATED", "CWD");
            for s in &list {
                let created = chrono_fmt(s.created_ms);
                let cwd = s.cwd.chars().take(10).collect::<String>();
                println!("{:<38} {:<20} {:<10} {}", s.id, created, cwd, s.title);
            }
            0
        }
        SessionsCmd::Export { id } => {
            let Ok(sid) = id.parse() else {
                eprintln!("loon: `{id}` is not a session id (uuid)");
                return 64;
            };
            let store = match loon_core::open_store(&paths) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("loon: store: {e}");
                    return 1;
                }
            };
            match loon_events::Journal::open_default(&store, &paths.sessions_dir(), sid) {
                Ok((_, events)) => {
                    println!("# Loon session {id}");
                    println!();
                    for ev in &events {
                        println!("- `seq {}` {}", ev.seq, ev.summary().replace('\n', " "));
                    }
                    0
                }
                Err(e) => {
                    eprintln!("loon: cannot open session: {e}");
                    1
                }
            }
        }
        SessionsCmd::Delete { id, force } => {
            let Ok(sid) = id.parse() else {
                eprintln!("loon: `{id}` is not a session id (uuid)");
                return 64;
            };
            let store = match loon_core::open_store(&paths) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("loon: store: {e}");
                    return 1;
                }
            };
            let Some(meta) = (match store.session_meta(sid) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("loon: store: {e}");
                    return 1;
                }
            }) else {
                eprintln!(
                    "loon: no session {} (it may have been deleted, or never had a message)",
                    id
                );
                return 1;
            };
            if !force {
                let short = id.chars().take(8).collect::<String>();
                eprintln!(
                    "delete session {short} ({} events) titled `{}`?",
                    store.events(sid).map(|e| e.len()).unwrap_or(0),
                    meta.title
                );
                eprint!("type the session id prefix to confirm: ");
                use std::io::Write as _;
                let _ = std::io::stderr().flush();
                let mut answer = String::new();
                if std::io::stdin().read_line(&mut answer).is_err() {
                    return 1;
                }
                if answer.trim() != short {
                    eprintln!("aborted");
                    return 1;
                }
            }
            match store.delete_session(sid) {
                Ok(()) => {
                    println!("deleted session {}", id);
                    0
                }
                Err(e) => {
                    eprintln!("loon: cannot delete session: {e}");
                    1
                }
            }
        }
    }
}

fn chrono_fmt(ms: i64) -> String {
    // ISO-ish without pulling chrono into the CLI: ms since epoch, UTC.
    let secs = ms / 1000;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}Z")
}

fn config_cmd(cmd: &ConfigCmd) -> i32 {
    let paths = LoonPaths::resolve();
    match cmd {
        ConfigCmd::Path => {
            println!("{}", paths.config_file().display());
            0
        }
        ConfigCmd::Validate => match Config::load_or_builtin(&paths) {
            Ok((_, findings)) => {
                if findings.is_empty() {
                    println!("config OK");
                    0
                } else {
                    for f in &findings {
                        println!(
                            "{} {}: {}",
                            if f.is_error { "error" } else { "warning" },
                            f.path,
                            f.message
                        );
                    }
                    if findings.iter().any(|f| f.is_error) {
                        1
                    } else {
                        0
                    }
                }
            }
            Err(e) => {
                eprintln!("{e}");
                64
            }
        },
        ConfigCmd::Init => {
            let p = paths.config_file();
            if p.exists() {
                eprintln!("loon: {} already exists; not overwriting", p.display());
                return 1;
            }
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&p, loon_config::example_toml()) {
                Ok(()) => {
                    println!("wrote {}", p.display());
                    0
                }
                Err(e) => {
                    eprintln!("loon: {e}");
                    1
                }
            }
        }
    }
}

fn doctor(connect: bool) -> i32 {
    let mut ok = true;
    let paths = LoonPaths::resolve();
    println!("loon doctor");
    println!("  paths:");
    for (name, d) in [
        ("config", &paths.config_dir),
        ("data", &paths.data_dir),
        ("state", &paths.state_dir),
        ("cache", &paths.cache_dir),
    ] {
        let writable = std::fs::create_dir_all(d).is_ok();
        println!(
            "    {name:<8} {} {}",
            d.display(),
            if writable { "ok" } else { "NOT WRITABLE" }
        );
        ok &= writable;
    }
    println!("  terminal:");
    println!(
        "    TERM={} COLORTERM={} NO_COLOR={}",
        std::env::var("TERM").unwrap_or_default(),
        std::env::var("COLORTERM").unwrap_or_default(),
        std::env::var("NO_COLOR").is_ok()
    );
    println!("    transparent mode is the default; set [appearance] mode = \"opaque\" to opt out");
    println!("  tools:");
    let sh = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("true")
        .status()
        .is_ok();
    println!("    /bin/sh: {}", if sh { "ok" } else { "MISSING" });
    ok &= sh;
    println!("  config:");
    match Config::load_or_builtin(&paths) {
        Ok((cfg, findings)) => {
            if findings.is_empty() {
                println!("    valid");
            } else {
                for f in &findings {
                    println!(
                        "    {}: {} — {}",
                        if f.is_error { "error" } else { "warning" },
                        f.path,
                        f.message
                    );
                    ok &= !f.is_error;
                }
            }
            println!("  providers:");
            for (id, p) in &cfg.providers {
                let auth = match &p.api_key_env {
                    Some(e) => format!(
                        "env {e} {}",
                        if std::env::var(e).is_ok() {
                            "set"
                        } else {
                            "NOT SET"
                        }
                    ),
                    None => "no auth (local endpoint?)".into(),
                };
                println!("    {id}: {} [{}] {}", p.base_url, p.api.as_str(), auth);
                if connect && !p.mock {
                    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
                    let status = std::process::Command::new("curl")
                        .args([
                            "-s",
                            "-o",
                            "/dev/null",
                            "-w",
                            "%{http_code}",
                            "--max-time",
                            "5",
                            &url,
                        ])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                        .unwrap_or_else(|_| "no-curl".into());
                    println!("      GET /models → HTTP {status}");
                }
            }
        }
        Err(e) => {
            println!("    INVALID: {e}");
            ok = false;
        }
    }
    println!(
        "{}",
        if ok {
            "doctor: OK"
        } else {
            "doctor: problems found"
        }
    );
    if ok {
        0
    } else {
        1
    }
}
