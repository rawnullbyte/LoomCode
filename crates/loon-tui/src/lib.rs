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
    CycleMode,
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
    /// Pre-styled lines (the welcome banner); rendered verbatim, scrolls away with
    /// the transcript like any other block.
    Rich(Vec<ratatui::text::Line<'static>>),
}

/// A text selection across the transcript, in absolute (line, char-col) coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelRange {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
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
    /// Text cursor position in the input line, as a char offset (0..=len).
    pub cursor: usize,
    /// Popup/input geometry recorded by the last draw, for mouse hit-testing.
    pub popup_rect: Option<(u16, u16, u16)>, // (x, y, height) incl. border row
    pub input_rect: Option<(u16, u16)>, // (x, y) of the input text row
    /// First completion row rendered in the popup window (for click mapping).
    pub popup_window_start: usize,
    /// Transcript geometry for selection hit-testing: (x, y, width, height).
    pub transcript_rect: Option<(u16, u16, u16, u16)>,
    /// Active text selection (drag); Some while selecting, copied on release.
    pub selection: Option<SelRange>,
    /// Text queued for clipboard write (Ctrl+Y copies the last answer).
    pub pending_copy: Option<String>,
    /// Pending ask-mode approval: tool, args excerpt, and the answer channel. Modal —
    /// keys other than y/n/Esc are ignored while set.
    pub pending_approval: Option<(String, String, tokio::sync::oneshot::Sender<bool>)>,
    /// Interactive /config menu: Some(selected row). Modal overlay.
    pub settings: Option<usize>,
    /// Model picker (bare /model): Some((refs, selected)). Modal overlay.
    pub model_picker: Option<(Vec<String>, usize)>,
    /// Effort picker (bare /effort): Some(selected). Modal overlay.
    pub effort_picker: Option<usize>,
    /// Menu-local mirrors (optimistically updated on toggle; commands re-apply).
    pub settings_autocompact: bool,
    pub settings_effort: String,
    pub settings_max_steps: u32,
    pub settings_tool_cap: usize,
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
            cursor: 0,
            popup_rect: None,
            input_rect: None,
            popup_window_start: 0,
            transcript_rect: None,
            selection: None,
            pending_approval: None,
            pending_copy: None,
            settings: None,
            model_picker: None,
            effort_picker: None,
            settings_autocompact: true,
            settings_effort: "medium".into(),
            settings_max_steps: 24,
            settings_tool_cap: 65_536,
        }
    }

    /// The welcome banner: logo spark, version, model · mode, workspace — pushed as
    /// the first transcript block so chat messages scroll it away naturally.
    pub fn push_welcome_banner(&mut self) {
        self.blocks.insert(0, Block::Rich(self.banner_lines()));
    }

    /// Banner content: animated spark + version, model, workspace — nothing else.
    fn banner_lines(&self) -> Vec<ratatui::text::Line<'static>> {
        vec![
            ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(
                    format!(" {} ", self.intro_frame()),
                    theme::accent_fg(),
                ),
                ratatui::text::Span::styled(
                    format!("Loon v{}", env!("CARGO_PKG_VERSION")),
                    theme::user_fg(),
                ),
            ]),
            ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("   {}", self.status.model),
                theme::system_fg(),
            )),
            ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("   {}", self.status.cwd),
                theme::system_fg(),
            )),
            ratatui::text::Line::default(),
        ]
    }

    /// Intro burst animation: grows, shimmers, settles on the logo glyph (forward,
    /// reverse, hold).
    fn intro_frame(&self) -> &'static str {
        const FRAMES: [&str; 7] = ["·", "✦", "✦˖", "✧˖·", "✻ ✧", "✶ ✦˖", "✦"];
        let n = FRAMES.len() as u64;
        if self.tick < n {
            FRAMES[self.tick as usize]
        } else if self.tick < 2 * n {
            FRAMES[((2 * n - 1 - self.tick) as usize).min(n as usize - 1)]
        } else {
            FRAMES[n as usize - 1]
        }
    }

    /// While the intro plays, rebuild the banner from the current animation frame.
    pub fn update_intro(&mut self) {
        if self.tick < 2 * 7 {
            let lines = self.banner_lines();
            if let Some(Block::Rich(existing)) = self.blocks.first_mut() {
                *existing = lines;
            }
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
        self.cursor = self.input.chars().count();
        self.input_changed();
        true
    }

    /// Byte offset of the given char index in the input (panics-free on clamps).
    pub fn cursor_byte_offset(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
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
        let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
        let w = width.max(8) as usize;
        // plain fn (not a closure) so the Rich arm can also borrow `lines`
        fn push(
            lines: &mut Vec<ratatui::text::Line<'static>>,
            prefix: &str,
            text: &str,
            style: ratatui::style::Style,
            w: usize,
        ) {
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
        }
        for b in &self.blocks {
            match b {
                Block::User(t) => push(&mut lines, "you › ", t, theme::user_fg(), w),
                Block::Assistant(t) => push(&mut lines, "loon › ", t, theme::assistant_fg(), w),
                Block::Tool {
                    name,
                    ok,
                    excerpt,
                    pending,
                } => {
                    // Pending tools get the animated glyph in accent — never a check
                    // mark, which would claim a result that does not exist yet.
                    let (mark, style) = if *pending {
                        (self.spinner_glyph(), theme::pending_fg())
                    } else if *ok {
                        ('✓', theme::tool_ok_fg())
                    } else {
                        ('✗', theme::tool_err_fg())
                    };
                    push(&mut lines, &format!("  {mark} {name}: "), excerpt, style, w);
                }
                Block::System(t) => push(&mut lines, "· ", t, theme::system_fg(), w),
                Block::Member(t) => push(&mut lines, "↻ ", t, theme::member_fg(), w),
                // Pre-styled lines (welcome banner) render verbatim and scroll away
                // with the rest of the transcript.
                Block::Rich(rich) => lines.extend(rich.iter().cloned()),
            }
        }
        if let Some(s) = &self.streaming {
            let wrapped = wrap_text(s, w.saturating_sub(7).max(4));
            for (i, seg) in wrapped.iter().enumerate() {
                let lead = if i == 0 { "loon › " } else { "       " };
                lines.push(ratatui::text::Line::styled(
                    format!("{lead}{seg}"),
                    theme::assistant_fg(),
                ));
            }
            // block caret at the end of the streaming text, accent-colored
            if let Some(last) = lines.last_mut() {
                last.spans
                    .push(ratatui::text::Span::styled("▊", theme::accent_fg()));
            }
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
            crossterm::cursor::Hide,
            crossterm::event::EnableMouseCapture
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let r = run_inner(&mut terminal, paths, cfg, registry, transparent, cwd).await;
        // restore before propagating the result
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            crossterm::event::DisableMouseCapture,
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
    let loop_registry = std::sync::Arc::new(loon_core::loops::LoopRegistry::new());
    let agent = Arc::new(Mutex::new({
        let mut a = Agent::new_session(
            paths,
            cfg.clone(),
            registry.clone(),
            loon_core::subagent::default_tools(
                paths,
                &cwd,
                &cfg,
                registry.clone(),
                Some(loop_registry.clone()),
            ),
            cwd.clone(),
            false,
        )
        .map_err(|e| anyhow::anyhow!("session init: {e}"))?;
        a.loops = loop_registry.clone();
        a
    }));
    let (ev_tx, mut ev_rx) = mpsc::channel::<AgentEvent>(1024);
    let (slash_tx, mut slash_rx) = mpsc::channel::<String>(64);
    agent.lock().await.set_event_sink(ev_tx.clone());
    // Ask-mode approvals: the agent (holding its own lock inside the turn) asks
    // through this lock-free channel; the UI answers via a oneshot. y approves, n or
    // Esc refuses.
    let (approval_tx, mut approval_rx) = mpsc::channel::<(
        loon_core::permissions::ApprovalRequest,
        tokio::sync::oneshot::Sender<bool>,
    )>(8);
    {
        let approval_tx = approval_tx.clone();
        let hook: loon_core::permissions::ApprovalHook =
            Arc::new(move |req: loon_core::permissions::ApprovalRequest| {
                let tx = approval_tx.clone();
                Box::pin(async move {
                    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
                    if tx.send((req, answer_tx)).await.is_err() {
                        return false;
                    }
                    answer_rx.await.unwrap_or(false)
                })
            });
        agent.lock().await.approval_hook = Some(hook);
    }

    let status = StatusLine {
        model: cfg.model_ref.clone(),
        member: None,
        mode: "bypass".into(),
        context_est: 0,
        busy: false,
        cwd: cwd.display().to_string(),
    };
    let mut app = App::new(transparent, status);
    app.push_welcome_banner();

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(32);

    // Driver: one turn at a time; cancellation stays responsive while a turn runs.
    // The cancel handle is lock-free on purpose: the turn task holds the agent mutex,
    // so cancel must never queue behind it.
    let cancel_handle = agent.lock().await.cancel_handle();
    let mut prompt_queue = queue::PromptQueue::new(16);
    let driver_slash_tx = slash_tx.clone();
    let driver = tokio::spawn(async move {
        let mut running: Option<(
            tokio::task::JoinHandle<TurnResult>,
            mpsc::Sender<AgentEvent>,
        )> = None;
        loop {
            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => c,
                    None => break,
                },
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    // Loop-task scheduler: due prompts enter the normal queue (or start
                    // immediately when idle) — the same path as a typed submission.
                    for (_id, prompt) in loop_registry.take_due() {
                        if running.is_some() {
                            let _ = prompt_queue.push(prompt);
                        } else {
                            let a = agent.clone();
                            let tx = ev_tx.clone();
                            running = Some((
                                tokio::spawn(async move {
                                    let mut guard = a.lock().await;
                                    guard.run_turn(&prompt).await
                                }),
                                tx,
                            ));
                        }
                    }
                    continue;
                }
            };
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
                    let _ = cancel_handle.send(true);
                }
                UiCommand::CycleMode => {
                    let a = agent.clone();
                    let tx = ev_tx.clone();
                    tokio::spawn(async move {
                        let mut guard = a.lock().await;
                        let next_mode =
                            loon_core::Agent::next_permission_mode(guard.cfg.permissions.mode);
                        guard.cfg.permissions.mode = next_mode;
                        let next = match next_mode {
                            loon_config::PermissionMode::Bypass => "bypass",
                            loon_config::PermissionMode::Auto => "auto",
                            loon_config::PermissionMode::Ask => "ask",
                            loon_config::PermissionMode::Allowlist => "allowlist",
                        };
                        let _ = tx.send(AgentEvent::ModeChanged { mode: next.into() }).await;
                    });
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
        if let Some(text) = app.pending_copy.take() {
            use std::io::Write;
            let b64 = {
                use std::fmt::Write as _;
                let mut out = String::new();
                const T: &[u8; 64] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                let bytes = text.as_bytes();
                for chunk in bytes.chunks(3) {
                    let b = [
                        chunk[0],
                        chunk.get(1).copied().unwrap_or(0),
                        chunk.get(2).copied().unwrap_or(0),
                    ];
                    let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                    let _ = write!(
                        out,
                        "{}{}{}{}",
                        T[(n >> 18) as usize & 63],
                        T[(n >> 12) as usize & 63],
                        if chunk.len() > 1 {
                            T[(n >> 6) as usize & 63]
                        } else {
                            b'='
                        },
                        if chunk.len() > 2 {
                            T[n as usize & 63]
                        } else {
                            b'='
                        },
                    );
                }
                out
            };
            let _ = write!(std::io::stdout(), "\x1b]52;c;{b64}\x07");
            let _ = std::io::stdout().flush();
        }
        while let Ok((req, answer_tx)) = approval_rx.try_recv() {
            app.push_block(Block::System(format!(
                "approve? (y/n) {} {}",
                req.tool, req.args_excerpt
            )));
            app.pending_approval = Some((req.tool, req.args_excerpt, answer_tx));
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
                AgentEvent::ModeChanged { mode } => app.status.mode = mode,
                AgentEvent::ToggleTheme => app.transparent = !app.transparent,
                AgentEvent::OpenConfigMenu => app.settings = Some(0),
                AgentEvent::OpenModelPicker { refs } => app.model_picker = Some((refs, 0)),
                AgentEvent::OpenEffortPicker => app.effort_picker = Some(1),
                AgentEvent::ClearTranscript => {
                    let banner = if matches!(app.blocks.first(), Some(crate::Block::Rich(_))) {
                        app.blocks.first().cloned()
                    } else {
                        None
                    };
                    app.blocks.clear();
                    if let Some(b) = banner {
                        app.blocks.push(b);
                    }
                    app.scroll_from_bottom = None;
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
        app.update_intro();
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.quit {
            break;
        }

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CtEvent::Key(key))) => handle_key(&mut app, key, &cmd_tx).await,
                    Some(Ok(CtEvent::Mouse(m))) => {
                        if let Some(text) = handle_mouse(&mut app, m) {
                            // OSC 52: hand the selection to the terminal clipboard
                            use std::io::Write;
                            let b64 = {
                                use std::fmt::Write as _;
                                let mut out = String::new();
                                const T: &[u8; 64] =
                                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                                let bytes = text.as_bytes();
                                for chunk in bytes.chunks(3) {
                                    let b = [
                                        chunk[0],
                                        chunk.get(1).copied().unwrap_or(0),
                                        chunk.get(2).copied().unwrap_or(0),
                                    ];
                                    let n = ((b[0] as u32) << 16)
                                        | ((b[1] as u32) << 8)
                                        | b[2] as u32;
                                    let _ = write!(
                                        out,
                                        "{}{}{}{}",
                                        T[(n >> 18) as usize & 63],
                                        T[(n >> 12) as usize & 63],
                                        if chunk.len() > 1 {
                                            T[(n >> 6) as usize & 63]
                                        } else {
                                            b'='
                                        },
                                        if chunk.len() > 2 {
                                            T[n as usize & 63]
                                        } else {
                                            b'='
                                        },
                                    );
                                }
                                out
                            };
                            let _ = write!(
                                std::io::stdout(),
                                "\x1b]52;c;{b64}\x07"
                            );
                            let _ = std::io::stdout().flush();
                            app.push_block(Block::System(
                                "copied selection to clipboard".into(),
                            ));
                        }
                    }
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
    // The effort picker is modal: arrows move, Enter sets, Esc closes.
    if let Some(selected) = app.effort_picker {
        const LEVELS: [&str; 3] = ["low", "medium", "high"];
        match key.code {
            KeyCode::Up => app.effort_picker = Some(if selected == 0 { 2 } else { selected - 1 }),
            KeyCode::Down => app.effort_picker = Some((selected + 1) % 3),
            KeyCode::Esc | KeyCode::Char('q') => app.effort_picker = None,
            KeyCode::Enter | KeyCode::Char(' ') => {
                let level = LEVELS[selected].to_string();
                let _ = cmd_tx
                    .send(UiCommand::Slash(
                        loon_core::commands::SlashCommand::Effort { level },
                    ))
                    .await;
                app.effort_picker = None;
            }
            _ => {}
        }
        return;
    }
    // The model picker is modal: arrows move, Enter switches, Esc closes.
    if let Some((refs, selected)) = app.model_picker.clone() {
        match key.code {
            KeyCode::Up => {
                app.model_picker = Some((
                    refs.clone(),
                    if selected == 0 {
                        refs.len() - 1
                    } else {
                        selected - 1
                    },
                ));
            }
            KeyCode::Down => {
                app.model_picker = Some((refs.clone(), (selected + 1) % refs.len()));
            }
            KeyCode::Esc | KeyCode::Char('q') => app.model_picker = None,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(r) = refs.get(selected).cloned() {
                    let _ = cmd_tx
                        .send(UiCommand::Slash(loon_core::commands::SlashCommand::Model {
                            reference: Some(r),
                        }))
                        .await;
                }
                app.model_picker = None;
            }
            _ => {}
        }
        return;
    }
    // The settings menu is modal: arrows move, Enter toggles/cycles, Esc/q closes.
    if let Some(selected) = app.settings {
        const ITEMS: usize = 7;
        match key.code {
            KeyCode::Up => {
                app.settings = Some(if selected == 0 {
                    ITEMS - 1
                } else {
                    selected - 1
                });
            }
            KeyCode::Down => {
                app.settings = Some((selected + 1) % ITEMS);
            }
            KeyCode::Esc | KeyCode::Char('q') => app.settings = None,
            KeyCode::Enter | KeyCode::Char(' ') => match selected {
                0 => {
                    let _ = cmd_tx.send(UiCommand::CycleMode).await;
                }
                1 => {
                    app.settings_autocompact = !app.settings_autocompact;
                    let _ = cmd_tx
                        .send(UiCommand::Slash(
                            loon_core::commands::SlashCommand::AutoCompact {
                                setting: "toggle".into(),
                            },
                        ))
                        .await;
                }
                2 => {
                    let next = match app.settings_effort.as_str() {
                        "low" => "medium",
                        "medium" => "high",
                        _ => "low",
                    };
                    app.settings_effort = next.into();
                    let _ = cmd_tx
                        .send(UiCommand::Slash(
                            loon_core::commands::SlashCommand::Effort { level: next.into() },
                        ))
                        .await;
                }
                3 => {
                    let next = match app.settings_max_steps {
                        12 => 24,
                        24 => 48,
                        48 => 96,
                        _ => 12,
                    };
                    app.settings_max_steps = next;
                    let _ = cmd_tx
                        .send(UiCommand::Slash(
                            loon_core::commands::SlashCommand::MaxSteps { value: next },
                        ))
                        .await;
                }
                4 => {
                    let next = match app.settings_tool_cap {
                        4_096 => 65_536,
                        65_536 => 262_144,
                        _ => 4_096,
                    };
                    app.settings_tool_cap = next;
                    let _ = cmd_tx
                        .send(UiCommand::Slash(
                            loon_core::commands::SlashCommand::ToolCap { bytes: next },
                        ))
                        .await;
                }
                5 => {
                    let _ = cmd_tx
                        .send(UiCommand::Slash(loon_core::commands::SlashCommand::Theme))
                        .await;
                }
                _ => app.settings = None,
            },
            _ => {}
        }
        return;
    }
    // A pending approval is modal: y approves, n or Esc refuses, everything else is
    // swallowed so the turn cannot be poked while it waits.
    if let Some((tool, _excerpt, answer_tx)) = app.pending_approval.take() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let _ = answer_tx.send(true);
                app.push_block(Block::System(format!("approved: {tool}")));
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                let _ = answer_tx.send(false);
                app.push_block(Block::System(format!("refused: {tool}")));
            }
            other => {
                // not an answer: keep waiting
                app.pending_approval = Some((tool, _excerpt, answer_tx));
                let _ = other;
            }
        }
        return;
    }
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
            app.cursor = 0;
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
            if app.cursor > 0 {
                let char_idx = app.cursor - 1;
                let byte = app.cursor_byte_offset(char_idx);
                let byte_end = app.cursor_byte_offset(char_idx + 1);
                app.input.replace_range(byte..byte_end, "");
                app.cursor -= 1;
            }
            app.input_changed();
        }
        KeyCode::Left => {
            app.cursor = app.cursor.saturating_sub(1);
        }
        KeyCode::Right => {
            app.cursor = (app.cursor + 1).min(app.input.chars().count());
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.chars().count(),
        // single-line input: Up/Down jump to the ends (popup navigation keeps
        // priority when the popup is visible)
        KeyCode::Up if app.completion_count() == 0 => app.cursor = 0,
        KeyCode::Down if app.completion_count() == 0 => app.cursor = app.input.chars().count(),
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
        KeyCode::BackTab => {
            let _ = cmd_tx.send(UiCommand::CycleMode).await;
        }
        KeyCode::Esc if app.completion_count() > 0 => {
            app.popup_dismissed = true;
        }
        // Esc advances: with a turn running, stop it — the driver automatically starts
        // the next queued prompt, so with a queue Esc means "skip to the next one",
        // and a second Esc stops that. Idle, it does nothing.
        KeyCode::Esc if app.status.busy => {
            let _ = cmd_tx.send(UiCommand::Cancel).await;
            if !app.cancel_hint_shown {
                app.push_block(Block::System(
                    "stopping… queued prompts will run next".into(),
                ));
                app.cancel_hint_shown = true;
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let byte = app.cursor_byte_offset(app.cursor);
            app.input.insert(byte, c);
            app.cursor += 1;
            app.input_changed();
        }
        _ => {}
    }
}

