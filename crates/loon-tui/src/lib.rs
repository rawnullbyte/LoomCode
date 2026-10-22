//! Loon's terminal UI.
//!
//! Rendering invariant: in transparent mode **every application-owned cell uses the
//! terminal default background** (`Color::Reset`). All styles come from [`theme`]; a test
//! walks rendered buffers asserting the invariant. Selection and highlights are done with
//! foreground color/underline — never reverse video. Opaque mode is explicit opt-in.

pub mod clipboard;
pub mod driver;
pub mod manager;
pub mod motion;
pub mod request_rate;
pub mod theme;
pub mod transcript;
pub mod ui;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
pub mod autocomplete;
pub mod banner;
pub mod queue;

use loon_core::{Agent, AgentConfig, AgentEvent, TurnStatus};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone)]
pub enum UiCommand {
    Submit(String),
    /// Ctrl+Enter: cancel the running turn now, then run this prompt immediately
    /// (never queued).
    SendNow(String),
    /// Hard stop: signal the cancel handle and abort the running turn task.
    Cancel,
    /// Remove the most recently queued prompt (Up on an empty input).
    TakeLastQueued,
    Slash(loon_core::commands::SlashCommand),
    CycleMode,
}

/// Driver → UI state that is not an [`AgentEvent`]. Prompt-queue mirror plus prompt
/// restoration when a cancelled turn hands its text back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiNotice {
    Queue(Vec<String>),
    /// A cancelled turn's prompt: put it back in the input bar (Esc restores the draft).
    RestorePrompt(String),
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
        /// Human-facing command/path, e.g. `$ cargo test`.
        detail: String,
        /// When the call started, so a running call can show elapsed time.
        started: Option<std::time::Instant>,
        /// How long the finished call took.
        elapsed: Option<std::time::Duration>,
    },
    System(String),
    Member(String),
    /// Pre-styled lines rendered verbatim, scrolls away with the transcript.
    Rich(Vec<ratatui::text::Line<'static>>),
    /// The animated opening banner. Drawn from the animation clock at render time, so
    /// it is a normal transcript block that scrolls away instead of a fixed header.
    Banner,
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
    pub navigator: Option<manager::Navigator>,
    pub editor: Option<manager::Form>,
    pub pending_editor: Option<manager::Form>,
    pub palette: String,
    pub theme_restore: Option<(String, bool)>,
    pub settings_return: Option<manager::Navigator>,
    pub manager_notice: String,
    pub context_limit: u64,
    pub activity: String,
    pub animations: bool,
    pub popup_rows: usize,
    pub tip_text: String,
    pub footer: bool,
    pub welcome_banner: bool,
    pub spinner_verbs: Vec<String>,
    pub ascii_only: bool,
    pub color_mode: String,
    pub show_tool_details: bool,
    pub request_rate: request_rate::RequestRate,
    pub modal_age: u64,
    pub work_state: loon_core::worklog::WorkState,
    pub selection_snapshot: Option<Vec<transcript::TranscriptRow>>,
    pub viewport_area: Option<ratatui::layout::Rect>,
    pub input: String,
    /// Scroll offset from the bottom in lines; `None` means follow (stick to latest).
    pub scroll_from_bottom: Option<usize>,
    pub transparent: bool,
    pub status: StatusLine,
    streaming: Option<String>,
    pub quit: bool,
    pub cancel_hint_shown: bool,
    /// Animation clock, advanced by the render loop (~20 fps).
    pub tick: u64,
    /// When the current turn started, for the elapsed-time readout.
    pub turn_started: Option<std::time::Instant>,
    /// Mirrored prompt queue, kept in sync by [`UiNotice::Queue`].
    pub queued: Vec<String>,
    /// Esc was pressed once while busy; the next Esc is a hard stop.
    pub cancel_armed: bool,
    /// Selected row in the slash-command popup; reset on input change.
    pub completion_selected: usize,
    /// Set by Esc: hide the popup until the input changes again.
    pub popup_dismissed: bool,
    /// Text cursor position in the input line, as a char offset (0..=len).
    pub cursor: usize,
    pub input_view_start: usize,
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
    /// Menu-local mirrors (optimistically updated on toggle; commands re-apply).
    pub settings_autocompact: bool,
    pub settings_effort: String,
    pub settings_tool_cap: usize,
    /// `[ui] tips`: show the rotating input placeholder.
    pub tips: bool,
}

impl App {
    pub fn new(transparent: bool, status: StatusLine) -> App {
        App {
            blocks: Vec::new(),
            navigator: None,
            editor: None,
            pending_editor: None,
            palette: if transparent { "terminal" } else { "dark" }.into(),
            theme_restore: None,
            settings_return: None,
            manager_notice: String::new(),
            context_limit: 128_000,
            activity: "Ready".into(),
            animations: true,
            popup_rows: 6,
            tip_text: String::new(),
            footer: true,
            welcome_banner: true,
            spinner_verbs: Vec::new(),
            ascii_only: false,
            color_mode: "auto".into(),
            show_tool_details: false,
            request_rate: Default::default(),
            modal_age: 0,
            work_state: Default::default(),
            selection_snapshot: None,
            viewport_area: None,
            input: String::new(),
            scroll_from_bottom: None,
            transparent,
            status,
            streaming: None,
            quit: false,
            cancel_hint_shown: false,
            tick: 0,
            turn_started: None,
            queued: Vec::new(),
            cancel_armed: false,
            completion_selected: 0,
            popup_dismissed: false,
            cursor: 0,
            input_view_start: 0,
            popup_rect: None,
            input_rect: None,
            popup_window_start: 0,
            transcript_rect: None,
            selection: None,
            pending_approval: None,
            pending_copy: None,
            settings_autocompact: true,
            settings_effort: "medium".into(),
            settings_tool_cap: 65_536,
            tips: true,
        }
    }

