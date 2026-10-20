//! Loon's terminal UI.
//!
//! Rendering invariant: in transparent mode **every application-owned cell uses the
//! terminal default background** (`Color::Reset`). All styles come from [`theme`]; a test
//! walks rendered buffers asserting the invariant. Selection and highlights are done with
//! foreground color/underline — never reverse video. Opaque mode is explicit opt-in.

pub mod theme;
pub mod ui;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
pub mod autocomplete;
pub mod queue;

use loon_core::{Agent, AgentConfig, AgentEvent, TurnResult, TurnStatus};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone)]
pub enum UiCommand {
    Submit(String),
    Cancel,
    Slash(loon_core::commands::SlashCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        ok: bool,
        excerpt: String,
        pending: bool,
    },
    System(String),
    Member(String),
}

#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub model: String,
    pub member: Option<String>,
    pub mode: String,
    pub context_est: u64,
    pub busy: bool,
    pub cwd: String,
}

pub struct App {
    pub blocks: Vec<Block>,
    pub input: String,
    /// Scroll offset from the bottom in lines; `None` means follow (stick to latest).
    pub scroll_from_bottom: Option<u16>,
    pub transparent: bool,
    pub status: StatusLine,
    streaming: Option<String>,
    pub quit: bool,
    pub cancel_hint_shown: bool,
    /// Animation clock, advanced by the render loop (~20 fps).
    pub tick: u64,
    /// When the current turn started, for the elapsed-time readout.
    pub turn_started: Option<std::time::Instant>,
    /// Selected row in the slash-command popup; reset on input change.
    pub completion_selected: usize,
    /// Set by Esc: hide the popup until the input changes again.
    pub popup_dismissed: bool,
}

impl App {
    pub fn new(transparent: bool, status: StatusLine) -> App {
        App {
            blocks: Vec::new(),
            input: String::new(),
            scroll_from_bottom: None,
            transparent,
            status,
            streaming: None,
            quit: false,
            cancel_hint_shown: false,
            tick: 0,
            turn_started: None,
            completion_selected: 0,
            popup_dismissed: false,
        }
    }

    /// Advance the animation clock (called once per render loop pass).
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Input changed: popup returns, selection resets.
    pub fn input_changed(&mut self) {
        self.completion_selected = 0;
        self.popup_dismissed = false;
    }

    /// Commands matching the current input (popup contents), or empty when hidden.
    pub fn visible_completions(&self) -> Vec<autocomplete::Completion> {
        if self.popup_dismissed {
            return Vec::new();
        }
        autocomplete::completions_for(&self.input)
    }

    pub fn completion_count(&self) -> usize {
        self.visible_completions().len()
    }

    /// Replace the input with the selected completion's command word.
    pub fn accept_completion(&mut self) -> bool {
        let completions = self.visible_completions();
        if completions.is_empty() {
            return false;
        }
        let idx = self.completion_selected.min(completions.len() - 1);
        let cmd = completions[idx].insert.clone();
        self.input = format!("/{cmd} ");
        self.input_changed();
        true
    }

    /// The busy-spinner glyph: six glyphs cycled forward then backward (palindrome),
    /// so the motion eases rather than snaps at the loop point.
    pub fn spinner_glyph(&self) -> char {
        const FRAMES: [char; 6] = ['·', '✢', '✳', '✶', '✻', '✽'];
        let n = FRAMES.len();
        let period = 2 * n - 2;
        let i = (self.tick as usize) % period;
        FRAMES[if i < n { i } else { period - i }]
    }