/// Mouse: wheel scrolls the transcript; click positions the input cursor; click on a
/// popup row selects that completion; drag across the transcript selects text, and
/// releasing copies it — returns the selected text for the caller to put on the
/// clipboard (OSC 52).
fn handle_mouse(app: &mut App, m: crossterm::event::MouseEvent) -> Option<String> {
    use crossterm::event::{MouseButton, MouseEventKind};
    match m.kind {
        MouseEventKind::ScrollUp => {
            let next = app.scroll_from_bottom.unwrap_or(0).saturating_add(3);
            app.scroll_from_bottom = Some(next);
            None
        }
        MouseEventKind::ScrollDown => {
            match app.scroll_from_bottom {
                Some(0) | None => app.scroll_from_bottom = None,
                Some(n) => app.scroll_from_bottom = Some(n.saturating_sub(3)),
            }
            None
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.selection = None; // a fresh click clears any highlight
                                  // popup rows select+accept
            if let Some((_px, py, ph)) = app.popup_rect {
                if m.row > py && m.row < py + ph {
                    let row = (m.row - py - 1) as usize; // skip border row
                    let completions = app.visible_completions();
                    let idx = app.popup_window_start + row;
                    if idx < completions.len() {
                        app.completion_selected = idx;
                        app.accept_completion();
                    }
                    return None;
                }
            }
            // transcript rows begin a selection
            if let Some(point) = transcript_point(app, m.column, m.row) {
                app.selection = Some(SelRange {
                    start_line: point.0,
                    start_col: point.1,
                    end_line: point.0,
                    end_col: point.1,
                });
                return None;
            }
            // input row positions the text cursor
            if let Some((ix, iy)) = app.input_rect {
                if m.row == iy && m.column > ix {
                    app.cursor = click_col_to_cursor(m.column, ix, app.input.chars().count());
                }
            }
            None
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let (Some(point), Some(_)) = (transcript_point(app, m.column, m.row), app.selection)
            {
                if let Some(sel) = app.selection.as_mut() {
                    sel.end_line = point.0;
                    sel.end_col = point.1;
                }
            }
            None
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let sel = app.selection?;
            let text = selected_text(app, sel);
            if text.trim().is_empty() {
                app.selection = None;
                return None;
            }
            app.selection = None; // copied: highlight served its purpose
            Some(text)
        }
        _ => None,
    }
}