    /// Keep editor input available when server-side validation fails.
    /// Preserve selection/history while server-generated settings pages refresh.
    pub fn open_menu(&mut self, title: String, items: Vec<loon_core::catalog::MenuItem>) {
        let next = manager::View::Menu(title.clone(), items);
        if let Some(nav) = &mut self.navigator {
            let current = nav.title();
            if current == title {
                nav.view = next;
                nav.selected = nav.selected.min(nav.rows().len().saturating_sub(1));
                return;
            }
            if (current == "Settings" || current.starts_with("Settings / "))
                && (title == "Settings" || title.starts_with("Settings / "))
            {
                if let Some(index) = nav
                    .history
                    .iter()
                    .rposition(|(v, _, _)| matches!(v,manager::View::Menu(t,_)if t==&title))
                {
                    while nav.history.len() > index {
                        nav.back();
                    }
                    nav.view = next;
                } else if current != "Settings"
                    && title.starts_with("Settings / All configuration (")
                {
                    nav.history
                        .retain(|(v, _, _)| matches!(v,manager::View::Menu(t,_) if t=="Settings"));
                    nav.view = next;
                    nav.query.clear();
                    nav.selected = 0;
                } else {
                    nav.open(next);
                }
                return;
            }
        }
        self.navigator = Some(manager::Navigator::new(Default::default(), next));
    }
    pub fn slash_result(&mut self, out: String) {
        if let Some(mut form) = self.pending_editor.take() {
            if let Some(error) = out.strip_prefix("Error: ") {
                form.error = error.into();
                if self.navigator.is_some() {
                    self.settings_return = self.navigator.take();
                }
                self.navigator = None;
                self.editor = Some(form);
                return;
            }
        }
        if !out.trim().is_empty() {
            if self.navigator.as_ref().is_some_and(|n|matches!(&n.view,manager::View::Menu(t,_) if t=="Settings" || t.starts_with("Settings / "))) {self.manager_notice=out;}
            else{self.push_block(Block::System(out));}
        }
    }

    /// The welcome banner: version, workspace, command hints — pushed as the first
    /// transcript block so chat messages scroll it away naturally.
    pub fn push_welcome_banner(&mut self) {
        if self.welcome_banner && !self.blocks.contains(&Block::Banner) {
            self.blocks.insert(0, Block::Banner);
        }
    }

    pub fn update_intro(&mut self) {}

    /// Advance the animation clock (called once per render loop pass).
    pub fn tick(&mut self) {
        if self.animations {
            self.tick = self.tick.wrapping_add(1);
        }
        if self.navigator.is_some() || self.editor.is_some() {
            self.modal_age = self.modal_age.saturating_add(1);
        } else {
            self.modal_age = 0;
        }
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
        let insert = completions[idx].insert.clone();
        // Command-word completion replaces the whole input ("/re" → "/redo ").
        // Argument completion replaces only the word being typed ("/config ti" →
        // "/config tips "), keeping the command and earlier args intact.
        if self.input.trim_start().contains(char::is_whitespace) {
            let trimmed_end = self.input.trim_end().len();
            let cut = self.input[..trimmed_end]
                .rfind(char::is_whitespace)
                .map(|i| i + 1)
                .unwrap_or(0);
            self.input.truncate(cut);
            self.input.push_str(&insert);
            self.input.push(' ');
        } else {
            self.input = format!("/{insert} ");
        }
        self.cursor = self.input.chars().count();
        self.input_changed();
        true
    }

