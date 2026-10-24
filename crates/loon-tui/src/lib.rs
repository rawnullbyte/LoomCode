//! Loon's terminal UI.
//!
//! Rendering invariant: in transparent mode **every application-owned cell uses the
//! terminal default background** (`Color::Reset`). All styles come from [`theme`]; a test
//! walks rendered buffers asserting the invariant. Selection and highlights are done with
//! foreground color/underline — never reverse video. Opaque mode is explicit opt-in.

pub mod clipboard;
pub mod code;
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
pub mod history;
pub mod queue;

use loon_core::{Agent, AgentConfig, AgentEvent, TurnStatus};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Most sent prompts kept for Up/Down recall. Enough for a long session's recent
/// work without unbounded memory growth.
pub const SENT_HISTORY_LIMIT: usize = 100;

/// State for the Ctrl+R reverse search over prompt history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorySearch {
    /// The text typed since the search opened; matching is a substring, case-insensitive.
    pub query: String,
    /// Index into `sent_history` of the match being shown, searching from the newest.
    pub index: usize,
    /// The draft that was in the input when the search opened, restored on cancel.
    pub saved_input: String,
}

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
    QueuePaused(bool),
    /// Rejected queued text is restored without deleting conversation progress.
    Draft(String),
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
    /// Bumped by every mutation of `blocks` (push, truncate, stream commit). The
    /// transcript row cache keys on it: without it, a long session re-rendered every
    /// block on every frame — measured 24 ms/frame at 2 000 blocks against a 50 ms tick,
    /// which is the "laggy after a few hours" report. Only ~30 rows are ever visible.
    blocks_generation: u64,
    /// Cached `transcript_rows` for the current width and generation, plus the cached
    /// row count so `ui.rs` can scroll before paying for the build.
    /// Rendered on the UI thread only; `RefCell` because the read path takes `&App`
    /// (selection copy and hit-testing), so the cache cannot require `&mut`.
    row_cache: std::cell::RefCell<Option<crate::transcript::TranscriptRowCache>>,
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
    pub queue_paused: bool,
    /// Sent prompts, newest last. Up on an empty input with no queued prompt
    /// walks this shell-style history instead of moving the caret; Down walks
    /// back toward the empty draft. `history_pos` is the index into
    /// `sent_history` currently shown, or `None` when the input is a live
    /// draft rather than a recalled entry.
    pub sent_history: Vec<String>,
    pub history_pos: Option<usize>,
    /// The draft the user had typed before starting to browse `sent_history`
    /// with Up; restored when Down walks back past the newest entry.
    pub history_draft: Option<String>,
    /// Prefix captured when browsing started. Up then only walks entries that *start
    /// with* what was typed, so "cargo" steps through cargo commands instead of every
    /// prompt ever sent. `None`/empty means walk the whole history.
    pub history_filter: Option<String>,
    /// Where to persist `sent_history` so recall survives a restart. `None` keeps
    /// history in memory only (tests, and any host without a state directory).
    pub history_file: Option<std::path::PathBuf>,
    /// Ctrl+R reverse search: the query typed since it opened, and which match of the
    /// history is currently shown. `None` means the search is not open.
    pub history_search: Option<HistorySearch>,
    /// Esc was pressed once while busy; the next Esc is a hard stop.
    pub cancel_armed: bool,
    /// Selected row in the slash-command popup; reset on input change.
    pub completion_selected: usize,
    /// Set by Esc: hide the popup until the input changes again.
    pub popup_dismissed: bool,
    /// Text cursor position in the input line, as a char offset (0..=len).
    pub cursor: usize,
    /// Keyboard anchor when Shift selects input text: the fixed end of the selection
    /// while the cursor moves. `None` means no keyboard selection; typing, clicking
    /// or any plain caret motion clears it.
    pub input_anchor: Option<usize>,
    pub input_view_start: usize,
    /// First input line drawn in the input box (vertical scroll), for click mapping.
    pub input_scroll: usize,
    /// Visible input content rows, for click mapping.
    pub input_rows: u16,
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
    /// `[ui] syntax`: highlight code in excerpts and fenced blocks. Off renders code in the
    /// plain body colour.
    pub syntax: bool,
    /// The background-subagent hub this session's tools share. `None` in tests that build
    /// an `App` directly; present in a real session, where the panel reads running jobs.
    pub background: Option<loon_core::subagent::BackgroundHub>,
}

/// One line describing a background subagent's state change.
fn subagent_update_block(id: u64, label: &str, status: &str, note: &str) -> Block {
    let line = format!("background #{id} {label}: {status}");
    Block::System(if note.is_empty() {
        line
    } else {
        format!("{line} — {note}")
    })
}