    /// Seconds since the current turn started (0 when idle).
    pub fn elapsed_secs(&self) -> u64 {
        self.turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    pub fn push_block(&mut self, b: Block) {
        self.blocks.push(b);
        self.scroll_from_bottom = None; // follow
    }

    pub fn stream_text(&mut self, delta: &str) {
        self.streaming
            .get_or_insert_with(String::new)
            .push_str(delta);
        self.scroll_from_bottom = None;
    }

    pub fn commit_stream(&mut self) {
        if let Some(s) = self.streaming.take() {
            if !s.trim().is_empty() {
                self.push_block(Block::Assistant(s));
            }
        }
    }

    pub fn tool_started(&mut self, name: &str, args_excerpt: &str) {
        self.commit_stream();
        self.push_block(Block::Tool {
            name: name.to_string(),
            ok: true,
            excerpt: format!("… {}", args_excerpt.lines().next().unwrap_or("")),
            pending: true,
        });
    }

    pub fn tool_finished(&mut self, name: &str, ok: bool, excerpt: &str) {
        // Replace the newest pending block for this tool, if present.
        if let Some(pos) = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::Tool { pending: true, name: n, .. } if n == name))
        {
            let first_lines: String = excerpt.lines().take(3).collect::<Vec<_>>().join(" ⏎ ");
            self.blocks[pos] = Block::Tool {
                name: name.to_string(),
                ok,
                excerpt: if first_lines.is_empty() {
                    "(no output)".into()
                } else {
                    first_lines
                },
                pending: false,
            };
        } else {
            self.push_block(Block::Tool {
                name: name.to_string(),
                ok,
                excerpt: excerpt.to_string(),
                pending: false,
            });
        }
    }

    /// Rendered transcript lines, wrapped to `width`.
    pub fn transcript_lines(&self, width: u16) -> Vec<ratatui::text::Line<'static>> {
        let mut lines = Vec::new();
        let w = width.max(8) as usize;
        let mut push = |prefix: &str, text: &str, style: ratatui::style::Style| {
            let indent = prefix.chars().count();
            let wrapped = wrap_text(text, w.saturating_sub(indent).max(4));
            for (i, seg) in wrapped.into_iter().enumerate() {
                let lead = if i == 0 {
                    prefix.to_string()
                } else {
                    " ".repeat(indent)
                };
                lines.push(ratatui::text::Line::styled(format!("{lead}{seg}"), style));
            }
        };
        for b in &self.blocks {
            match b {
                Block::User(t) => push("you › ", t, theme::user_fg()),
                Block::Assistant(t) => push("loon › ", t, theme::assistant_fg()),
                Block::Tool {
                    name, ok, excerpt, ..
                } => {
                    let mark = if *ok { "✓" } else { "✗" };
                    push(
                        &format!("  {mark} {name}: "),
                        excerpt,
                        if *ok {
                            theme::tool_ok_fg()
                        } else {
                            theme::tool_err_fg()
                        },
                    );
                }
                Block::System(t) => push("· ", t, theme::system_fg()),
                Block::Member(t) => push("↻ ", t, theme::member_fg()),
            }
        }
        if let Some(s) = &self.streaming {
            push("loon › ", s, theme::assistant_fg());
        }
        lines
    }
}

pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in para.split(' ') {
            if line.chars().count() + word.chars().count() + 1 > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

/// Run the interactive TUI. Restores terminal state on every exit path.
pub async fn run(
    paths: &loon_config::LoonPaths,
    cfg: AgentConfig,
    registry: Arc<loon_providers::ProviderRegistry>,
    transparent: bool,
    cwd: PathBuf,
) -> anyhow::Result<()> {
    let res = {
        let mut stdout = std::io::stdout();
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let r = run_inner(&mut terminal, paths, cfg, registry, transparent, cwd).await;
        // restore before propagating the result
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
        r
    };
    res
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    paths: &loon_config::LoonPaths,
    cfg: AgentConfig,
    registry: Arc<loon_providers::ProviderRegistry>,
    transparent: bool,
    cwd: PathBuf,
) -> anyhow::Result<()> {
    let agent = Arc::new(Mutex::new(
        Agent::new_session(
            paths,
            cfg.clone(),
            registry.clone(),
            loon_core::subagent::default_tools(paths, &cwd, &cfg, registry),
            cwd.clone(),
            false,
        )
        .map_err(|e| anyhow::anyhow!("session init: {e}"))?,
    ));
    let (ev_tx, mut ev_rx) = mpsc::channel::<AgentEvent>(1024);
    let (slash_tx, mut slash_rx) = mpsc::channel::<String>(64);
    agent.lock().await.set_event_sink(ev_tx.clone());

    let status = StatusLine {
        model: cfg.model_ref.clone(),
        member: None,
        mode: "bypass".into(),
        context_est: 0,
        busy: false,
        cwd: cwd.display().to_string(),
    };
    let mut app = App::new(transparent, status);
    app.push_block(Block::System(format!(
        "Loon ready — model {} · transparent mode {} · Enter sends · Ctrl+C cancels (twice quits) · /quit exits",
        cfg.model_ref,
        if transparent { "on" } else { "OFF (opaque)" }
    )));

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(32);

    // Driver: one turn at a time; cancellation stays responsive while a turn runs.
    let mut prompt_queue = queue::PromptQueue::new(16);
    let driver_slash_tx = slash_tx.clone();
    let driver = tokio::spawn(async move {
        let mut running: Option<(
            tokio::task::JoinHandle<TurnResult>,
            mpsc::Sender<AgentEvent>,
        )> = None;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                UiCommand::Submit(text) => {
                    if running.is_some() {
                        // Queue instead of dropping; position is visible via the
                        // queued event the UI prints.
                        match prompt_queue.push(text.clone()) {
                            Ok(n) => {
                                let _ = ev_tx
                                    .send(AgentEvent::Warning(format!(
                                        "queued ({n} pending) — runs after the current turn"
                                    )))
                                    .await;
                            }
                            Err(reason) => {
                                let _ = ev_tx.send(AgentEvent::Warning(reason)).await;
                            }
                        }
                        continue;
                    }
                    let a = agent.clone();
                    let tx = ev_tx.clone();
                    running = Some((
                        tokio::spawn(async move {
                            let mut guard = a.lock().await;
                            guard.run_turn(&text).await
                        }),
                        tx,
                    ));
                }
                UiCommand::Cancel => {
                    let a = agent.clone();
                    tokio::spawn(async move { a.lock().await.cancel() });
                }
                UiCommand::Slash(cmd) => {
                    if running.is_some() {
                        let _ = driver_slash_tx
                            .send(
                                "busy: wait for the current turn to finish (Ctrl+C cancels)".into(),
                            )
                            .await;
                    } else {
                        let a = agent.clone();
                        let tx = driver_slash_tx.clone();
                        tokio::spawn(async move {
                            let mut guard = a.lock().await;
                            let out = match cmd {
                                loon_core::commands::SlashCommand::Quit => {
                                    "use /quit (handled by the interface)".to_string()
                                }
                                other => guard.run_slash(other).await.unwrap_or_else(|e| e),
                            };
                            let _ = tx.send(out).await;
                        });
                    }
                }
            }
            // Reap finished turns without blocking command handling; a finished turn
            // releases the queued prompt, if any.
            if let Some((h, _)) = &running {
                if h.is_finished() {
                    running = None;
                    if let Some(next) = prompt_queue.pop() {
                        let a = agent.clone();
                        let tx = ev_tx.clone();
                        running = Some((
                            tokio::spawn(async move {
                                let mut guard = a.lock().await;
                                guard.run_turn(&next).await
                            }),
                            tx,
                        ));
                    }
                }
            }
        }
    });

    let mut events = EventStream::new();
    loop {
        while let Ok(out) = slash_rx.try_recv() {
            app.push_block(Block::System(out));
        }
        while let Ok(ev) = ev_rx.try_recv() {
            match ev {
                AgentEvent::TextDelta(t) => app.stream_text(&t),
                AgentEvent::ReasoningDelta(_) => {}
                AgentEvent::ToolStarted { name, args_excerpt } => {
                    app.tool_started(&name, &args_excerpt)
                }
                AgentEvent::ToolFinished { name, ok, excerpt } => {
                    app.tool_finished(&name, ok, &excerpt)
                }
                AgentEvent::MemberNote(m) => app.push_block(Block::Member(m)),
                AgentEvent::ApprovalRequested { tool, args_excerpt } => {
                    // Ask mode without a hook (or before one is installed): surface the
                    // pending request; the agent's hook (wired in interactive mode)
                    // carries the actual y/n answer.
                    app.push_block(Block::System(format!(
                        "approval requested: {tool} {args_excerpt}"
                    )));
                }
                AgentEvent::Warning(w) => app.push_block(Block::System(format!("warning: {w}"))),
                AgentEvent::TurnFinished { status } => {
                    app.commit_stream();
                    match status {
                        TurnStatus::Completed => {}
                        TurnStatus::Cancelled => app.push_block(Block::System("cancelled".into())),
                        TurnStatus::Failed(e) => {
                            app.push_block(Block::System(format!("failed: {e}")))
                        }
                        TurnStatus::BudgetPaused { steps_used } => {
                            app.push_block(Block::System(format!(
                                "paused at step budget ({steps_used}) — resume with another prompt"
                            )))
                        }
                    }
                    app.status.busy = false;
                    app.turn_started = None;
                }
            }
        }

        app.tick();
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.quit {
            break;
        }

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CtEvent::Key(key))) => handle_key(&mut app, key, &cmd_tx).await,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break, // terminal disconnected
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    driver.abort();
    Ok(())
}