/// Absolute (transcript-line, char-col) under the cursor, or None outside the
/// transcript area.
fn transcript_point(app: &App, column: u16, row: u16) -> Option<(usize, usize)> {
    let (tx, ty, tw, th) = app.transcript_rect?;
    if row < ty || row >= ty + th || column < tx || column >= tx + tw {
        return None;
    }
    let lines = app.transcript_lines(tw);
    let total = lines.len() as u16;
    let visible = th;
    let start = match app.scroll_from_bottom {
        None => total.saturating_sub(visible),
        Some(off) => total.saturating_sub(off).saturating_sub(visible),
    };
    let line = start as usize + (row - ty) as usize;
    if line >= lines.len() {
        return None;
    }
    let col = (column - tx) as usize;
    Some((line, col))
}

/// The selected substrings joined with newlines.
fn selected_text(app: &App, sel: SelRange) -> String {
    let lines = app.transcript_lines(200); // width only affects wrapping; spans are what we copy
    let (first_l, first_c, last_l, last_c) = if sel.start_line <= sel.end_line {
        (sel.start_line, sel.start_col, sel.end_line, sel.end_col)
    } else {
        (sel.end_line, sel.end_col, sel.start_line, sel.start_col)
    };
    let mut out = Vec::new();
    let last = last_l.min(lines.len().saturating_sub(1));
    for (idx, line) in lines.iter().enumerate().take(last + 1).skip(first_l) {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let chars: Vec<char> = text.chars().collect();
        let begin = if idx == first_l {
            first_c.min(chars.len())
        } else {
            0
        };
        let stop = if idx == last {
            last_c.min(chars.len())
        } else {
            chars.len()
        };
        if stop > begin {
            out.push(chars[begin..stop].iter().collect::<String>());
        }
    }
    out.join("\n")
}

/// Map a clicked column to a char offset in the input line. The input row renders as
/// `› {input}` starting at inner x; clicking anywhere past the end clamps to the end.
fn click_col_to_cursor(column: u16, inner_x: u16, input_chars: usize) -> usize {
    let offset = (column as usize).saturating_sub(inner_x as usize + 2); // prompt glyph + space
    offset.min(input_chars)
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

    #[test]
    fn pending_tool_never_shows_a_checkmark_and_stream_caret_renders() {
        let mut app = app_for_test();
        app.tool_started("bash", "{\"command\": \"cargo test\"}");
        let lines = app.transcript_lines(80);
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("✓ bash"),
            "pending must not claim success: {joined}"
        );
        assert!(
            joined.contains(&format!("{}", app.spinner_glyph())),
            "{joined}"
        );
        // streaming: caret span appended with accent fg
        app.stream_text("working on it");
        let lines = app.transcript_lines(80);
        let last = lines.last().unwrap();
        let caret = last.spans.last().unwrap();
        assert_eq!(caret.content, "▊");
        assert_eq!(caret.style.fg, Some(ratatui::style::Color::Cyan));
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