/// A finished background subagent's report, shown in the transcript so the work it did
/// is visible without asking the model to list its jobs.
fn subagent_finished_block(id: u64, label: &str, status: &str, text: &str) -> Block {
    Block::System(format!("background #{id} {label} {status}\n{text}"))
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
            queue_paused: false,
            sent_history: Vec::new(),
            history_pos: None,
            history_draft: None,
            history_filter: None,
            history_file: None,
            history_search: None,
            cancel_armed: false,
            completion_selected: 0,
            popup_dismissed: false,
            cursor: 0,
            input_anchor: None,
            input_view_start: 0,
            input_scroll: 0,
            input_rows: 0,
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
            syntax: true,
            background: None,
            blocks_generation: 0,
            row_cache: std::cell::RefCell::new(None),
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
            self.invalidate_rows();
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

    /// Input changed: popup returns, keyboard selection resets. History browsing
    /// state is deliberately *not* reset here: `show_recalled` loads entries
    /// programmatically, and `input_changed` is also called from paths (typing over
    /// a recall) where dropping the position would break Down. Only a submit or a
    /// driver-side draft restore ends browsing.
    pub fn input_changed(&mut self) {
        self.completion_selected = 0;
        self.popup_dismissed = false;
        self.input_anchor = None;
    }

    /// Selected prompt text, if any: the span between the Shift anchor and the cursor
    /// (either direction), normalised to (start, end).
    pub fn input_selection(&self) -> Option<(usize, usize)> {
        let anchor = self.input_anchor?;
        let (lo, hi) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        if lo == hi {
            None
        } else {
            Some((lo, hi))
        }
    }

    /// Move the caret, extending or clearing the Shift selection.
    /// With Shift, the anchor stays where selection started; without, it clears.
    pub fn move_cursor(&mut self, target: usize, extend: bool) {
        let count = self.input.chars().count();
        let target = target.min(count);
        if extend {
            if self.input_anchor.is_none() {
                self.input_anchor = Some(self.cursor);
            }
        } else {
            self.input_anchor = None;
        }
        self.cursor = target;
    }

    /// Record a prompt that was actually submitted so Up can recall it later.
    /// Slash commands are deliberately not remembered: recalling one would open the
    /// completion popup, where Up navigates the popup instead of the history. A
    /// repeat of the previous prompt is not stored twice, so resending after a
    /// cancelled turn does not pile up duplicates.
    pub fn record_sent(&mut self, text: &str) {
        self.history_pos = None;
        self.history_draft = None;
        self.history_filter = None;
        let text = text.trim();
        if text.is_empty() || text.starts_with('/') {
            return;
        }
        if self.sent_history.last().map(String::as_str) == Some(text) {
            return;
        }
        if self.sent_history.len() >= SENT_HISTORY_LIMIT {
            self.sent_history.remove(0);
        }
        self.sent_history.push(text.to_string());
        // Persist so the next launch can recall it. Best-effort: a failure here must never
        // affect sending the prompt itself.
        if let Some(path) = &self.history_file {
            if let Some(dir) = path.parent() {
                crate::history::append(dir, text);
            }
        }
    }

    /// Up steps into the history when the caret is on the first input line; on any
    /// lower line it stays ordinary line motion. A single-line draft is always on
    /// line 0, so recall starts immediately instead of spending a keystroke on
    /// "jump to the start". On line 0 of a *multi-line* draft, Up would otherwise be
    /// a dead key (it only re-targets the line it is already on), so it recalls too:
    /// the draft is kept in `history_draft` and Down brings it back intact.
    pub fn can_recall_previous(&self) -> bool {
        !self.sent_history.is_empty() && self.cursor_line_col().0 == 0
    }

    /// Down leaves a recalled entry only from its last line, so a recalled
    /// multi-line prompt still uses Down for ordinary line motion.
    pub fn can_recall_next(&self) -> bool {
        self.history_pos.is_some() && self.cursor_line_col().0 + 1 >= self.input_lines().len()
    }

    /// Show a recalled entry (or the set-aside draft) with the caret at its end.
    fn show_recalled(&mut self, text: String) {
        // A recalled prompt that happens to start with '/' is draft text, not an
        // open command menu.
        self.popup_dismissed = text.starts_with('/');
        self.input = text;
        self.cursor = self.input.chars().count();
        self.input_view_start = 0;
        self.completion_selected = 0;
        self.input_anchor = None;
    }

    /// Indices of the entries that Up may walk, newest last. With a filter captured at the
    /// start of browsing, only entries beginning with it qualify.
    fn recallable(&self) -> Vec<usize> {
        self.recallable_for(self.history_filter.as_deref().unwrap_or(""))
    }

    fn recallable_for(&self, filter: &str) -> Vec<usize> {
        if filter.is_empty() {
            return (0..self.sent_history.len()).collect();
        }
        let needle = filter.to_lowercase();
        self.sent_history
            .iter()
            .enumerate()
            .filter(|(_, text)| text.to_lowercase().starts_with(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Up: the previous (older) sent prompt, walking from newest to oldest. The
    /// live draft is kept aside so Down can bring it back unchanged.
    ///
    /// When the input holds text that prefixes at least one earlier prompt, Up walks only
    /// those — typing "cargo" steps through cargo commands instead of everything ever sent.
    /// A draft that prefixes nothing (an ordinary sentence) falls back to the whole history,
    /// so Up never becomes a dead key just because of what happens to be typed.
    pub fn recall_previous(&mut self) -> bool {
        if self.sent_history.is_empty() {
            return false;
        }
        let candidates = match self.history_pos {
            // Browsing already started: the filter was captured on the first Up.
            Some(_) => self.recallable(),
            None => {
                self.history_draft = Some(self.input.clone());
                let filtered = self.recallable_for(&self.input);
                // Use the prefix filter only when it actually narrows to something;
                // otherwise a normal draft made Up do nothing at all.
                let narrows = filtered.len() < self.sent_history.len();
                if narrows && !filtered.is_empty() {
                    self.history_filter = Some(self.input.clone());
                    filtered
                } else {
                    self.history_filter = None;
                    self.recallable_for("")
                }
            }
        };
        if candidates.is_empty() {
            return false;
        }
        let pos = match self.history_pos {
            Some(current) => {
                // Step to the next older candidate before `current`.
                match candidates.iter().rev().find(|&&i| i < current) {
                    Some(&i) => i,
                    None => return false,
                }
            }
            None => *candidates.last().expect("non-empty"),
        };
        self.history_pos = Some(pos);
        let text = self.sent_history[pos].clone();
        self.show_recalled(text);
        true
    }

    /// Down: the next (newer) sent prompt, or the draft set aside when browsing
    /// started. Returns false when the input is a live draft rather than a recall.
    pub fn recall_next(&mut self) -> bool {
        let Some(pos) = self.history_pos else {
            return false;
        };
        let candidates = self.recallable();
        if let Some(&i) = candidates.iter().find(|&&i| i > pos) {
            self.history_pos = Some(i);
            let text = self.sent_history[i].clone();
            self.show_recalled(text);
        } else {
            // Past the newest match: restore the draft and end browsing.
            self.history_pos = None;
            self.history_filter = None;
            let draft = self.history_draft.take().unwrap_or_default();
            self.show_recalled(draft);
        }
        true
    }

    /// Open Ctrl+R reverse search over the sent history, saving the current draft so Esc
    /// can restore it. With an empty history there is nothing to search, so it reports
    /// that it did not open.
    pub fn open_history_search(&mut self) -> bool {
        if self.sent_history.is_empty() {
            return false;
        }
        let newest = self.sent_history.len() - 1;
        self.history_search = Some(HistorySearch {
            query: String::new(),
            index: newest,
            saved_input: self.input.clone(),
        });
        // Show the newest entry straight away, as a shell does: Ctrl+R is "the last prompt"
        // before it is anything else, and an empty query matches everything anyway.
        self.input = self.sent_history[newest].clone();
        self.cursor = self.input.chars().count();
        true
    }

    /// The history entries matching the open search, newest first. An empty query matches
    /// everything, which is what makes Ctrl+R a plain "show me my last prompts".
    pub fn history_search_matches(&self) -> Vec<usize> {
        let Some(search) = &self.history_search else {
            return Vec::new();
        };
        let needle = search.query.to_lowercase();
        self.sent_history
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, text)| needle.is_empty() || text.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Show the current match in the input. When the query matches nothing, the previously
    /// shown match is kept: an empty result must not blank the prompt the user is looking at
    /// while they finish typing the query.
    fn sync_history_search(&mut self, previous: String) {
        let matches = self.history_search_matches();
        let Some(&newest) = matches.first() else {
            // Restore whatever was displayed before this keystroke.
            self.input = previous;
            self.cursor = self.input.chars().count();
            return;
        };
        if let Some(search) = &mut self.history_search {
            search.index = newest;
            let text = self.sent_history[newest].clone();
            self.input = text;
            self.cursor = self.input.chars().count();
        }
    }

    /// Add a character to the reverse-search query.
    pub fn history_search_push(&mut self, c: char) {
        let previous = self.input.clone();
        if let Some(search) = &mut self.history_search {
            search.query.push(c);
            search.index = self.sent_history.len().saturating_sub(1);
        }
        self.sync_history_search(previous);
    }

    /// Backspace in the reverse-search query.
    pub fn history_search_backspace(&mut self) {
        let previous = self.input.clone();
        if let Some(search) = &mut self.history_search {
            search.query.pop();
            search.index = self.sent_history.len().saturating_sub(1);
        }
        self.sync_history_search(previous);
    }

    /// Ctrl+R again: step to the next older match.
    pub fn history_search_older(&mut self) -> bool {
        let matches = self.history_search_matches();
        let Some(search) = &mut self.history_search else {
            return false;
        };
        if search.index == 0 {
            return false;
        }
        // Walk to the next-older matching entry.
        let Some(pos) = matches.iter().position(|&i| i == search.index) else {
            return false;
        };
        let Some(&next) = matches.get(pos + 1) else {
            return false;
        };
        search.index = next;
        let text = self.sent_history[next].clone();
        self.input = text;
        self.cursor = self.input.chars().count();
        true
    }

    /// Enter: accept the shown match and close the search.
    pub fn history_search_accept(&mut self) {
        if self.history_search.take().is_some() {
            self.cursor = self.input.chars().count();
        }
    }

    /// Esc: close the search and put back exactly what was being typed before it opened.
    pub fn history_search_cancel(&mut self) {
        if let Some(search) = self.history_search.take() {
            self.input = search.saved_input;
            self.cursor = self.input.chars().count();
        }
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

    /// Insert the selected completion. A command word replaces the whole input
    /// ("/re" → "/redo "); an argument value replaces only the word being typed
    /// ("/config ti" → "/config tips "), or is appended to a bare command
    /// ("/autocompact" → "/autocompact 90% ").
    pub fn accept_completion(&mut self) -> bool {
        let completions = self.visible_completions();
        if completions.is_empty() {
            return false;
        }
        let idx = self.completion_selected.min(completions.len() - 1);
        let insert = completions[idx].insert.clone();
        match completions[idx].kind {
            autocomplete::Kind::Command => {
                self.input = format!("/{insert} ");
            }
            autocomplete::Kind::Argument => {
                // "Replace the word being typed" only applies when an argument word
                // actually exists. `/autocompact ` (trailing space only) has none, and
                // cutting back to the previous whitespace would eat the command word.
                let trimmed_end = self.input.trim_end();
                let has_argument_word = trimmed_end.contains(char::is_whitespace);
                if has_argument_word {
                    let trimmed_end_len = trimmed_end.len();
                    // Advance past the separator by its UTF-8 length: an ideographic space
                    // (U+3000, 3 bytes) pasted from a CJK input method would otherwise leave
                    // the cut mid-character and `truncate` panics.
                    let cut = self.input[..trimmed_end_len]
                        .char_indices()
                        .rev()
                        .find(|(_, c)| c.is_whitespace())
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    self.input.truncate(cut);
                    self.input.push_str(&insert);
                    self.input.push(' ');
                } else {
                    // The command word is complete but no argument has been typed yet:
                    // keep the command and add the value after it, normalising any
                    // trailing whitespace to a single space.
                    let indent_len = self.input.len() - self.input.trim_start().len();
                    let indent = self.input[..indent_len].to_string();
                    let command = trimmed_end.trim_start().trim_start_matches('/').trim_end();
                    self.input = format!("{indent}/{command} {insert} ");
                }
            }
        }
        self.cursor = self.input.chars().count();
        self.input_changed();
        true
    }

    /// Replace the transcript with a loaded session's blocks, keeping the welcome
    /// banner pinned at the top. A fresh chat (`/new`, empty history, fresh agent) has
    /// no session title to show, so it says what it is; a resumed session with real
    /// content shows its banner with no opener.
    pub fn set_session_blocks(&mut self, blocks: impl Iterator<Item = Block>) {
        self.blocks.clear();
        self.invalidate_rows();
        self.streaming = None;
        self.scroll_from_bottom = Some(usize::MAX); // Show the opening wordmark even in a small terminal.
        self.selection = None;
        self.selection_snapshot = None;
        self.queued.clear();
        self.queue_paused = false;
        // Prompt history is deliberately NOT cleared: it lives in the state directory and
        // is shared across sessions, the way shell history is. Clearing it here would only
        // hide it until the next launch, since the file still holds it.
        self.history_pos = None;
        self.history_draft = None;
        self.history_filter = None;
        self.status.busy = false;
        self.status.member = None;
        self.cancel_armed = false;
        self.turn_started = None;
        self.request_rate = Default::default();
        self.activity = "Ready".into();
        if self.welcome_banner {
            self.blocks.push(Block::Banner);
        }
        let mut empty = true;
        for b in blocks {
            empty = false;
            self.blocks.push(b);
        }
        if empty {
            self.push_block(Block::System("new session — ask anything".into()));
        }
    }

    pub fn rewind_conversation(
        &mut self,
        blocks: Vec<loon_core::replay::VisibleBlock>,
        prompt: String,
    ) {
        let kept: Vec<&str> = blocks
            .iter()
            .filter(|b| matches!(b, loon_core::replay::VisibleBlock::User { .. }))
            .map(|b| match b {
                loon_core::replay::VisibleBlock::User { text } => text.as_str(),
                _ => unreachable!("filtered to user rows above"),
            })
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
            self.invalidate_rows();
        } else {
            self.blocks = blocks
                .into_iter()
                .map(|b| match b {
                    loon_core::replay::VisibleBlock::User { text } => Block::User(text),
                    loon_core::replay::VisibleBlock::Assistant { text } => Block::Assistant(text),
                    loon_core::replay::VisibleBlock::Tool {
                        name,
                        detail,
                        ok,
                        excerpt,
                    } => Block::Tool {
                        name,
                        ok,
                        excerpt,
                        pending: false,
                        detail,
                        started: None,
                        elapsed: None,
                    },
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
        self.history_pos = None;
        self.history_draft = None;
        self.history_filter = None;
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
            UiNotice::QueuePaused(paused) => self.queue_paused = paused,
            UiNotice::Draft(text) => {
                // A driver-restored draft replaces the input, so any history entry
                // being browsed is no longer what Down would return to.
                self.history_pos = None;
                self.history_draft = None;
                self.history_filter = None;
                if self.input.is_empty() {
                    self.input = text;
                } else {
                    self.input = format!("{text}\n{}", self.input);
                }
                self.cursor = self.input.chars().count();
                self.input_changed();
            }

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
                            self.invalidate_rows();
                            break;
                        }
                        Block::Assistant(_)
                        | Block::Member(_)
                        | Block::System(_)
                        | Block::Tool { .. } => {
                            self.blocks.pop();
                            self.invalidate_rows();
                        }
                    }
                }
                self.streaming = None;
                self.selection = None;
                self.selection_snapshot = None;
                // The cancelled prompt comes back as editable text: cursor at the end,
                // the popup stays closed so the draft is not mistaken for a command.
                self.history_pos = None;
                self.history_draft = None;
                self.history_filter = None;
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
        self.invalidate_rows();
        if self.selection.is_none() && self.scroll_from_bottom.is_none() {
            self.scroll_from_bottom = None;
        }
    }

    /// Drop the cached transcript rows. Called by every `blocks` mutation: push, truncate,
    /// rewind, session load, stream commit. A miss is one rebuild; a stale hit is a
    /// transcript that silently shows yesterday's output, so mutation sites call this even
    /// when it looks redundant.
    fn invalidate_rows(&mut self) {
        self.blocks_generation = self.blocks_generation.wrapping_add(1);
    }

    pub fn stream_text(&mut self, delta: &str) {
        self.invalidate_rows();
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
            // Keep the whole result: the diff module already bounds its own output
            // (clipping with an explicit marker), so a second cap here would drop the
            // tail of a large edit without saying so. The transcript renders excerpts
            // line by line and the viewport scrolls, so length costs nothing.
            let first_lines: String = excerpt.lines().collect::<Vec<_>>().join(" ⏎ ");
            let (detail, elapsed) = match &self.blocks[pos] {
                Block::Tool {
                    detail, started, ..
                } => (detail.clone(), started.map(|s| s.elapsed())),
                _ => (String::new(), None),
            };
            // In-place replacement of a pending block: same length, so a length-only
            // cache key would miss nothing. Invalidate explicitly — a stale cache kept a
            // "running…" tool row on screen after its result had arrived.
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
            self.invalidate_rows();
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
    // Shared with the tools below so a background subagent's report reaches this
    // agent's turn loop, not a registry nobody reads.
    let background = loon_core::subagent::BackgroundHub::new();
    // Keep a handle for the live panel before the hub is moved into the agent and tools.
    let background_handle = background.clone();
    let agent = Arc::new(Mutex::new({
        let mut a = Agent::new_session(
            paths,
            cfg.clone(),
            registry.clone(),
            loon_core::subagent::default_tools_with_hub(
                paths,
                &cwd,
                &cfg,
                registry.clone(),
                Some(loop_registry.clone()),
                background.clone(),
            ),
            cwd.clone(),
            false,
        )
        .map_err(|e| anyhow::anyhow!("session init: {e}"))?;
        a.loops = loop_registry.clone();
        a.background = background;
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
    // The live subagent panel reads the same hub the background_* tools use.
    app.background = Some(background_handle);
    // Prompt history survives a restart: recall is far less useful if quitting erases it.
    // Loading is best-effort — a missing or unreadable file just means no history yet.
    app.history_file = Some(history::history_path(&paths.state_dir));
    app.sent_history = history::load(&paths.state_dir);
    // The in-memory list is a view of the recent tail; keep it bounded the same way.
    let keep = app.sent_history.len().saturating_sub(SENT_HISTORY_LIMIT);
    app.sent_history.drain(..keep);
    if let Ok((loaded, _)) = loon_config::Config::load_effective(paths, &cwd) {
        app.palette = loaded.appearance.palette;
        app.settings_effort = loaded.session.effort.unwrap_or("medium".into());
        app.tips = loaded.ui.tips;
        app.syntax = loaded.ui.syntax;
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
    app.settings_autocompact = agent.lock().await.autocompact;
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
                let b64 = osc52_base64(text.as_bytes());
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
                    app.set_session_blocks(blocks.into_iter().map(|b| match b {
                        loon_core::replay::VisibleBlock::User { text } => Block::User(text),
                        loon_core::replay::VisibleBlock::Assistant { text } => {
                            Block::Assistant(text)
                        }
                        loon_core::replay::VisibleBlock::Tool {
                            name,
                            detail,
                            ok,
                            excerpt,
                        } => Block::Tool {
                            name,
                            ok,
                            excerpt,
                            pending: false,
                            detail,
                            started: None,
                            elapsed: None,
                        },
                    }));
                    app.streaming = None;
                    app.selection = None;
                    app.selection_snapshot = None;
                    app.navigator = None;
                    terminal.clear()?;
                }
                AgentEvent::WorkStateChanged { state } => app.work_state = state,
                AgentEvent::TurnStarted => {
                    app.status.busy = true;
                    app.activity = "Waiting for provider".into();
                    app.begin_turn();
                    app.cancel_hint_shown = false;
                    app.cancel_armed = false;
                }
                AgentEvent::QueuedMessagesApplied { messages } => {
                    app.commit_stream();
                    for text in messages {
                        app.push_block(Block::User(text));
                    }
                    app.queued.clear();
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
                    "autocompact" => app.settings_autocompact = value == "true",
                    "tips" => app.tips = value == "true",
                    "syntax" => app.syntax = value == "true",
                    "tip_text" => app.tip_text = value,
                    "footer" => app.footer = value == "true",
                    "welcome_banner" => {
                        app.welcome_banner = value == "true";
                        if app.welcome_banner {
                            app.push_welcome_banner();
                        } else {
                            app.blocks.retain(|b| !matches!(b, Block::Banner));
                            app.invalidate_rows();
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
                    app.set_session_blocks(std::iter::empty());
                    terminal.clear()?;
                }
                AgentEvent::MemberNote(m) => {
                    if app.status.busy
                        && (m.starts_with("ultraloop")
                            || m.starts_with("verification")
                            || m.starts_with("Compacting"))
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
                AgentEvent::SubagentUpdate {
                    id,
                    label,
                    status,
                    note,
                } => app.push_block(subagent_update_block(id, &label, &status, &note)),
                AgentEvent::SubagentFinished {
                    id,
                    label,
                    status,
                    text,
                } => app.push_block(subagent_finished_block(id, &label, &status, &text)),
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
                        TurnStatus::Cancelled => {
                            app.push_block(Block::System("stopped · completed work kept".into()))
                        }
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
    if key.code == KeyCode::Esc && key.kind == crossterm::event::KeyEventKind::Repeat {
        return;
    }
    // Ctrl+R reverse search is a modal prompt-line mode: while it is open, printable keys
    // extend the query rather than the prompt, and Enter/Esc commit or abandon it. Handled
    // before everything else so a stray shortcut cannot fire mid-search.
    if app.history_search.is_some() {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            // Ctrl+R again walks to the next older match, as in a shell.
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.history_search_older();
            }
            KeyCode::Backspace => app.history_search_backspace(),
            KeyCode::Esc if plain => app.history_search_cancel(),
            KeyCode::Enter | KeyCode::Tab if plain => app.history_search_accept(),
            KeyCode::Char(c) if plain => app.history_search_push(c),
            _ => {}
        }
        return;
    }
    // Ctrl+R opens the search (the shell binding users reach for).
    if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if !app.open_history_search() {
            app.push_block(Block::System(
                "no prompt history yet — send a prompt first".into(),
            ));
        }
        return;
    }
    // A double-Esc stop is armed by one Esc and disarmed by anything else.
    if key.code != KeyCode::Esc {
        app.cancel_armed = false;
    }
    if app.pending_approval.is_none() && manager::key(app, key, cmd_tx).await {
        return;
    }
    // A pending approval is modal: y approves, n or Esc refuses, everything else is
    // swallowed so the turn cannot be poked while it waits.
    //
    // The answer must be an *unmodified* key. Matching `key.code` alone meant
    // Ctrl+Y — the copy-last-answer shortcut — approved the pending tool call, so a
    // user reaching for a copy silently authorized whatever was being asked about.
    if let Some((tool, _excerpt, answer_tx)) = app.pending_approval.take() {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('y' | 'Y') if plain => {
                let _ = answer_tx.send(true);
                app.push_block(Block::System(format!("approved: {tool}")));
            }
            KeyCode::Char('n' | 'N') if plain => {
                let _ = answer_tx.send(false);
                app.push_block(Block::System(format!("refused: {tool}")));
            }
            KeyCode::Esc if plain => {
                let _ = answer_tx.send(false);
                app.push_block(Block::System(format!("refused: {tool}")));
            }
            _ => {
                // Not an answer: keep waiting. Anything else (Ctrl+Y, Ctrl+O, typing)
                // must not resolve the prompt.
                app.pending_approval = Some((tool, _excerpt, answer_tx));
            }
        }
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
        && (app.selection.is_some() || app.input_selection().is_some())
    {
        app.pending_copy = app.selection.map(|s| selected_text(app, s)).or_else(|| {
            app.input_selection().map(|(lo, hi)| {
                let from = app.cursor_byte_offset(lo);
                let to = app.cursor_byte_offset(hi);
                app.input[from..to].to_string()
            })
        });
        app.selection = None;
        app.selection_snapshot = None;
        app.input_anchor = None;
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
            app.input_anchor = None;
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
            app.input_anchor = None;
            if text.is_empty() {
                if app.queue_paused && !app.queued.is_empty() {
                    app.queue_paused = false;
                    app.begin_turn();
                    app.status.busy = true;
                    let _ = cmd_tx.send(UiCommand::SendNow(String::new())).await;
                }
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
                let start = word_left(&app.input, app.cursor);
                let from = app.cursor_byte_offset(start);
                let to = app.cursor_byte_offset(app.cursor);
                app.input.replace_range(from..to, "");
                app.cursor = start;
            }
            app.input_changed();
        }
        KeyCode::Backspace => {
            // Backspace on a selection deletes the selection, not one more character.
            if let Some((lo, hi)) = app.input_selection() {
                let from = app.cursor_byte_offset(lo);
                let to = app.cursor_byte_offset(hi);
                app.input.replace_range(from..to, "");
                app.cursor = lo;
            } else if app.cursor > 0 {
                let char_idx = app.cursor - 1;
                let byte = app.cursor_byte_offset(char_idx);
                let byte_end = app.cursor_byte_offset(char_idx + 1);
                app.input.replace_range(byte..byte_end, "");
                app.cursor -= 1;
            }
            app.input_changed();
        }
        // Arrow keys: plain moves one char, Ctrl moves a whole word (matching
        // Ctrl+Backspace's word boundaries), Shift extends a selection. The three
        // combine: Ctrl+Shift+Left selects the word to the left.
        KeyCode::Left => {
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            let target = if key.modifiers.contains(KeyModifiers::CONTROL) {
                prev_word_start(&app.input, app.cursor)
            } else {
                app.cursor.saturating_sub(1)
            };
            app.move_cursor(target, extend);
        }
        KeyCode::Right => {
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            let target = if key.modifiers.contains(KeyModifiers::CONTROL) {
                word_right(&app.input, app.cursor)
            } else {
                app.cursor + 1
            };
            app.move_cursor(target, extend);
        }
        KeyCode::Home => {
            let (line, _) = app.cursor_line_col();
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            app.move_cursor(app.offset_at(line, 0), extend);
        }
        KeyCode::End => {
            let (line, _) = app.cursor_line_col();
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            app.move_cursor(app.offset_at(line, usize::MAX), extend);
        }
        // single-line input: Up/Down jump to the ends (popup navigation keeps
        // priority when the popup is visible). Up on an empty input pulls the most
        // recently queued prompt back for editing; otherwise Up steps through the
        // sent-prompt history shell-style, newest first. Queued recall wins when
        // both apply because its text never ran; browsing sent prompts is the
        // fallback. Shift+Up/Down stay ordinary caret motion: a selection anchor
        // cannot survive swapping the whole input for a different prompt.
        KeyCode::Up
            if app.completion_count() == 0 && app.input.is_empty() && !app.queued.is_empty() =>
        {
            // The request loop can consume the queue concurrently: the driver returns
            // only text it actually removed, preventing duplicate sends after Up.
            let _ = cmd_tx.send(UiCommand::TakeLastQueued).await;
        }
        // Shift is excluded so it falls through to the line-motion arms below:
        // swapping the whole input out from under a selection anchor is never what
        // "select the line above" means.
        KeyCode::Up
            if app.completion_count() == 0
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && app.can_recall_previous() =>
        {
            app.recall_previous();
        }
        KeyCode::Up if app.completion_count() == 0 && app.input.contains('\n') => {
            let (line, col) = app.cursor_line_col();
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            app.move_cursor(app.offset_at(line.saturating_sub(1), col), extend);
        }
        KeyCode::Down
            if app.completion_count() == 0
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && app.can_recall_next() =>
        {
            app.recall_next();
        }
        KeyCode::Down if app.completion_count() == 0 && app.input.contains('\n') => {
            let (line, col) = app.cursor_line_col();
            let last = app.input_lines().len().saturating_sub(1);
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            app.move_cursor(app.offset_at((line + 1).min(last), col), extend);
        }
        KeyCode::Up if app.completion_count() == 0 => {
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            app.move_cursor(0, extend)
        }
        KeyCode::Down if app.completion_count() == 0 => {
            let end = app.input.chars().count();
            let extend = key.modifiers.contains(KeyModifiers::SHIFT);
            app.move_cursor(end, extend)
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
        KeyCode::BackTab => {
            let _ = cmd_tx.send(UiCommand::CycleMode).await;
        }
        KeyCode::Esc if app.completion_count() > 0 && !app.status.busy => {
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
                app.popup_dismissed = true;
                app.activity = "Esc again to stop".into();
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Typing over a selection replaces it, as in any text field.
            if let Some((lo, hi)) = app.input_selection() {
                let from = app.cursor_byte_offset(lo);
                let to = app.cursor_byte_offset(hi);
                app.input.replace_range(from..to, "");
                app.cursor = lo;
            }
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
    // Remember what was actually submitted so Up can recall it later. Slash
    // commands are filtered inside `record_sent`.
    app.record_sent(&text);
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
    app.scroll_from_bottom = None;
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

/// Base64 for the OSC 52 clipboard escape. Terminals decode this strictly, so it
/// must be the real alphabet, not decimal byte values.
fn osc52_base64(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        // The table holds bytes but the payload is text: format them as chars.
        // `write!("{}")` on a `u8` emits its decimal value ("f" became "901036161"),
        // which no terminal can decode back to the copied text.
        let _ = write!(
            out,
            "{}{}{}{}",
            T[(n >> 18) as usize & 63] as char,
            T[(n >> 12) as usize & 63] as char,
            if chunk.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            },
            if chunk.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            },
        );
    }
    out
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
            // input box positions the text cursor, on any visible line
            if let Some((ix, iy)) = app.input_rect {
                let rows = app.input_rows.max(1);
                if m.row >= iy && m.row < iy + rows {
                    let line = app.input_scroll + (m.row - iy) as usize;
                    let lines = app.input_lines();
                    let text = lines.get(line)?;
                    // Column offset: the prompt glyph plus one space sit left of the
                    // text on the first line, two cells on continuation lines.
                    let lead = 2u16;
                    let cell = m.column.saturating_sub(ix + lead) as usize;
                    // The cursor line may be scrolled horizontally; other lines are
                    // drawn from their first column.
                    let (cursor_line, _) = app.cursor_line_col();
                    let view = if line == cursor_line {
                        app.input_view_start
                    } else {
                        0
                    };
                    let col = view
                        + input_cell_to_cursor(&text.chars().skip(view).collect::<String>(), cell);
                    app.input_anchor = None;
                    app.cursor = app.offset_at(line, col);
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

/// Start of the word before `cursor`, for Ctrl+Backspace.
///
/// Skips spaces backwards, then the word itself. A newline is a hard stop: deleting
/// must never merge two lines silently.
fn word_left(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut start = cursor.min(chars.len());
    while start > 0 && chars[start - 1].is_whitespace() && chars[start - 1] != '\n' {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    start
}

/// Start of the word before `cursor` for Ctrl+Left: same boundaries as
/// [`word_left`], but a line break is crossed instead of stopping, because moving the
/// caret destroys nothing. Standing at the start of a line, Ctrl+Left therefore lands
/// on the start of the previous line's last word rather than doing nothing.
fn prev_word_start(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let start = word_left(text, cursor);
    if start < cursor.min(chars.len()) || start == 0 {
        return start;
    }
    // `word_left` could not move: a newline is immediately before the caret.
    let mut at = start;
    while at > 0 && chars[at - 1] == '\n' {
        at -= 1;
    }
    word_left(text, at)
}

/// End of the word after `cursor`, for Ctrl+Right: skip the word being finished, then
/// the whitespace after it, landing at the start of the next word.
fn word_right(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut end = cursor.min(chars.len());
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    while end < chars.len() && chars[end].is_whitespace() {
        end += 1;
    }
    end
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

#[cfg(test)]
mod prompt_editing_tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn app_with(input: &str) -> App {
        let mut app = super::tests::app_for_test();
        app.input = input.to_string();
        app.cursor = input.chars().count();
        app
    }

    async fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        let (tx, _rx) = mpsc::channel(8);
        handle_key(app, key(code, mods), &tx).await;
    }

    #[tokio::test]
    async fn ctrl_arrows_jump_by_a_word() {
        let mut app = app_with("alpha beta gamma");
        press(&mut app, KeyCode::Left, KeyModifiers::CONTROL).await;
        assert_eq!(app.cursor, 11, "Ctrl+Left jumps to the start of 'gamma'");
        press(&mut app, KeyCode::Left, KeyModifiers::CONTROL).await;
        assert_eq!(app.cursor, 6, "then to the start of 'beta'");
        press(&mut app, KeyCode::Right, KeyModifiers::CONTROL).await;
        assert_eq!(app.cursor, 11, "Ctrl+Right returns to the start of 'gamma'");
        press(&mut app, KeyCode::Right, KeyModifiers::CONTROL).await;
        assert_eq!(app.cursor, 16, "and to the end of the input");
    }

    #[tokio::test]
    async fn word_motion_matches_ctrl_backspace_boundaries() {
        // Within a line, what Ctrl+Left skips is exactly what Ctrl+Backspace deletes:
        // both call `word_left`, so the two can never drift apart.
        let mut app = app_with("alpha beta");
        press(&mut app, KeyCode::Left, KeyModifiers::CONTROL).await;
        assert_eq!(app.cursor, 6);
        press(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL).await;
        assert_eq!(
            app.input, "beta",
            "deleted exactly the span the caret had skipped (word plus trailing space)"
        );
        assert_eq!(app.cursor, 0);
    }

    #[tokio::test]
    async fn ctrl_left_crosses_a_line_break_but_backspace_does_not() {
        let mut app = app_with("first\nsecond");
        press(&mut app, KeyCode::Left, KeyModifiers::CONTROL).await;
        assert_eq!(app.cursor, 6, "stops at the start of the second line");
        press(&mut app, KeyCode::Left, KeyModifiers::CONTROL).await;
        assert_eq!(
            app.cursor, 0,
            "Ctrl+Left crosses the break and lands on the previous word"
        );
        // Ctrl+Backspace at the start of a line must not merge the two lines.
        let mut app = app_with("first\nsecond");
        app.cursor = 6;
        press(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL).await;
        assert_eq!(
            app.input, "first\nsecond",
            "nothing to delete across a newline"
        );
        assert_eq!(app.cursor, 6);
    }

    #[tokio::test]
    async fn shift_arrows_select_input_text() {
        let mut app = app_with("hello world");
        press(&mut app, KeyCode::Left, KeyModifiers::SHIFT).await;
        assert_eq!(
            app.input_selection(),
            Some((10, 11)),
            "one char selected to the left"
        );
        press(&mut app, KeyCode::Left, KeyModifiers::SHIFT).await;
        assert_eq!(app.input_selection(), Some((9, 11)), "the anchor stays put");
        press(&mut app, KeyCode::Right, KeyModifiers::SHIFT).await;
        assert_eq!(app.input_selection(), Some((10, 11)), "shrinking works too");
        // A plain arrow collapses the selection instead of extending it.
        press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
        assert_eq!(app.input_selection(), None);
        assert_eq!(app.cursor, 9);
    }

    #[tokio::test]
    async fn ctrl_shift_arrows_select_a_whole_word() {
        let mut app = app_with("alpha beta");
        press(
            &mut app,
            KeyCode::Left,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        )
        .await;
        assert_eq!(app.input_selection(), Some((6, 10)), "selects 'beta'");
    }

    #[tokio::test]
    async fn typing_and_backspace_replace_the_selection() {
        // "alpha beta" with the caret at 10; two Shift+Lefts select "ta" (8..10).
        let mut app = app_with("alpha beta");
        press(&mut app, KeyCode::Left, KeyModifiers::SHIFT).await;
        press(&mut app, KeyCode::Left, KeyModifiers::SHIFT).await;
        assert_eq!(app.input_selection(), Some((8, 10)));
        press(&mut app, KeyCode::Char('X'), KeyModifiers::NONE).await;
        assert_eq!(app.input, "alpha beX", "the selection was replaced");
        assert_eq!(app.cursor, 9);

        let mut app = app_with("alpha beta");
        press(&mut app, KeyCode::Left, KeyModifiers::SHIFT).await;
        press(&mut app, KeyCode::Left, KeyModifiers::SHIFT).await;
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "alpha be",
            "backspace deleted only the selection"
        );
        assert_eq!(app.cursor, 8);
    }

    #[tokio::test]
    async fn shift_selection_can_be_copied() {
        let mut app = app_with("alpha beta");
        press(
            &mut app,
            KeyCode::Left,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        )
        .await;
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
        assert_eq!(app.pending_copy.as_deref(), Some("beta"));
        assert_eq!(app.input_selection(), None, "copy clears the highlight");
        assert_eq!(app.input, "alpha beta", "copy does not modify the input");
    }

    #[tokio::test]
    async fn shift_arrows_move_across_lines_in_a_multiline_prompt() {
        let mut app = app_with("one\ntwo");
        press(&mut app, KeyCode::Up, KeyModifiers::SHIFT).await;
        assert_eq!(
            app.cursor, 3,
            "Shift+Up moves to the same column on the line above"
        );
        assert_eq!(
            app.input_selection(),
            Some((3, 7)),
            "the selection spans the line break"
        );
    }

    #[test]
    fn clicking_any_input_line_places_the_cursor_there() {
        // Regression: click-to-cursor only worked on the first line because the hit
        // test compared against the single recorded row.
        let mut app = super::tests::app_for_test();
        app.input = "first\nsecond\nthird".into();
        app.input_rect = Some((0, 10));
        app.input_rows = 3;
        app.input_scroll = 0;
        // Click on the third visible row, two cells past the prompt and its space.
        let click = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2 + 3,
            row: 12,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, click);
        assert_eq!(
            app.cursor,
            app.offset_at(2, 3),
            "the caret lands on the third line, third column"
        );
    }

    #[test]
    fn clicking_uses_the_input_scroll_offset() {
        let mut app = super::tests::app_for_test();
        app.input = "one\ntwo\nthree\nfour".into();
        app.input_rect = Some((0, 5));
        app.input_rows = 2;
        // The box is scrolled down by two lines, so the top visible row is line 2.
        app.input_scroll = 2;
        let click = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, click);
        assert_eq!(
            app.cursor,
            app.offset_at(2, 0),
            "maps through the scroll offset"
        );
    }

    #[test]
    fn clicking_below_the_input_leaves_the_cursor_alone() {
        let mut app = super::tests::app_for_test();
        app.input = "one\ntwo".into();
        app.cursor = 2;
        app.input_rect = Some((0, 5));
        app.input_rows = 2;
        let click = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 3,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, click);
        assert_eq!(app.cursor, 2, "a click outside the box is not a caret move");
    }
}

/// `accept_completion` rewrites only the argument word being typed. The cut has to
/// land on a char boundary: a multi-byte separator (ideographic space U+3000, U+00A0,
/// U+2028) pasted from a CJK input method used to leave the cut mid-character and
/// `String::truncate` panicked, killing the session mid-edit.
/// Up/Down must recall prompts that were actually *sent*, not only the ones still
/// sitting in the queue. The reported symptom was that arrows did nothing after a
/// turn ran: the only recall path was `TakeLastQueued`, which by definition cannot
/// return text the agent already consumed. These pin the shell-style contract,
/// including the edges where history must yield to ordinary caret motion.
#[cfg(test)]
mod prompt_history_tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    async fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        let (tx, _rx) = mpsc::channel(8);
        handle_key(app, key(code, mods), &tx).await;
    }

    fn app_with_history(sent: &[&str]) -> App {
        let mut app = super::tests::app_for_test();
        for text in sent {
            app.record_sent(text);
        }
        app
    }

    /// The core reported defect: send a prompt, then Up must bring it back.
    #[tokio::test]
    async fn up_recalls_the_prompt_that_was_just_sent() {
        let mut app = app_with_history(&["fix the parser tests"]);
        assert!(app.input.is_empty());
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "fix the parser tests");
        assert_eq!(
            app.cursor,
            app.input.chars().count(),
            "the caret sits at the end of the recalled prompt, ready to edit"
        );
    }

    /// Submitting through the real Enter path is what populates the history.
    #[tokio::test]
    async fn sending_a_prompt_makes_it_recallable() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = super::tests::app_for_test();
        app.input = "summarise the diff".into();
        app.cursor = app.input.chars().count();
        handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), &tx).await;
        assert!(matches!(rx.try_recv(), Ok(UiCommand::Submit(t)) if t == "summarise the diff"));
        assert!(app.input.is_empty(), "Enter cleared the input");

        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "summarise the diff",
            "the prompt sent a moment ago comes back on Up"
        );
    }

    /// Repeated Up walks newest → oldest; Down walks back and finally restores the
    /// draft that was being typed, so browsing never destroys work.
    #[tokio::test]
    async fn up_and_down_walk_the_history_and_restore_the_draft() {
        let mut app = app_with_history(&["first", "second", "third"]);
        app.input = "half-written draft".into();
        app.cursor = app.input.chars().count();

        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "third", "Up starts at the newest prompt");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "second");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "first", "walked to the oldest entry");

        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(app.input, "second", "Down returns toward the newest");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(app.input, "third");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "half-written draft",
            "stepping past the newest entry restores the draft untouched"
        );
    }

    /// At the oldest entry Up must not wrap around or clear the input.
    #[tokio::test]
    async fn up_at_the_oldest_entry_stays_put() {
        let mut app = app_with_history(&["only one"]);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "only one");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "only one", "no wrap, no blank input");
    }

    /// Slash commands are not remembered: recalling one would reopen the completion
    /// popup, where Up navigates the popup instead of the history — the keys would
    /// appear dead again.
    #[tokio::test]
    async fn slash_commands_are_not_recalled_but_prompts_around_them_are() {
        let (tx, _rx) = mpsc::channel(8);
        let mut app = super::tests::app_for_test();
        for text in ["a real prompt", "/model p/m"] {
            app.input = text.into();
            app.cursor = text.chars().count();
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), &tx).await;
        }
        assert_eq!(
            app.sent_history,
            vec!["a real prompt".to_string()],
            "the slash command is skipped, the prompt is kept"
        );
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "a real prompt");
        assert_eq!(app.completion_count(), 0, "no popup hijacks Up here");
    }

    /// A repeated prompt is stored once, so resending after a cancel does not fill
    /// the history with copies.
    #[tokio::test]
    async fn consecutive_duplicates_are_collapsed() {
        let app = app_with_history(&["same", "same", "other", "same"]);
        assert_eq!(
            app.sent_history,
            vec!["same".to_string(), "other".to_string(), "same".to_string()],
            "only immediately repeated entries collapse"
        );
    }

    /// The history is bounded so a long session cannot grow without limit, and the
    /// oldest entry is the one dropped.
    #[test]
    fn history_keeps_only_the_most_recent_entries() {
        let mut app = super::tests::app_for_test();
        for n in 0..SENT_HISTORY_LIMIT + 5 {
            app.record_sent(&format!("prompt {n}"));
        }
        assert_eq!(app.sent_history.len(), SENT_HISTORY_LIMIT);
        assert_eq!(app.sent_history.first().unwrap(), "prompt 5");
        assert_eq!(
            app.sent_history.last().unwrap(),
            &format!("prompt {}", SENT_HISTORY_LEN_LAST)
        );
    }

    const SENT_HISTORY_LEN_LAST: usize = SENT_HISTORY_LIMIT + 4;

    /// Shift+Up must stay a selection gesture even when history could apply: the
    /// anchor cannot survive the whole input being swapped for another prompt.
    #[tokio::test]
    async fn shift_up_selects_instead_of_recalling() {
        let mut app = app_with_history(&["an old prompt"]);
        app.input = "draft".into();
        app.cursor = 5;
        press(&mut app, KeyCode::Up, KeyModifiers::SHIFT).await;
        assert_eq!(app.input, "draft", "the input was not replaced");
        assert_eq!(
            app.input_selection(),
            Some((0, 5)),
            "Shift+Up selected the draft instead"
        );
    }

    /// A multi-line draft keeps Up as line motion *below* the first line, so the
    /// caret never jumps out of what is being written mid-draft. On the first line
    /// Up recalls instead of being a dead key — and Down restores the draft intact.
    #[tokio::test]
    async fn multi_line_draft_moves_by_line_then_browses_and_restores() {
        let mut app = app_with_history(&["older", "newer"]);
        app.input = "line one\nline two".into();
        app.cursor = app.input.chars().count();
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "line one\nline two",
            "Up on the last line moved the caret, it did not replace the draft"
        );
        assert_eq!(
            app.cursor, 8,
            "caret moved to the same column (end of 'line one') one line up"
        );

        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "newer",
            "now on the first line, Up recalls instead of re-targeting line 0"
        );

        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "older", "Up continues toward the oldest entry");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(app.input, "newer", "Down steps back toward the newest");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "line one\nline two",
            "the multi-line draft comes back unchanged, line breaks included"
        );
    }

    /// Typing a prefix and pressing Up should step through the prompts that start with
    /// it, not every prompt ever sent. This is the shell behaviour people expect after
    /// typing "cargo".
    #[tokio::test]
    async fn up_after_a_prefix_walks_only_matching_entries() {
        let mut app = app_with_history(&[
            "cargo test --workspace",
            "explain the parser",
            "cargo build --release",
            "unrelated prompt",
        ]);
        app.input = "cargo".into();
        app.cursor = 5;

        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo build --release", "newest cargo entry");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo test --workspace", "older cargo entry");
        // Nothing older matches, so Up stops rather than jumping to an unrelated prompt.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "cargo test --workspace",
            "stays on the oldest match"
        );

        // Down walks back and finally restores the typed prefix as the draft.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo build --release");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo", "the original draft comes back");
    }

    /// The filter is captured when browsing starts: typing after a recall edits the
    /// recalled text without re-filtering mid-walk.
    #[tokio::test]
    async fn the_filter_is_captured_on_the_first_up() {
        // A third entry that "cargo" does not prefix, so the filter actually narrows.
        let mut app = app_with_history(&["cargo one", "cargo two", "something else"]);
        app.input = "cargo".into();
        app.cursor = 5;
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo two");
        assert_eq!(
            app.history_filter.as_deref(),
            Some("cargo"),
            "the filter is the prefix that was typed"
        );
        // Editing the recalled entry must not re-filter the walk: the filter was captured
        // when browsing started, not recomputed from the text now in the input.
        app.input = "cargo two and more".into();
        app.cursor = app.input.chars().count();
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "cargo one",
            "the walk continues on the captured filter, not the edited text"
        );
    }

    /// A prefix that matches nothing must not make Up a dead key: an ordinary sentence
    /// falls back to the whole history, because Up is still the fastest way to the last
    /// prompt.
    #[tokio::test]
    async fn a_prefix_that_matches_nothing_falls_back_to_all_history() {
        let mut app = app_with_history(&["cargo test", "cargo build"]);
        app.input = "an ordinary sentence".into();
        app.cursor = app.input.chars().count();
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(
            app.input, "cargo build",
            "Up still recalls the newest prompt rather than doing nothing"
        );
    }

    /// A prefix that matches *everything* is not a filter either: it must not narrow
    /// anything away. (The guard is by count, so this pins that it does not change the
    /// walk when the draft prefixes every entry.)
    #[tokio::test]
    async fn a_prefix_matching_everything_still_walks_the_whole_history() {
        let mut app = app_with_history(&["cargo a", "cargo b", "cargo c"]);
        app.input = "cargo".into();
        app.cursor = 5;
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo c");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo b");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "cargo a");
    }

    /// A recalled prompt is submitted like any other and then becomes the newest
    /// history entry again — the classic "Up, Enter to resend" flow.
    #[tokio::test]
    async fn a_recalled_prompt_can_be_resent() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_with_history(&["rerun the tests"]);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
        assert_eq!(app.input, "rerun the tests");
        handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE), &tx).await;
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::Submit(t)) if t == "rerun the tests"),
            "the recalled prompt is submitted verbatim"
        );
        assert_eq!(
            app.sent_history,
            vec!["rerun the tests".to_string()],
            "resending does not duplicate the entry"
        );
    }

    /// Prompt history is shared across sessions, like shell history: it is stored in the
    /// state directory, so wiping it on `/new` would only hide it until the next launch
    /// while the file still held it. A session switch must keep it recallable, and must
    /// reset the browsing position so Down does not walk into the previous session's entry.
    #[test]
    fn switching_sessions_keeps_the_history_but_resets_browsing() {
        let mut app = app_with_history(&["old session prompt"]);
        app.set_session_blocks(std::iter::empty());
        assert_eq!(
            app.sent_history,
            vec!["old session prompt".to_string()],
            "history survives the switch"
        );
        assert!(app.history_pos.is_none(), "browsing position resets");
        assert!(
            app.recall_previous(),
            "the prompt is still recallable in the new session"
        );
        assert_eq!(app.input, "old session prompt");
    }

    /// Persistence: a prompt recorded in one launch must be recallable in the next. The
    /// history lives in the state directory, so this is what makes recall survive a restart
    /// rather than only the current process.
    #[test]
    fn a_recorded_prompt_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::history::history_path(dir.path());

        // First "launch": record two prompts through the real submit path.
        let mut first = super::tests::app_for_test();
        first.history_file = Some(path.clone());
        first.sent_history = crate::history::load(dir.path());
        first.record_sent("keep this prompt");
        first.record_sent("and this one");

        // Second "launch": a fresh App loading the same state directory.
        let mut second = super::tests::app_for_test();
        second.history_file = Some(path);
        second.sent_history = crate::history::load(dir.path());

        assert_eq!(
            second.sent_history,
            vec!["keep this prompt".to_string(), "and this one".to_string()],
            "the next launch must see both prompts"
        );
        assert!(second.recall_previous());
        assert_eq!(second.input, "and this one");
    }

    /// A prompt sent a moment ago must come back after quitting and reopening, which is the
    /// user-visible promise of persistence.
    #[tokio::test]
    async fn a_prompt_sent_before_quitting_is_recallable_after_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::history::history_path(dir.path());
        let (tx, _rx) = mpsc::channel::<UiCommand>(8);

        let mut first = super::tests::app_for_test();
        first.history_file = Some(path.clone());
        first.input = "the prompt I quit after".into();
        first.cursor = first.input.chars().count();
        handle_key(&mut first, key(KeyCode::Enter, KeyModifiers::NONE), &tx).await;

        let mut second = super::tests::app_for_test();
        second.history_file = Some(path);
        second.sent_history = crate::history::load(dir.path());
        assert!(second.recall_previous(), "history must survive the restart");
        assert_eq!(second.input, "the prompt I quit after");
    }

    /// Ctrl+R is the binding people reach for; it must search the history rather than do
    /// nothing, and typing must go to the query, not the prompt.
    #[tokio::test]
    async fn ctrl_r_searches_history_and_enter_accepts() {
        let mut app = app_with_history(&[
            "cargo test --workspace",
            "explain the parser",
            "cargo build --release",
        ]);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        assert!(app.history_search.is_some(), "Ctrl+R must open the search");

        // Typing narrows to the cargo entries, newest first.
        for c in "cargo".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE).await;
        }
        assert_eq!(app.input, "cargo build --release", "newest match first");
        assert_eq!(
            app.history_search.as_ref().unwrap().query,
            "cargo",
            "the query holds what was typed"
        );

        // Enter accepts the shown prompt and closes the search.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE).await;
        assert!(app.history_search.is_none(), "Enter closes the search");
        assert_eq!(app.input, "cargo build --release");
    }

    /// Esc must restore exactly what was being typed, so opening the search by accident
    /// never loses the draft.
    #[tokio::test]
    async fn escaping_the_search_restores_the_draft() {
        let mut app = app_with_history(&["an old prompt"]);
        app.input = "my half-written draft".into();
        app.cursor = app.input.chars().count();
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE).await;
        assert!(app.history_search.is_some());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
        assert!(app.history_search.is_none());
        assert_eq!(app.input, "my half-written draft", "the draft comes back");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    /// A second Ctrl+R walks to the next older match, like a shell.
    #[tokio::test]
    async fn a_second_ctrl_r_walks_older() {
        let mut app = app_with_history(&["cargo one", "cargo two", "cargo three"]);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        for c in "cargo".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE).await;
        }
        assert_eq!(app.input, "cargo three");
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        assert_eq!(app.input, "cargo two", "the older match");
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        assert_eq!(app.input, "cargo one");
        // At the oldest match there is nowhere older to go.
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        assert_eq!(app.input, "cargo one", "stays put");
    }

    /// Backspace edits the query and re-runs the search.
    #[tokio::test]
    async fn backspace_edits_the_search_query() {
        let mut app = app_with_history(&["alpha", "beta"]);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        press(&mut app, KeyCode::Char('b'), KeyModifiers::NONE).await;
        assert_eq!(app.input, "beta");
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE).await;
        assert_eq!(app.history_search.as_ref().unwrap().query, "");
        assert_eq!(app.input, "beta", "an empty query shows the newest entry");
    }

    /// A query that matches nothing must not blank the prompt the user is looking at.
    #[tokio::test]
    async fn a_query_with_no_matches_keeps_the_current_text() {
        let mut app = app_with_history(&["alpha", "beta"]);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL).await;
        for c in "zzz".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE).await;
        }
        assert_eq!(app.input, "beta", "the last match stays visible");
        assert!(app.history_search_matches().is_empty());
    }

    /// With no history at all, Ctrl+R explains itself instead of silently doing nothing.
    #[tokio::test]
    async fn ctrl_r_without_history_explains_itself() {
        let mut app = super::tests::app_for_test();
        let (tx, _rx) = mpsc::channel(8);
        handle_key(
            &mut app,
            key(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &tx,
        )
        .await;
        assert!(app.history_search.is_none());
        assert!(
            app.blocks
                .iter()
                .any(|b| matches!(b, Block::System(s) if s.contains("no prompt history"))),
            "the user must be told why nothing happened"
        );
    }

    /// The queue still wins on an empty input: that text never ran, so editing it
    /// is more useful than recalling something already sent.
    #[tokio::test]
    async fn a_queued_prompt_takes_priority_over_sent_history() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut app = app_with_history(&["already sent"]);
        app.queued = vec!["still pending".into()];
        handle_key(&mut app, key(KeyCode::Up, KeyModifiers::NONE), &tx).await;
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::TakeLastQueued)),
            "Up on an empty input pulls the queued prompt first"
        );
        assert!(app.input.is_empty(), "the history entry was not shown");
    }
}