async fn handle_key(app: &mut App, key: KeyEvent, cmd_tx: &mpsc::Sender<UiCommand>) {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.status.busy {
                let _ = cmd_tx.send(UiCommand::Cancel).await;
                if !app.cancel_hint_shown {
                    app.push_block(Block::System(
                        "cancelling at the next safe boundary…".into(),
                    ));
                    app.cancel_hint_shown = true;
                }
            } else if app.cancel_hint_shown {
                app.quit = true;
            } else {
                app.cancel_hint_shown = true;
                app.push_block(Block::System("press Ctrl+C again to quit".into()));
            }
        }
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Enter
            if !key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let text = app.input.trim().to_string();
            app.input.clear();
            if text.is_empty() {
                return;
            }
            if text == "/quit" || text == "/exit" {
                app.quit = true;
                return;
            }
            if let Some(parsed) = loon_core::commands::parse(&text) {
                match parsed {
                    Ok(cmd) => {
                        let _ = cmd_tx.send(UiCommand::Slash(cmd)).await;
                    }
                    Err(msg) => app.push_block(Block::System(msg)),
                }
                return;
            }
            app.cancel_hint_shown = false;
            app.turn_started = Some(std::time::Instant::now());
            app.push_block(Block::User(text.clone()));
            app.status.busy = true;
            let _ = cmd_tx.send(UiCommand::Submit(text)).await;
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.input_changed();
        }
        KeyCode::PageUp => {
            app.scroll_from_bottom = Some(app.scroll_from_bottom.unwrap_or(0).saturating_add(10));
        }
        KeyCode::PageDown => match app.scroll_from_bottom {
            Some(0) | None => app.scroll_from_bottom = None,
            Some(n) => app.scroll_from_bottom = Some(n.saturating_sub(10)),
        },
        KeyCode::Up if app.completion_count() > 0 => {
            if app.completion_selected == 0 {
                app.completion_selected = app.completion_count() - 1; // wrap
            } else {
                app.completion_selected -= 1;
            }
        }
        KeyCode::Down if app.completion_count() > 0 => {
            app.completion_selected = (app.completion_selected + 1) % app.completion_count();
        }
        KeyCode::Tab if app.completion_count() > 0 => {
            app.accept_completion();
        }
        KeyCode::Esc if app.completion_count() > 0 => {
            app.popup_dismissed = true;
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.push(c);
            app.input_changed();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_streaming_and_tool_replace() {
        let mut app = app_for_test();
        app.stream_text("Hel");
        app.stream_text("lo world");
        app.tool_started("read", r#"{"path": "a.txt"}"#);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Tool { pending: true, .. })
        ));
        app.tool_finished("read", true, "1\tfn main()");
        match app.blocks.last() {
            Some(Block::Tool {
                pending: false,
                ok: true,
                excerpt,
                ..
            }) => assert!(excerpt.contains("fn main")),
            other => panic!("expected replaced tool block, got {other:?}"),
        }
        app.commit_stream();
        let lines = app.transcript_lines(80);
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Hello world"));
        assert!(!joined.contains("you ›"), "no user block yet");
    }

    pub fn app_for_test() -> App {
        App::new(
            true,
            StatusLine {
                model: "mock".into(),
                ..Default::default()
            },
        )
    }
}