    pub fn rewind_conversation(&mut self, blocks: Vec<(String, String)>, prompt: String) {
        let kept: Vec<&str> = blocks
            .iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, text)| text.as_str())
            .collect();
        let live: Vec<(usize, &str)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                if let Block::User(text) = b {
                    Some((i, text.as_str()))
                } else {
                    None
                }
            })
            .collect();
        let cut = live.get(kept.len()).map(|(i, _)| *i);
        let matches = live.len() > kept.len()
            && kept
                .iter()
                .zip(&live)
                .all(|(expected, (_, actual))| expected == actual);
        if matches {
            // Retain earlier tool/status rows, formatting, and the welcome banner.
            self.blocks.truncate(cut.unwrap_or(0));
        } else {
            self.blocks = blocks
                .into_iter()
                .map(|(role, text)| {
                    if role == "user" {
                        Block::User(text)
                    } else {
                        Block::Assistant(text)
                    }
                })
                .collect();
        }
        self.streaming = None;
        self.input = prompt;
        self.cursor = self.input.chars().count();
        self.input_view_start = 0;
        self.scroll_from_bottom = None;
        self.navigator = None;
        self.editor = None;
        self.pending_editor = None;
        self.status.busy = false;
        self.activity = "Ready".into();
        self.turn_started = None;
        self.resized();
        self.input_changed();
        // A restored message starting with '/' is draft text, not an open menu.
        self.popup_dismissed = true;
    }

    /// Invalidate coordinates from the previous terminal geometry, retaining text,
    /// cursor and menu selection so shrinking/growing never discards a draft.
    pub fn resized(&mut self) {
        self.selection = None;
        self.selection_snapshot = None;
        self.input_view_start = 0;
        self.popup_rect = None;
        self.input_rect = None;
        self.transcript_rect = None;
        if let Some(nav) = &mut self.navigator {
            nav.rect = None;
        }
        if let Some(form) = &mut self.editor {
            form.rect = None;
            if let Some(picker) = &mut form.picker {
                picker.rect = None;
            }
        }
    }

    /// Input split into logical lines; a trailing newline leaves a final empty line.
    pub fn input_lines(&self) -> Vec<&str> {
        self.input.split('\n').collect()
    }

    /// (line index, char column) of the text cursor.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before = &self.input[..self.cursor_byte_offset(self.cursor)];
        let line = before.matches('\n').count();
        let col = before
            .rsplit('\n')
            .next()
            .map(|s| s.chars().count())
            .unwrap_or(0);
        (line, col)
    }

    /// Char offset of a (line, column) position, clamped to that line's length.
    pub fn offset_at(&self, line: usize, col: usize) -> usize {
        let mut offset = 0;
        for (i, text) in self.input.split('\n').enumerate() {
            if i == line {
                return offset + col.min(text.chars().count());
            }
            offset += text.chars().count() + 1;
        }
        self.input.chars().count()
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

    /// Apply driver-side state (prompt-queue mirror, or a restored draft).
    pub fn apply_notice(&mut self, notice: UiNotice) {
        match notice {
            UiNotice::Queue(items) => self.queued = items,
            UiNotice::RestorePrompt(text) => {
                // The cancelled turn was forgotten agent-side, so every transcript row it
                // produced is stale UI state. Walk back from the tail dropping its rows and
                // stop at (and including) the user message that opened it. The welcome
                // banner and anything before that message are never touched.
                while let Some(last) = self.blocks.last() {
                    match last {
                        Block::Banner | Block::Rich(_) => break,
                        Block::User(_) => {
                            self.blocks.pop();
                            break;
                        }
                        Block::Assistant(_)
                        | Block::Member(_)
                        | Block::System(_)
                        | Block::Tool { .. } => {
                            self.blocks.pop();
                        }
                    }
                }
                self.streaming = None;
                self.selection = None;
                self.selection_snapshot = None;
                // The cancelled prompt comes back as editable text: cursor at the end,
                // the popup stays closed so the draft is not mistaken for a command.
                self.input = text;
                self.cursor = self.input.chars().count();
                self.input_view_start = 0;
                self.popup_dismissed = true;
            }
        }
    }

    /// A fresh turn: its clock and token estimate start at zero.
    fn begin_turn(&mut self) {
        self.turn_started = Some(std::time::Instant::now());
        self.request_rate = Default::default();
    }

    pub fn push_block(&mut self, b: Block) {
        self.blocks.push(b);
        if self.selection.is_none() && self.scroll_from_bottom.is_none() {
            self.scroll_from_bottom = None;
        }
    }

    pub fn stream_text(&mut self, delta: &str) {
        self.streaming
            .get_or_insert_with(String::new)
            .push_str(delta);
        self.request_rate.add_chars(delta.chars().count() as u64);
        // Keep the viewport stable when the user is reading or selecting.
    }

    pub fn commit_stream(&mut self) {
        if let Some(s) = self.streaming.take() {
            if !s.trim().is_empty() {
                self.push_block(Block::Assistant(s));
            }
        }
    }

    pub fn tool_started(&mut self, name: &str, args_excerpt: &str, detail: &str) {
        self.commit_stream();
        self.push_block(Block::Tool {
            name: name.to_string(),
            ok: true,
            excerpt: format!("… {}", args_excerpt.lines().next().unwrap_or("")),
            pending: true,
            detail: detail.to_string(),
            started: Some(std::time::Instant::now()),
            elapsed: None,
        });
    }

    /// Turn over: no tool call may keep claiming it is still running. Blocks whose
    /// result never arrived (cancelled turn, dropped stream) flip to ✗ cancelled.
    pub fn finish_all_pending(&mut self) {
        for b in &mut self.blocks {
            if let Block::Tool { name, pending, .. } = b {
                if *pending {
                    *b = Block::Tool {
                        name: name.clone(),
                        ok: false,
                        excerpt: "cancelled — no result arrived".into(),
                        pending: false,
                        detail: String::new(),
                        started: None,
                        elapsed: None,
                    };
                }
            }
        }
    }

    pub fn tool_finished(&mut self, name: &str, ok: bool, excerpt: &str) {
        // Replace the newest pending block for this tool, if present.
        if let Some(pos) = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::Tool { pending: true, name: n, .. } if n == name))
        {
            let first_lines: String = excerpt.lines().take(3).collect::<Vec<_>>().join(" ⏎ ");
            let (detail, elapsed) = match &self.blocks[pos] {
                Block::Tool {
                    detail, started, ..
                } => (detail.clone(), started.map(|s| s.elapsed())),
                _ => (String::new(), None),
            };
            self.blocks[pos] = Block::Tool {
                name: name.to_string(),
                ok,
                excerpt: if first_lines.is_empty() {
                    "(no output)".into()
                } else {
                    first_lines
                },
                pending: false,
                detail,
                started: None,
                elapsed,
            };
        } else {
            self.push_block(Block::Tool {
                name: name.to_string(),
                ok,
                excerpt: excerpt.to_string(),
                pending: false,
                detail: String::new(),
                started: None,
                elapsed: None,
            });
        }
    }
}

pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    transcript::wrap(&transcript::clean(text), width)
}

/// Enable the progressive keyboard protocol so modified keys (Shift+Enter, Ctrl+Enter,
/// Ctrl+Backspace) are reported distinctly. Best-effort: terminals that do not answer
/// the query keep working with the classic encoding.
fn push_keyboard_enhancement(stdout: &mut std::io::Stdout) -> bool {
    use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    let supported = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if !supported {
        return false;
    }
    crossterm::execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )
    .is_ok()
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
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableMouseCapture
        )?;
        // Without the progressive keyboard protocol a terminal cannot distinguish
        // Shift+Enter, Ctrl+Enter or Ctrl+Backspace — they arrive as plain Enter or
        // DEL. Ask for disambiguated key events when the terminal supports them.
        let keyboard_enhanced = push_keyboard_enhancement(&mut stdout);
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let r = run_inner(&mut terminal, paths, cfg, registry, transparent, cwd).await;
        // restore before propagating the result
        if keyboard_enhanced {
            let _ = crossterm::execute!(
                terminal.backend_mut(),
                crossterm::event::PopKeyboardEnhancementFlags
            );
        }
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            crossterm::event::DisableBracketedPaste,
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
        mode: format!("{:?}", cfg.permissions.mode).to_lowercase(),
        context_est: 0,
        busy: false,
        cwd: cwd.display().to_string(),
    };
    let mut app = App::new(transparent, status);
    if let Ok((loaded, _)) = loon_config::Config::load_effective(paths, &cwd) {
        app.palette = loaded.appearance.palette;
        app.settings_effort = loaded.session.effort.unwrap_or("medium".into());
        app.tips = loaded.ui.tips;
        app.animations = loaded.ui.animations;
        app.popup_rows = loaded.ui.popup_rows as usize;
        app.tip_text = loaded.ui.tip_text;
        app.footer = loaded.ui.footer;
        app.welcome_banner = loaded.ui.welcome_banner;
        app.spinner_verbs = loaded.ui.spinner_verbs;
        app.ascii_only = loaded.appearance.ascii_only;
        app.color_mode = serde_json::to_value(loaded.appearance.color)
            .unwrap()
            .as_str()
            .unwrap()
            .into();
    }
    app.work_state = agent.lock().await.worklog.read().unwrap_or_default();
    app.context_limit = cfg.context_window;
    app.settings_tool_cap = cfg.tool_output_bytes;
    app.settings_autocompact = cfg.context_window > 0;
    app.push_welcome_banner();

    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(32);
    let (notice_tx, mut notice_rx) = mpsc::channel::<UiNotice>(32);

    let driver = tokio::spawn(driver::run(
        agent.clone(),
        cmd_rx,
        ev_tx.clone(),
        notice_tx,
        slash_tx.clone(),
        loop_registry,
    ));

    let mut events = EventStream::new();
    // Fixed-schedule animation clock: unlike a sleep future (which select! cancels
    // and restarts on every event), interval keeps its own deadline, so constant
    // mouse motion cannot stall or accelerate the animation.
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        while let Ok(notice) = notice_rx.try_recv() {
            app.apply_notice(notice);
        }
        while let Ok(out) = slash_rx.try_recv() {
            app.slash_result(out);
        }
        if let Some(text) = app.pending_copy.take() {
            // System clipboard first (terminals frequently reject OSC 52); OSC 52 is
            // the fallback for ssh/headless setups.
            let delivered = clipboard::copy(&text).await;
            if !delivered {
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
                AgentEvent::TextDelta(t) => {
                    app.activity = "Writing".into();
                    app.stream_text(&t);
                }
                AgentEvent::StreamReset { note } => {
                    // The attempt is being retried: drop the half-written answer instead
                    // of appending a second one to it.
                    app.streaming = None;
                    app.request_rate.start();
                    app.activity = "Retrying".into();
                    app.push_block(Block::System(note));
                }
                AgentEvent::CatalogUpdated { catalog } => {
                    if let Some(nav) = &mut app.navigator {
                        nav.catalog = catalog;
                    }
                }
                AgentEvent::OpenCatalog { view, catalog } => {
                    if app.navigator.as_ref().is_some_and(
                        |n| matches!(&n.view,manager::View::Menu(t,_) if t=="Settings" || t.starts_with("Settings / ")),
                    ) {
                        app.settings_return = app.navigator.clone();
                    }
                    app.navigator = Some(manager::Navigator::from_catalog(catalog, &view))
                }
                AgentEvent::OpenEditor(editor) => {
                    if app.navigator.as_ref().is_some_and(
                        |n| matches!(&n.view,manager::View::Menu(t,_) if t=="Settings" || t.starts_with("Settings / ")),
                    ) {
                        app.settings_return = app.navigator.clone();
                    }
                    app.navigator = None;
                    app.editor = Some(manager::Form::new(editor));
                }
                AgentEvent::OpenMenu { title, items } => {
                    if title == "Appearance"
                        && app
                            .navigator
                            .as_ref()
                            .map(|n| matches!(&n.view,manager::View::Menu(t,_) if t=="Settings" || t.starts_with("Settings / ")))
                            .unwrap_or(false)
                    {
                        app.settings_return = app.navigator.clone();
                    }
                    app.open_menu(title, items);
                }
                AgentEvent::SetPalette { name } => {
                    app.transparent = name == "terminal";
                    app.palette = name;
                }
                AgentEvent::ContextUpdated { tokens } => app.status.context_est = tokens,
                AgentEvent::ModelChanged { model } => app.status.model = model,
                AgentEvent::EffortChanged { level } => app.settings_effort = level,
                AgentEvent::ConversationRewound { blocks, prompt } => {
                    app.rewind_conversation(blocks, prompt);
                    terminal.clear()?;
                }
                AgentEvent::SessionLoaded { blocks } => {
                    app.blocks = blocks
                        .into_iter()
                        .map(|(role, text)| {
                            if role == "user" {
                                Block::User(text)
                            } else {
                                Block::Assistant(text)
                            }
                        })
                        .collect();
                    app.streaming = None;
                    app.scroll_from_bottom = None;
                    app.selection = None;
                    app.selection_snapshot = None;
                    app.navigator = None;
                }
                AgentEvent::WorkStateChanged { state } => app.work_state = state,
                AgentEvent::TurnStarted => {
                    app.status.busy = true;
                    app.activity = "Waiting for provider".into();
                    app.begin_turn();
                    app.cancel_hint_shown = false;
                    app.cancel_armed = false;
                }
                AgentEvent::RequestStarted => {
                    app.request_rate.start();
                    app.activity = "Waiting for provider".into();
                }
                AgentEvent::RequestUsage { output_tokens } => {
                    app.request_rate.report(output_tokens)
                }
                AgentEvent::RequestFinished => app.request_rate.finish(),
                AgentEvent::OutputProgress { chars } => app.request_rate.add_chars(chars),
                AgentEvent::ReasoningUsage { .. } => {} // retained JSONL compatibility; never double-count usage
                AgentEvent::ReasoningDelta(text) => {
                    app.request_rate.add_chars(text.chars().count() as u64);
                    app.activity = "Thinking".into();
                }
                AgentEvent::UiPreference { key, value } => match key.as_str() {
                    "animations" => app.animations = value == "true",
                    "popup_rows" => app.popup_rows = value.parse().unwrap_or(6),
                    "context_window" => app.context_limit = value.parse().unwrap_or(0),
                    "tips" => app.tips = value == "true",
                    "tip_text" => app.tip_text = value,
                    "footer" => app.footer = value == "true",
                    "welcome_banner" => {
                        app.welcome_banner = value == "true";
                        if app.welcome_banner {
                            app.push_welcome_banner();
                        } else {
                            app.blocks.retain(|b| !matches!(b, Block::Banner));
                        }
                    }
                    "spinner_verbs" => {
                        app.spinner_verbs = serde_json::from_str(&value).unwrap_or_default()
                    }
                    "ascii_only" => app.ascii_only = value == "true",
                    "color" => app.color_mode = value,
                    _ => {}
                },
                AgentEvent::ToolStarted {
                    name,
                    args_excerpt,
                    detail,
                } => {
                    app.activity = format!("Running {name}");
                    app.tool_started(&name, &args_excerpt, &detail)
                }
                AgentEvent::ToolFinished { name, ok, excerpt } => {
                    app.tool_finished(&name, ok, &excerpt)
                }
                AgentEvent::ModeChanged { mode } => app.status.mode = mode,
                AgentEvent::ToggleTheme => app.transparent = !app.transparent,
                AgentEvent::SetTransparent { transparent } => app.transparent = transparent,
                AgentEvent::OpenConfigMenu => {
                    let items = loon_core::commands::settings_registry()
                        .iter()
                        .map(|d| loon_core::catalog::MenuItem {
                            label: d.key.into(),
                            detail: d.description.into(),
                            command: format!("/config edit {}", d.key),
                        })
                        .collect();
                    app.navigator = Some(manager::Navigator::new(
                        Default::default(),
                        manager::View::Menu("Settings".into(), items),
                    ));
                }
                AgentEvent::OpenModelPicker { refs } => {
                    app.navigator = Some(manager::Navigator::new(
                        loon_core::catalog::Catalog {
                            models: refs,
                            ..Default::default()
                        },
                        manager::View::AllModels(None),
                    ))
                }
                AgentEvent::OpenEffortPicker => {
                    let items = loon_config::Effort::LEVELS
                        .into_iter()
                        .map(|n| loon_core::catalog::MenuItem {
                            label: n.into(),
                            detail: loon_config::Effort::parse(n).unwrap().guidance().into(),
                            command: format!("/effort {n}"),
                        })
                        .collect();
                    app.navigator = Some(manager::Navigator::new(
                        Default::default(),
                        manager::View::Menu("Coding effort".into(), items),
                    ));
                }
                AgentEvent::OpenProviderForm => {
                    app.editor = Some(manager::Form::new(loon_core::catalog::Editor {
                        kind: "provider".into(),
                        values: vec![
                            String::new(),
                            String::new(),
                            String::new(),
                            "chat".into(),
                            String::new(),
                            String::new(),
                        ],
                        editing: false,
                        fields: vec![
                            loon_core::catalog::FieldSpec::text(),
                            loon_core::catalog::FieldSpec::text(),
                            loon_core::catalog::FieldSpec::text(),
                            loon_core::catalog::FieldSpec::choice(["chat", "responses"]),
                            loon_core::catalog::FieldSpec::text(),
                            loon_core::catalog::FieldSpec::text(),
                        ],
                    }));
                }
                AgentEvent::ClearTranscript => {
                    let banner = if matches!(
                        app.blocks.first(),
                        Some(crate::Block::Rich(_) | crate::Block::Banner)
                    ) {
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
                AgentEvent::MemberNote(m) => {
                    if app.status.busy
                        && (m.starts_with("ultraloop") || m.starts_with("verification"))
                    {
                        app.activity = m.clone();
                    }
                    app.push_block(Block::Member(m));
                }
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
                    app.request_rate.finish();
                    app.cancel_armed = false;
                    // The driver synthesises Cancelled after aborting a task that may
                    // already have emitted its own TurnFinished. Once idle, a second
                    // finish is stale and must not disturb a newly started turn.
                    if !app.status.busy {
                        continue;
                    }
                    app.commit_stream();
                    match status {
                        TurnStatus::Completed => {}
                        // Cancelled restores the prompt to the input bar (via a
                        // RestorePrompt notice), so no "cancelled" marker or status
                        // text is needed — the turn simply did not happen.
                        TurnStatus::Cancelled => {}
                        TurnStatus::Failed(e) => {
                            app.push_block(Block::System(format!("failed: {e}")))
                        }
                    }
                    app.status.busy = false;
                    app.activity = "Ready".into();
                    app.turn_started = None;
                    app.finish_all_pending();
                }
            }
        }

        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.quit {
            break;
        }

        tokio::select! {
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CtEvent::Key(key))) => handle_key(&mut app, key, &cmd_tx).await,
                    Some(Ok(CtEvent::Mouse(m))) => {
                        let clicked=app.navigator.as_ref().and_then(|n|n.rect).map(|r|m.column>=r.x && m.column<r.right() && m.row>=r.y && m.row<r.bottom()).unwrap_or(false) && matches!(m.kind,crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left));
                        if manager::mouse(&mut app,m) {if clicked{manager::key(&mut app,KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE),&cmd_tx).await;}continue; }
                        if let Some(text) = handle_mouse(&mut app, m) {
                            app.pending_copy=Some(text);
                            continue;
                        }
                    }
                    Some(Ok(CtEvent::Resize(_, _))) => {
                        app.resized();
                        // Query current size rather than replaying stale dimensions
                        // during a resize burst. Clear even if draw already autoresized.
                        terminal.autoresize()?;
                        terminal.clear()?;
                    },
                    Some(Ok(CtEvent::Paste(text))) => {
                        if let Some(form)=&mut app.editor {form.insert(&text);}
                        else if let Some(nav)=&mut app.navigator {nav.query.push_str(&transcript::clean(&text).replace('\n'," "));nav.selected=0;}
                        else {let text=transcript::clean(&text);let byte=app.cursor_byte_offset(app.cursor);app.input.insert_str(byte,&text);app.cursor+=text.chars().count();app.input_changed();}
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break, // terminal disconnected
                }
            }
            _ = ticker.tick() => {
                // Animation clock advances ONLY on the timer — event-driven
                // iterations (mouse motion especially) must not speed it up.
                app.tick();
                app.update_intro();
            }
        }
    }
    let _ = cmd_tx.send(UiCommand::Cancel).await;
    drop(cmd_tx);
    let mut driver = driver;
    if tokio::time::timeout(Duration::from_secs(16), &mut driver)
        .await
        .is_err()
    {
        driver.abort();
    }
    Ok(())
}