#[cfg(test)]
mod completion_boundary_tests {
    use crate::tests::app_for_test;

    fn completed(input: &str) -> String {
        let mut app = app_for_test();
        app.input = input.to_string();
        app.cursor = app.input.chars().count();
        app.completion_selected = 0;
        assert!(
            app.accept_completion(),
            "completion should apply for {input:?}"
        );
        app.input
    }

    #[test]
    fn a_multibyte_separator_does_not_panic() {
        // The separator itself is kept, exactly as an ASCII space is today
        // ("/config ti" -> "/config tips "); only the half-typed word is replaced.
        for sep in ['\u{3000}', '\u{00a0}', '\u{2028}', '\t', ' '] {
            let input = format!("/config{sep}ti");
            let out = completed(&input);
            assert_eq!(
                out,
                format!("/config{sep}tips "),
                "separator {sep:?} ({} bytes) must be stepped over whole, not split mid-character",
                sep.len_utf8()
            );
        }
    }

    #[test]
    fn a_multibyte_separator_keeps_earlier_arguments() {
        // Only the last word is replaced; the command and earlier args survive, and a
        // multi-byte separator between them is stepped over whole.
        let out = completed("/config theme\u{3000}tr");
        assert_eq!(out, "/config theme\u{3000}transparent ");
    }

    /// The reported defect: typing `/autocompact`, picking `90%` and pressing Tab
    /// replaced the whole input with `/90%`, so the command was lost. Tab must keep
    /// the command and append the value — with or without a typed space.
    #[test]
    fn completing_a_command_argument_never_swallows_the_command() {
        for (input, expected) in [
            ("/autocompact", "/autocompact 90% "),
            ("/autocompact ", "/autocompact 90% "),
            ("/context", "/context 90% "),
            ("/context ", "/context 90% "),
        ] {
            let mut app = app_for_test();
            app.input = input.to_string();
            app.cursor = app.input.chars().count();
            let idx = app
                .visible_completions()
                .iter()
                .position(|c| c.insert == "90%")
                .unwrap_or_else(|| panic!("90% must be offered for {input:?}"));
            app.completion_selected = idx;
            assert!(app.accept_completion());
            assert_eq!(app.input, expected, "Tab on {input:?}");
        }
    }

    /// Tab must not leave a half-typed command behind, and must not duplicate a
    /// separator when one was already typed.
    #[test]
    fn completing_a_value_keeps_exactly_one_separator() {
        for input in ["/effort\t", "/effort  "] {
            let mut app = app_for_test();
            app.input = input.to_string();
            app.cursor = app.input.chars().count();
            app.completion_selected = 0;
            assert!(app.accept_completion());
            assert_eq!(app.input, "/effort low ", "Tab on {input:?}");
        }
    }