async fn handle_key(app: &mut App, key: KeyEvent, cmd_tx: &mpsc::Sender<UiCommand>) {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return;
    }
    // A double-Esc stop is armed by one Esc and disarmed by anything else.
    if key.code != KeyCode::Esc {
        app.cancel_armed = false;
    }
    if app.pending_approval.is_none() && manager::key(app, key, cmd_tx).await {
        return;
    }
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.show_tool_details = !app.show_tool_details;
        app.selection = None;
        app.selection_snapshot = None;
        return;
    }
    if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.pending_copy = app.blocks.iter().rev().find_map(|b| {
            if let Block::Assistant(s) = b {
                Some(s.clone())
            } else {
                None
            }
        });
        return;
    }
    if key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.selection.is_some()
    {
        app.pending_copy = app.selection.map(|s| selected_text(app, s));
        app.selection = None;
        app.selection_snapshot = None;
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
                    app.push_block(Block::System("stopping…".into()));
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
        // Ctrl+Enter (Alt+Enter as a terminal fallback): send now, ahead of the queue.
        // Ctrl+Enter: stop what is running and take over. With text, it sends that now
        // and the queued prompts follow; with an empty input it just stops the turn so
        // the queue drains. Doing nothing when both are empty would be a dead key.
        KeyCode::Enter
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let text = app.input.trim().to_string();
            if text.is_empty() && app.queued.is_empty() && !app.status.busy {
                return;
            }
            app.input.clear();
            app.cursor = 0;
            app.cancel_armed = false;
            if text.is_empty() {
                let _ = cmd_tx.send(UiCommand::SendNow(String::new())).await;
                app.push_block(Block::System("stopped — running the queue".into()));
                return;
            }
            submit(app, cmd_tx, text, true).await;
        }
        // Shift+Enter starts a new line; the input grows instead of sending.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            let byte = app.cursor_byte_offset(app.cursor);
            app.input.insert(byte, '\n');
            app.cursor += 1;
            app.input_changed();
        }
        KeyCode::Enter
            if !key.modifiers.contains(KeyModifiers::ALT)
                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let text = app.input.trim().to_string();
            app.input.clear();
            app.cursor = 0;
            if text.is_empty() {
                return;
            }
            submit(app, cmd_tx, text, false).await;
        }
        // Ctrl/Alt+Backspace deletes the word before the caret (and the whitespace
        // after it); a newline is a hard stop so a line break is never eaten silently.
        KeyCode::Backspace
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if app.cursor > 0 {
                let chars: Vec<char> = app.input.chars().collect();
                let mut start = app.cursor;
                while start > 0 && chars[start - 1].is_whitespace() && chars[start - 1] != '\n' {
                    start -= 1;
                }
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                let from = app.cursor_byte_offset(start);
                let to = app.cursor_byte_offset(app.cursor);
                app.input.replace_range(from..to, "");
                app.cursor = start;
            }
            app.input_changed();
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
        KeyCode::Home => {
            let (line, _) = app.cursor_line_col();
            app.cursor = app.offset_at(line, 0);
        }
        KeyCode::End => {
            let (line, _) = app.cursor_line_col();
            app.cursor = app.offset_at(line, usize::MAX);
        }
        // single-line input: Up/Down jump to the ends (popup navigation keeps
        // priority when the popup is visible). Up on an empty input pulls the most
        // recently queued prompt back for editing.
        KeyCode::Up
            if app.completion_count() == 0 && app.input.is_empty() && !app.queued.is_empty() =>
        {
            if let Some(text) = app.queued.pop() {
                app.input = text;
                app.cursor = app.input.chars().count();
                app.input_changed();
                let _ = cmd_tx.send(UiCommand::TakeLastQueued).await;
            }
        }
        KeyCode::Up if app.completion_count() == 0 && app.input.contains('\n') => {
            let (line, col) = app.cursor_line_col();
            app.cursor = app.offset_at(line.saturating_sub(1), col);
        }
        KeyCode::Down if app.completion_count() == 0 && app.input.contains('\n') => {
            let (line, col) = app.cursor_line_col();
            let last = app.input_lines().len().saturating_sub(1);
            app.cursor = app.offset_at((line + 1).min(last), col);
        }
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
            app.cancel_armed = false;
        }
        // Esc is a two-step hard stop: the first press only arms it, the second
        // aborts the running turn immediately. Idle, it does nothing.
        KeyCode::Esc if app.status.busy => {
            if app.cancel_armed {
                app.cancel_armed = false;
                let _ = cmd_tx.send(UiCommand::Cancel).await;
            } else {
                app.cancel_armed = true;
                app.push_block(Block::System(
                    "esc again to stop · typing continues to be queued".into(),
                ));
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

/// Shared Enter path. `send_now` cancels a running turn (in the driver) and starts
/// this prompt ahead of the queue; otherwise a running turn keeps it queued.
async fn submit(app: &mut App, cmd_tx: &mpsc::Sender<UiCommand>, text: String, send_now: bool) {
    if text == "/quit" || text == "/exit" {
        app.quit = true;
        return;
    }
    if let Some(parsed) = loon_core::commands::parse(&text) {
        match parsed {
            Ok(cmd) => {
                if send_now && app.status.busy {
                    let _ = cmd_tx.send(UiCommand::Cancel).await;
                }
                let _ = cmd_tx.send(UiCommand::Slash(cmd)).await;
            }
            Err(msg) => app.push_block(Block::System(msg)),
        }
        return;
    }
    app.cancel_hint_shown = false;
    app.cancel_armed = false;
    app.push_block(Block::User(text.clone()));
    if send_now || !app.status.busy {
        app.begin_turn();
    }
    app.status.busy = true;
    let command = if send_now {
        UiCommand::SendNow(text)
    } else {
        UiCommand::Submit(text)
    };
    let _ = cmd_tx.send(command).await;
}

/// Mouse: wheel scrolls the transcript; click positions the input cursor; click on a
/// popup row selects that completion; drag across the transcript selects text, and
/// releasing copies it — returns the selected text for the caller to put on the
/// clipboard (OSC 52).
fn handle_mouse(app: &mut App, m: crossterm::event::MouseEvent) -> Option<String> {
    use crossterm::event::{MouseButton, MouseEventKind};
    if let Some((_, py, ph)) = app.popup_rect {
        if m.row >= py
            && m.row < py + ph
            && matches!(
                m.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            let len = app.visible_completions().len();
            app.completion_selected = if m.kind == MouseEventKind::ScrollUp {
                app.completion_selected.saturating_sub(1)
            } else {
                (app.completion_selected + 1).min(len.saturating_sub(1))
            };
            return None;
        }
    }
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
            app.selection = None;
            app.selection_snapshot = None; // a fresh click clears any highlight
                                           // popup rows select+accept
            if let Some((_px, py, ph)) = app.popup_rect {
                if m.row >= py && m.row < py + ph {
                    let row = (m.row - py) as usize; // borderless list
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
                let width = app.transcript_rect.map(|(_, _, w, _)| w).unwrap_or(80);
                app.selection_snapshot = Some(app.transcript_rows(width));
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
                    app.cursor = app.input_view_start
                        + input_cell_to_cursor(
                            &app.input
                                .chars()
                                .skip(app.input_view_start)
                                .collect::<String>(),
                            m.column.saturating_sub(ix + 2) as usize,
                        );
                }
            }
            None
        }
        // Hovering a popup row highlights it (nothing is picked until a click).
        MouseEventKind::Moved => {
            if let Some((_px, py, ph)) = app.popup_rect {
                if m.row >= py && m.row < py + ph {
                    let row = (m.row - py) as usize;
                    let idx = app.popup_window_start + row;
                    if idx < app.visible_completions().len() {
                        app.completion_selected = idx;
                    }
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
            app.selection_snapshot = None;
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
    let total = lines.len();
    let visible = th as usize;
    let start = match app.scroll_from_bottom {
        None => total.saturating_sub(visible),
        Some(off) => total.saturating_sub(off).saturating_sub(visible),
    };
    let line = start + (row - ty) as usize;
    if line >= lines.len() {
        return None;
    }
    let col = (column - tx) as usize;
    Some((line, col))
}

/// The selected substrings joined with newlines.
fn selected_text(app: &App, sel: SelRange) -> String {
    let width = app.transcript_rect.map(|(_, _, w, _)| w).unwrap_or(80);
    transcript::copy(&app.transcript_rows(width), sel)
}

fn input_cell_to_cursor(text: &str, col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut cell = 0;
    for (i, c) in text.chars().enumerate() {
        if cell + c.width().unwrap_or(0) > col {
            return i;
        }
        cell += c.width().unwrap_or(0);
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[tokio::test]
    async fn arrows_move_the_input_cursor_and_shift_enter_breaks_the_line() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = app_for_test();
        for c in "abc".chars() {
            handle_key(&mut app, key(KeyCode::Char(c), KeyModifiers::NONE), &tx).await;
        }
        assert_eq!(app.cursor, 3);
        handle_key(&mut app, key(KeyCode::Left, KeyModifiers::NONE), &tx).await;
        assert_eq!(app.cursor, 2, "left arrow moves the caret");
        handle_key(&mut app, key(KeyCode::Right, KeyModifiers::NONE), &tx).await;
        assert_eq!(app.cursor, 3);
        handle_key(&mut app, key(KeyCode::Home, KeyModifiers::NONE), &tx).await;
        assert_eq!(app.cursor, 0, "home goes to the start of the line");
        handle_key(&mut app, key(KeyCode::End, KeyModifiers::NONE), &tx).await;
        assert_eq!(app.cursor, 3);

        // Shift+Enter starts a new line instead of sending.
        handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::SHIFT), &tx).await;
        assert_eq!(app.input, "abc\n");
        assert!(app.blocks.is_empty(), "nothing was submitted");
        for c in "de".chars() {
            handle_key(&mut app, key(KeyCode::Char(c), KeyModifiers::NONE), &tx).await;
        }
        assert_eq!(app.input, "abc\nde");
        handle_key(&mut app, key(KeyCode::Up, KeyModifiers::NONE), &tx).await;
        assert_eq!(
            app.cursor, 2,
            "up moves to the same column on the line above"
        );
        handle_key(&mut app, key(KeyCode::Down, KeyModifiers::NONE), &tx).await;
        assert_eq!(app.cursor, 6, "down returns to the second line");
    }

    #[tokio::test]
    async fn ctrl_backspace_deletes_a_whole_word() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = app_for_test();
        app.input = "git commit --amend".into();
        app.cursor = app.input.chars().count();
        handle_key(
            &mut app,
            key(KeyCode::Backspace, KeyModifiers::CONTROL),
            &tx,
        )
        .await;
        assert_eq!(app.input, "git commit ");
        handle_key(
            &mut app,
            key(KeyCode::Backspace, KeyModifiers::CONTROL),
            &tx,
        )
        .await;
        assert_eq!(app.input, "git ");
        // A newline is a hard stop: the word delete never swallows the line break.
        app.input = "first\nsecond".into();
        app.cursor = app.input.chars().count();
        handle_key(
            &mut app,
            key(KeyCode::Backspace, KeyModifiers::CONTROL),
            &tx,
        )
        .await;
        assert_eq!(app.input, "first\n");
    }

    #[tokio::test]
    async fn plain_enter_sends_and_clears_the_multi_line_input() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_for_test();
        app.input = "one\ntwo".into();
        app.cursor = 7;
        handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), &tx).await;
        assert!(app.input.is_empty());
        match rx.try_recv().unwrap() {
            UiCommand::Submit(text) => assert_eq!(text, "one\ntwo"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transcript_streaming_and_tool_replace() {
        let mut app = app_for_test();
        app.stream_text("Hel");
        app.stream_text("lo world");
        app.tool_started("read", r#"{"path": "a.txt"}"#, "a.txt");
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
        app.tool_started("bash", "{\"command\": \"cargo test\"}", "$ cargo test");
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