    /// A command word still replaces the input wholesale; that is what makes
    /// `/re` → `/redo ` work.
    #[test]
    fn a_command_completion_still_replaces_the_input() {
        assert_eq!(completed("/re"), "/redo ");
        assert_eq!(completed("/und"), "/undo ");
        // Leading indent is preserved for command completion too.
        assert_eq!(completed("  /und"), "/undo ");
    }
}

#[cfg(test)]
mod osc52_tests {
    use super::osc52_base64;

    /// OSC 52 carries the selection as base64. The encoder wrote `T[i]` (a `u8`)
    /// through `write!("{}")`, which formats the *decimal* value, so a terminal
    /// decoded "a" as "89816161" — a corrupted paste. Vectors from RFC 4648 §10.
    #[test]
    fn the_osc52_payload_is_real_base64() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(
                osc52_base64(input.as_bytes()),
                expected,
                "OSC 52 payload for {input:?}"
            );
        }
    }

    /// Multi-byte text must round-trip byte-exactly: the encoder works on UTF-8
    /// bytes, so a wide glyph or an emoji must not be split across chunks wrongly.
    #[test]
    fn multibyte_text_encodes_byte_exactly() {
        for input in ["日本語", "a日b", "✓", "line1\nline2\n", "tab\there"] {
            let encoded = osc52_base64(input.as_bytes());
            let decoded = decode(&encoded);
            assert_eq!(
                decoded,
                input.as_bytes(),
                "round-trip failed for {input:?} (payload {encoded})"
            );
        }
    }

    /// A minimal strict decoder, so the test checks the alphabet rather than
    /// trusting the encoder's own table.
    fn decode(s: &str) -> Vec<u8> {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut bits = 0;
        for c in s.bytes() {
            if c == b'=' {
                continue;
            }
            let v = T
                .iter()
                .position(|&t| t == c)
                .unwrap_or_else(|| panic!("not base64: {:?} in {s}", c as char))
                as u32;
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }
}

#[cfg(test)]
mod modal_contract_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn app_pending() -> (App, tokio::sync::oneshot::Receiver<bool>) {
        let mut app = App::new(
            true,
            StatusLine {
                model: "mock".into(),
                ..Default::default()
            },
        );
        let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
        app.pending_approval = Some(("bash".into(), "rm -rf /".into(), answer_tx));
        (app, answer_rx)
    }

    /// `pending_approval` is documented as modal: keys other than y/n/Esc are
    /// ignored while it is set. The copy and tool-detail shortcuts were handled
    /// *before* the modal check, so Ctrl+Y silently copied transcript text and
    /// Ctrl+O toggled the view while the user was being asked to authorize a
    /// command — and the prompt stayed up, so nothing on screen explained it.
    #[tokio::test]
    async fn shortcuts_do_not_fire_while_an_approval_is_pending() {
        let (tx, _rx) = mpsc::channel(8);
        let (mut app, mut answer_rx) = app_pending();
        app.push_block(Block::Assistant("transcript answer".into()));
        app.show_tool_details = false;

        handle_key(
            &mut app,
            key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            &tx,
        )
        .await;
        assert_eq!(
            app.pending_copy, None,
            "Ctrl+Y must not copy while an approval is pending"
        );

        handle_key(
            &mut app,
            key(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &tx,
        )
        .await;
        assert!(
            !app.show_tool_details,
            "Ctrl+O must not toggle tool details while an approval is pending"
        );

        assert!(
            app.pending_approval.is_some(),
            "the prompt must still be waiting for y/n/Esc"
        );
        assert!(
            answer_rx.try_recv().is_err(),
            "no answer may be sent by an unrelated key"
        );
    }

    /// Ctrl+Y is the "copy last answer" shortcut. The modal matched `key.code`
    /// alone, so Ctrl+Y resolved the prompt as *approval*: a user reaching for a
    /// copy silently authorized the pending tool call. Only an unmodified key may
    /// answer.
    #[tokio::test]
    async fn a_modified_key_never_answers_the_prompt() {
        let (tx, _rx) = mpsc::channel(8);
        for (code, mods) in [
            (KeyCode::Char('y'), KeyModifiers::CONTROL),
            (KeyCode::Char('n'), KeyModifiers::CONTROL),
            (KeyCode::Char('y'), KeyModifiers::ALT),
            (KeyCode::Esc, KeyModifiers::CONTROL),
        ] {
            let (mut app, mut rx) = app_pending();
            handle_key(&mut app, key(code, mods), &tx).await;
            assert!(
                rx.try_recv().is_err(),
                "{code:?}+{mods:?} must not answer the prompt"
            );
            assert!(
                app.pending_approval.is_some(),
                "{code:?}+{mods:?} must leave the prompt waiting"
            );
        }
    }

    /// The modal still has to work: y approves, n refuses, Esc refuses.
    #[tokio::test]
    async fn y_and_n_still_answer_the_prompt() {
        let (tx, _rx) = mpsc::channel(8);

        let (mut app, mut rx) = app_pending();
        handle_key(&mut app, key(KeyCode::Char('y'), KeyModifiers::NONE), &tx).await;
        assert_eq!(rx.try_recv(), Ok(true), "y approves");
        assert!(app.pending_approval.is_none(), "the prompt is cleared");

        let (mut app, mut rx) = app_pending();
        handle_key(&mut app, key(KeyCode::Char('n'), KeyModifiers::NONE), &tx).await;
        assert_eq!(rx.try_recv(), Ok(false), "n refuses");

        let (mut app, mut rx) = app_pending();
        handle_key(&mut app, key(KeyCode::Esc, KeyModifiers::NONE), &tx).await;
        assert_eq!(rx.try_recv(), Ok(false), "Esc refuses");
    }
}

#[cfg(test)]
mod stop_regression_tests {
    use super::*;
    #[tokio::test]
    async fn double_escape_stops_even_with_completions_and_repeat_does_not_count() {
        let mut app = App::new(
            true,
            StatusLine {
                busy: true,
                ..Default::default()
            },
        );
        app.input = "/mo".into();
        app.cursor = 3;
        app.input_changed();
        assert!(app.completion_count() > 0);
        let (tx, mut rx) = mpsc::channel(8);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &tx,
        )
        .await;
        assert!(app.cancel_armed);
        assert!(rx.try_recv().is_err());
        let repeat = KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Repeat,
        );
        handle_key(&mut app, repeat, &tx).await;
        assert!(rx.try_recv().is_err());
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &tx,
        )
        .await;
        assert!(matches!(rx.try_recv(), Ok(UiCommand::Cancel)));
        assert_eq!(
            app.input, "/mo",
            "stop must preserve the user's current draft"
        );
    }
    #[tokio::test]
    async fn enter_resumes_a_paused_queue_without_sending_empty_user_text() {
        let mut app = App::new(true, StatusLine::default());
        app.queue_paused = true;
        app.queued = vec!["pending".into()];
        let (tx, mut rx) = mpsc::channel(8);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        )
        .await;
        assert!(matches!(rx.try_recv(),Ok(UiCommand::SendNow(t)) if t.is_empty()));
        assert!(!app.queue_paused);
    }
}
