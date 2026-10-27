//! Configurable keybindings.
//!
//! One named [`Action`] per application command, one [`Chord`] per key combination, and a
//! [`KeyMap`] that resolves a chord to an action in a [`Scope`]. This module is deliberately
//! free of UI dependencies: the terminal layer converts its own key events to [`Chord`]s and
//! the configuration layer persists the same text form, so there is exactly one parser and
//! one set of defaults.
//!
//! Scope exists because the same chord legitimately means different things in different
//! places: `esc` dismisses a popup, `up` moves a selection or a caret. Only *commands* are
//! bindable here — character input, caret motion with shift-selection and terminal control
//! sequences stay fixed in the editor, because remapping them would make typing behave
//! differently per user without any visible benefit.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A physical key, independent of the terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

impl Code {
    fn parse(name: &str) -> Option<Code> {
        let lower = name.to_ascii_lowercase();
        Some(match lower.as_str() {
            "enter" | "return" | "cr" => Code::Enter,
            "esc" | "escape" => Code::Esc,
            "tab" => Code::Tab,
            "space" => Code::Char(' '),
            "backspace" | "bs" => Code::Backspace,
            "delete" | "del" => Code::Delete,
            "insert" | "ins" => Code::Insert,
            "left" => Code::Left,
            "right" => Code::Right,
            "up" => Code::Up,
            "down" => Code::Down,
            "home" => Code::Home,
            "end" => Code::End,
            "pageup" | "pgup" => Code::PageUp,
            "pagedown" | "pgdn" => Code::PageDown,
            other => {
                if let Some(n) = other.strip_prefix('f') {
                    if let Ok(n) = n.parse::<u8>() {
                        if (1..=24).contains(&n) {
                            return Some(Code::F(n));
                        }
                    }
                }
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Code::Char(c),
                    _ => return None,
                }
            }
        })
    }

    fn name(self) -> String {
        match self {
            Code::Char(' ') => "space".into(),
            Code::Char(c) => c.to_string(),
            Code::Enter => "enter".into(),
            Code::Esc => "esc".into(),
            Code::Tab => "tab".into(),
            Code::Backspace => "backspace".into(),
            Code::Delete => "delete".into(),
            Code::Insert => "insert".into(),
            Code::Left => "left".into(),
            Code::Right => "right".into(),
            Code::Up => "up".into(),
            Code::Down => "down".into(),
            Code::Home => "home".into(),
            Code::End => "end".into(),
            Code::PageUp => "pageup".into(),
            Code::PageDown => "pagedown".into(),
            Code::F(n) => format!("f{n}"),
        }
    }
}

/// Modifier state. Char chords are normalised to lowercase, so `shift` alone is only
/// meaningful for named keys (Shift+Enter) — a shifted letter arrives as its capital and is
/// normalised away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };
    pub const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
    };
    pub const ALT: Mods = Mods {
        ctrl: false,
        alt: true,
        shift: false,
    };
    pub const SHIFT: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: true,
    };
}

/// One key press: a key plus modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    pub code: Code,
    pub mods: Mods,
}

impl Chord {
    pub const fn new(code: Code, mods: Mods) -> Chord {
        Chord { code, mods }
    }
    pub const fn plain(code: Code) -> Chord {
        Chord {
            code,
            mods: Mods::NONE,
        }
    }
    pub const fn ctrl(c: char) -> Chord {
        Chord {
            code: Code::Char(c),
            mods: Mods::CTRL,
        }
    }
    pub const fn alt(c: char) -> Chord {
        Chord {
            code: Code::Char(c),
            mods: Mods::ALT,
        }
    }

    /// Parse the canonical form: `ctrl+shift+left`, `f1`, `ctrl+p`, `space`.
    pub fn parse(text: &str) -> Result<Chord, KeymapError> {
        let raw = text.trim();
        if raw.is_empty() {
            return Err(KeymapError::Empty);
        }
        if raw == "+" {
            return Ok(Chord::plain(Code::Char('+')));
        }
        // Split on '+' but keep the final segment as the key: "ctrl++" means Ctrl and '+'.
        let mut segments: Vec<&str> = raw.split('+').collect();
        if segments.len() > 1 && segments.last().is_some_and(|s| s.is_empty()) {
            segments.pop();
            segments.push("+");
        }
        let key = segments
            .pop()
            .ok_or_else(|| KeymapError::UnknownKey(raw.to_string()))?;
        let mut mods = Mods::NONE;
        for segment in segments {
            match segment.to_ascii_lowercase().as_str() {
                "" => {}
                "ctrl" | "c" | "control" => mods.ctrl = true,
                "alt" | "a" | "meta" | "m" | "option" => mods.alt = true,
                "shift" | "s" => mods.shift = true,
                other => return Err(KeymapError::UnknownModifier(format!("{other} in `{raw}`"))),
            }
        }
        let mut code = Code::parse(key).ok_or_else(|| KeymapError::UnknownKey(raw.to_string()))?;
        if let Code::Char(c) = code {
            // Canonical lowercase, and a shifted letter carries no extra shift flag: the
            // terminal reports "P" for Shift+p, which must still match `ctrl+p`.
            code = Code::Char(c.to_ascii_lowercase());
            if code != Code::Char(c) {
                mods.shift = false;
            }
        }
        Ok(Chord { code, mods })
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.ctrl {
            write!(f, "ctrl+")?;
        }
        if self.mods.alt {
            write!(f, "alt+")?;
        }
        if self.mods.shift {
            write!(f, "shift+")?;
        }
        write!(f, "{}", self.code.name())
    }
}

/// Where a binding applies. Two bindings may share a chord only when their scopes cannot
/// both be active, which for this application means they must be different scopes with
/// `Global` excluded from either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Available from the transcript, the prompt and the popup.
    Global,
    /// Only while the prompt editor has focus.
    Prompt,
    /// Only while a management list (navigator) is open; the navigator owns its keys.
    Manager,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Prompt => "prompt",
            Scope::Manager => "manager",
        }
    }
    /// Two bindings can only collide when they live in the *same* scope. A `Global` chord
    /// shadowed by a `Prompt`/`Manager` binding is not a collision: resolution prefers the
    /// specific scope deterministically, which is how `esc` stops a turn in the prompt and
    /// dismisses a dialog in a list.
    fn same_scope(self, other: Scope) -> bool {
        self == other
    }
}

/// Every bindable application command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    // ── prompt ──────────────────────────────────────────────────────────────
    SubmitPrompt,
    NewLine,
    InterruptSend,
    DeleteWord,
    WordLeft,
    WordRight,
    HistorySearch,
    HistoryPrevious,
    HistoryNext,
    // ── turn control ────────────────────────────────────────────────────────
    StopTurn,
    CancelAndQuit,
    /// Quit immediately from an idle prompt. Distinct from `CancelAndQuit`, which needs two
    /// presses while busy so a live turn is never dropped by accident.
    Quit,
    // ── transcript and surfaces ─────────────────────────────────────────────
    OpenPalette,
    Help,
    ExpandToolOutput,
    CopyLastResponse,
    ToggleDetails,
    ScrollUp,
    ScrollDown,
    // ── navigation surfaces ─────────────────────────────────────────────────
    OpenSessions,
    OpenThemes,
    OpenModels,
    OpenSettings,
    OpenKeybindings,
    CyclePermissionMode,
    Dismiss,
    Confirm,
    NavigatePrevious,
    NavigateNext,
}

impl Action {
    /// Every action, in the order the `/keybindings` list shows them.
    pub const ALL: [Action; 29] = [
        Action::SubmitPrompt,
        Action::NewLine,
        Action::InterruptSend,
        Action::StopTurn,
        Action::DeleteWord,
        Action::WordLeft,
        Action::WordRight,
        Action::HistorySearch,
        Action::HistoryPrevious,
        Action::HistoryNext,
        Action::OpenPalette,
        Action::Help,
        Action::ExpandToolOutput,
        Action::CopyLastResponse,
        Action::ToggleDetails,
        Action::ScrollUp,
        Action::ScrollDown,
        Action::OpenSessions,
        Action::OpenThemes,
        Action::OpenModels,
        Action::OpenSettings,
        Action::OpenKeybindings,
        Action::CyclePermissionMode,
        Action::CancelAndQuit,
        Action::Quit,
        Action::Dismiss,
        Action::Confirm,
        Action::NavigatePrevious,
        Action::NavigateNext,
    ];

    /// Stable identifier used in configuration and on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Action::SubmitPrompt => "submit_prompt",
            Action::NewLine => "new_line",
            Action::InterruptSend => "interrupt_send",
            Action::StopTurn => "stop_turn",
            Action::DeleteWord => "delete_word",
            Action::WordLeft => "word_left",
            Action::WordRight => "word_right",
            Action::HistorySearch => "history_search",
            Action::HistoryPrevious => "history_previous",
            Action::HistoryNext => "history_next",
            Action::OpenPalette => "open_palette",
            Action::Help => "help",
            Action::ExpandToolOutput => "expand_tool_output",
            Action::CopyLastResponse => "copy_last_response",
            Action::ToggleDetails => "toggle_details",
            Action::ScrollUp => "scroll_up",
            Action::ScrollDown => "scroll_down",
            Action::OpenSessions => "open_sessions",
            Action::OpenThemes => "open_themes",
            Action::OpenModels => "open_models",
            Action::OpenSettings => "open_settings",
            Action::OpenKeybindings => "open_keybindings",
            Action::CyclePermissionMode => "cycle_permission_mode",
            Action::CancelAndQuit => "cancel_and_quit",
            Action::Quit => "quit",
            Action::Dismiss => "dismiss",
            Action::Confirm => "confirm",
            Action::NavigatePrevious => "navigate_previous",
            Action::NavigateNext => "navigate_next",
        }
    }

    pub fn parse(id: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.id() == id)
    }

    pub fn description(self) -> &'static str {
        match self {
            Action::SubmitPrompt => "Send the prompt (or run the slash command)",
            Action::NewLine => "Insert a newline in the prompt instead of sending",
            Action::InterruptSend => "Stop the running turn and send this prompt now",
            Action::StopTurn => "Stop the running turn (double Escape preserves completed work)",
            Action::DeleteWord => "Delete the previous word",
            Action::WordLeft => "Move the caret one word left",
            Action::WordRight => "Move the caret one word right",
            Action::HistorySearch => "Search prompt history (reverse incremental)",
            Action::HistoryPrevious => "Recal the previous sent prompt",
            Action::HistoryNext => "Walk forward through prompt history",
            Action::OpenPalette => "Open the command palette",
            Action::Help => "Show grouped help",
            Action::ExpandToolOutput => "Expand or collapse grouped tool output",
            Action::CopyLastResponse => "Copy the last assistant response",
            Action::ToggleDetails => "Toggle the background task and work details view",
            Action::ScrollUp => "Scroll the transcript up",
            Action::ScrollDown => "Scroll the transcript down",
            Action::OpenSessions => "Open the session switcher",
            Action::OpenThemes => "Open the palette picker",
            Action::OpenModels => "Open the model browser",
            Action::OpenSettings => "Open settings",
            Action::OpenKeybindings => "Open the keybinding editor",
            Action::CyclePermissionMode => "Cycle the permission mode",
            Action::CancelAndQuit => "Cancel the running turn, or quit when idle",
            Action::Quit => "Quit immediately",
            Action::Dismiss => "Dismiss the open popup or dialog",
            Action::Confirm => "Confirm the selected item",
            Action::NavigatePrevious => "Move the selection up",
            Action::NavigateNext => "Move the selection down",
        }
    }

    pub fn scope(self) -> Scope {
        match self {
            Action::SubmitPrompt
            | Action::NewLine
            | Action::InterruptSend
            | Action::DeleteWord
            | Action::WordLeft
            | Action::WordRight
            | Action::HistorySearch
            | Action::HistoryPrevious
            | Action::HistoryNext
            // Escape stops a turn from the prompt; in a list the same chord dismisses the
            // dialog, which is why these two live in different scopes.
            | Action::StopTurn => Scope::Prompt,
            Action::Dismiss | Action::Confirm | Action::NavigatePrevious | Action::NavigateNext => {
                Scope::Manager
            }
            _ => Scope::Global,
        }
    }

    /// The documented default binding. `None` means "no default chord" — the action is
    /// reachable only after the user assigns one (or through its slash command).
    pub fn default_chord(self) -> Option<Chord> {
        Some(match self {
            Action::SubmitPrompt => Chord::plain(Code::Enter),
            Action::NewLine => Chord::new(Code::Enter, Mods::SHIFT),
            Action::InterruptSend => Chord::new(Code::Enter, Mods::CTRL),
            // Double Escape is handled by the dispatcher as an armed state, so the chord
            // records the single key and the arming policy lives next to the turn loop.
            Action::StopTurn => Chord::plain(Code::Esc),
            Action::DeleteWord => Chord::new(Code::Backspace, Mods::CTRL),
            Action::WordLeft => Chord::new(Code::Left, Mods::CTRL),
            Action::WordRight => Chord::new(Code::Right, Mods::CTRL),
            Action::HistorySearch => Chord::ctrl('r'),
            Action::HistoryPrevious => Chord::plain(Code::Up),
            Action::HistoryNext => Chord::plain(Code::Down),
            Action::OpenPalette => Chord::ctrl('p'),
            Action::Help => Chord::plain(Code::F(1)),
            Action::ExpandToolOutput => Chord::ctrl('o'),
            Action::CopyLastResponse => Chord::ctrl('y'),
            Action::ToggleDetails => Chord::ctrl('t'),
            Action::ScrollUp => Chord::plain(Code::PageUp),
            Action::ScrollDown => Chord::plain(Code::PageDown),
            Action::OpenSessions => Chord::ctrl('g'),
            Action::OpenThemes => Chord::alt('t'),
            Action::OpenModels => Chord::alt('m'),
            Action::OpenSettings => Chord::alt('s'),
            Action::OpenKeybindings => Chord::plain(Code::F(2)),
            Action::CyclePermissionMode => Chord::new(Code::Tab, Mods::SHIFT),
            Action::CancelAndQuit => Chord::ctrl('c'),
            Action::Quit => Chord::ctrl('q'),
            Action::Dismiss => Chord::plain(Code::Esc),
            Action::Confirm => Chord::plain(Code::Enter),
            Action::NavigatePrevious => Chord::plain(Code::Up),
            Action::NavigateNext => Chord::plain(Code::Down),
        })
    }
}

/// A named starting point. Profiles change only the keys whose *familiar* meaning differs
/// between interaction styles; everything else keeps the default, so a profile is a small
/// delta rather than a second keymap to maintain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Default,
    /// Shell/readline habits: Ctrl+P/Ctrl+N walk history, Enter still sends.
    Emacs,
    /// Ctrl+J/Ctrl+K walk history; Escape is the only modal dismissal.
    Vim,
}

impl Profile {
    pub const NAMES: [&'static str; 3] = ["default", "emacs", "vim"];

    pub fn parse(s: &str) -> Option<Profile> {
        Some(match s {
            "default" => Profile::Default,
            "emacs" => Profile::Emacs,
            "vim" => Profile::Vim,
            _ => return None,
        })
    }

    pub fn name_str(self) -> &'static str {
        match self {
            Profile::Default => "default",
            Profile::Emacs => "emacs",
            Profile::Vim => "vim",
        }
    }

    pub fn name(self) -> &'static str {
        self.name_str()
    }

    /// Bindings this profile changes relative to [`Profile::Default`].
    fn overrides(self) -> &'static [(Action, Chord)] {
        const NONE: &[(Action, Chord)] = &[];
        const EMACS: &[(Action, Chord)] = &[
            (Action::HistoryPrevious, Chord::ctrl('p')),
            (Action::HistoryNext, Chord::ctrl('n')),
            // The palette cannot keep Ctrl+P in a readline profile, so it moves to Alt+P
            // rather than silently shadowing history.
            (Action::OpenPalette, Chord::alt('p')),
        ];
        const VIM: &[(Action, Chord)] = &[
            (Action::HistoryPrevious, Chord::ctrl('k')),
            (Action::HistoryNext, Chord::ctrl('j')),
            (Action::WordLeft, Chord::alt('b')),
            (Action::WordRight, Chord::alt('w')),
        ];
        match self {
            Profile::Default => NONE,
            Profile::Emacs => EMACS,
            Profile::Vim => VIM,
        }
    }
}

// ── configuration ─────────────────────────────────────────────────────────────

/// The `[keys]` configuration table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keys {
    #[serde(default)]
    pub profile: Profile,
    /// `action_id = "chord"`. An empty string unbinds the action.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            profile: Profile::Default,
            bindings: BTreeMap::new(),
        }
    }
}

/// A chord that several actions claim in overlapping scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub chord: Chord,
    pub actions: Vec<Action>,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.actions.iter().map(|a| a.id()).collect();
        write!(f, "`{}` is bound to {}", self.chord, names.join(" and "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    Empty,
    UnknownKey(String),
    UnknownModifier(String),
    Protected(Chord),
    /// A protected chord with the reason it is reserved, so the UI can explain the refusal
    /// instead of only saying "no".
    Reserved(Chord, &'static str),
    Conflict(Conflict),
    UnknownAction(String),
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::Empty => write!(f, "a binding needs a key"),
            KeymapError::UnknownKey(k) => write!(f, "unknown key `{k}`"),
            KeymapError::UnknownModifier(m) => write!(f, "unknown modifier {m}"),
            KeymapError::Protected(c) => {
                write!(f, "`{c}` is reserved by the terminal and cannot be bound")
            }
            KeymapError::Reserved(c, why) => {
                write!(f, "`{c}` is reserved by the terminal: it {why}")
            }
            KeymapError::Conflict(c) => write!(f, "{c}"),
            KeymapError::UnknownAction(a) => write!(f, "unknown action `{a}`"),
        }
    }
}

impl std::error::Error for KeymapError {}

/// Chords the application does not take: the terminal or the shell needs them, and
/// accepting one would leave the user unable to interrupt, suspend or paste.
pub const PROTECTED: [(Chord, &str); 3] = [
    (
        Chord {
            code: Code::Char('z'),
            mods: Mods::CTRL,
        },
        "suspends the process",
    ),
    (
        Chord {
            code: Code::Char('\\'),
            mods: Mods::CTRL,
        },
        "sends SIGQUIT",
    ),
    (
        Chord {
            code: Code::Char('s'),
            mods: Mods::NONE,
        },
        "flow control in many terminals",
    ),
];

pub fn is_protected(chord: &Chord) -> Option<&'static str> {
    PROTECTED
        .iter()
        .find(|(c, _)| c == chord)
        .map(|(_, why)| *why)
}

/// One resolved binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub action: Action,
    pub chord: Chord,
    /// True when the user set this binding, false when it comes from the profile/defaults.
    pub custom: bool,
}

/// A resolved keymap: an ordered binding list plus the profile it was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    profile: Profile,
    bindings: Vec<Binding>,
}

impl Default for KeyMap {
    fn default() -> Self {
        KeyMap::new(Profile::Default, &BTreeMap::new())
    }
}

impl KeyMap {
    /// Build the effective map: defaults → profile overrides → user bindings. User bindings
    /// win, and an empty chord removes the action instead of binding "nothing".
    pub fn new(profile: Profile, user: &BTreeMap<String, String>) -> KeyMap {
        let mut map = KeyMap {
            profile,
            bindings: Vec::new(),
        };
        for action in Action::ALL {
            if let Some(chord) = action.default_chord() {
                map.put(action, chord, false);
            }
        }
        for (action, chord) in profile.overrides() {
            map.put(*action, *chord, false);
        }
        for (id, text) in user {
            let Some(action) = Action::parse(id) else {
                // Unknown action ids are reported by config validation; ignoring them here
                // keeps a typo from breaking every other binding.
                continue;
            };
            match Chord::parse(text) {
                Ok(chord) => map.put(action, chord, true),
                Err(_) if text.trim().is_empty() => map.unbind_in_place(action),
                Err(_) => {}
            }
        }
        map.sort();
        map
    }

    fn put(&mut self, action: Action, chord: Chord, custom: bool) {
        match self.bindings.iter_mut().find(|b| b.action == action) {
            Some(existing) => {
                existing.chord = chord;
                existing.custom |= custom;
            }
            None => self.bindings.push(Binding {
                action,
                chord,
                custom,
            }),
        }
    }

    fn unbind_in_place(&mut self, action: Action) {
        self.bindings.retain(|b| b.action != action);
    }

    fn sort(&mut self) {
        self.bindings.sort_by_key(|b| (b.action.scope(), b.action));
    }

    /// Build from the `[keys]` configuration table.
    pub fn from_config(keys: &Keys) -> KeyMap {
        KeyMap::new(keys.profile, &keys.bindings)
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn profile_name(&self) -> &'static str {
        self.profile.name()
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    pub fn chord_for(&self, action: Action) -> Option<Chord> {
        self.bindings
            .iter()
            .find(|b| b.action == action)
            .map(|b| b.chord)
    }

    /// Resolve a chord in a scope. `Global` and the requested scope both apply; when two
    /// actions claim the same chord the more specific scope wins, and a true tie is
    /// reported by [`KeyMap::conflicts`] instead of resolving by luck.
    pub fn resolve(&self, chord: Chord, scope: Scope) -> Option<Action> {
        let mut best: Option<(u8, Action)> = None;
        for binding in &self.bindings {
            if binding.chord != chord {
                continue;
            }
            let rank = match (binding.action.scope(), scope) {
                (Scope::Global, _) => 1,
                (s, scope) if s == scope => 2,
                _ => continue,
            };
            match best {
                Some((r, _)) if r >= rank => {}
                _ => best = Some((rank, binding.action)),
            }
        }
        best.map(|(_, a)| a)
    }

    /// Every chord claimed by more than one action *in the same scope*: a genuinely
    /// ambiguous keymap, where which action fires would depend on binding order.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut out: Vec<Conflict> = Vec::new();
        for (i, a) in self.bindings.iter().enumerate() {
            for b in self.bindings.iter().skip(i + 1) {
                if a.chord != b.chord || !a.action.scope().same_scope(b.action.scope()) {
                    continue;
                }
                match out.iter_mut().find(|c| c.chord == a.chord) {
                    Some(conflict) => {
                        if !conflict.actions.contains(&b.action) {
                            conflict.actions.push(b.action);
                        }
                    }
                    None => out.push(Conflict {
                        chord: a.chord,
                        actions: vec![a.action, b.action],
                    }),
                }
            }
        }
        out
    }

    /// Rebind one action after checking that the chord is usable. Refuses a protected
    /// chord and a chord another action already owns; the caller can retry with
    /// [`KeyMap::steal`] when the user explicitly accepted taking it over.
    pub fn rebind(&self, action: Action, chord: Chord) -> Result<KeyMap, KeymapError> {
        if let Some(why) = is_protected(&chord) {
            return Err(KeymapError::Reserved(chord, why));
        }
        let owner = self.bindings.iter().find(|b| {
            b.action != action && b.chord == chord && b.action.scope().same_scope(action.scope())
        });
        if let Some(owner) = owner {
            return Err(KeymapError::Conflict(Conflict {
                chord,
                actions: vec![owner.action, action],
            }));
        }
        let mut next = self.clone();
        next.upsert(action, chord);
        Ok(next)
    }

    /// Rebind and unbind whoever else claimed the chord.
    pub fn steal(&self, action: Action, chord: Chord) -> Result<KeyMap, KeymapError> {
        if let Some(why) = is_protected(&chord) {
            return Err(KeymapError::Reserved(chord, why));
        }
        let mut next = self.clone();
        next.bindings.retain(|b| {
            !(b.action != action && b.chord == chord && b.action.scope().same_scope(action.scope()))
        });
        next.upsert(action, chord);
        Ok(next)
    }

    fn upsert(&mut self, action: Action, chord: Chord) {
        self.put(action, chord, true);
        self.sort();
    }

    /// Remove one binding (the action becomes reachable only through its command).
    pub fn unbind(&self, action: Action) -> KeyMap {
        let mut next = self.clone();
        next.unbind_in_place(action);
        next
    }

    /// Drop every user binding, returning to the profile defaults for one action.
    pub fn reset(&self, action: Action) -> KeyMap {
        let mut next = self.clone();
        next.unbind_in_place(action);
        if let Some(chord) = action.default_chord() {
            next.put(action, chord, false);
        } else {
            for (overridden, chord) in self.profile.overrides() {
                if *overridden == action {
                    next.put(action, *chord, false);
                }
            }
        }
        next.sort();
        next
    }

    /// Reset every action to the profile defaults.
    pub fn reset_all(&self) -> KeyMap {
        KeyMap::new(self.profile, &BTreeMap::new())
    }

    /// The user-level overrides needed to reproduce `other` from its profile defaults —
    /// what `export` writes and what a save persists.
    pub fn overrides_of(&self, other: &KeyMap) -> BTreeMap<String, String> {
        let defaults = KeyMap::new(other.profile, &BTreeMap::new());
        let mut out = BTreeMap::new();
        for action in Action::ALL {
            let want = other.chord_for(action);
            let have = self.chord_for(action);
            if want == have {
                continue;
            }
            if let Some(chord) = want {
                out.insert(action.id().to_string(), chord.to_string());
            }
        }
        // An action bound in our map but absent from `other` must record the unbind.
        for action in Action::ALL {
            if self.chord_for(action).is_some() && other.chord_for(action).is_none() {
                out.insert(action.id().to_string(), String::new());
            }
        }
        // Drop entries that merely restate the profile default: keep the file small and the
        // diff meaningful.
        out.retain(
            |id, chord| match Action::parse(id).and_then(|a| defaults.chord_for(a)) {
                Some(default) => Chord::parse(chord).map(|c| c != default).unwrap_or(true),
                None => true,
            },
        );
        out
    }

    /// One-line rendering used by `/keybindings` and exported maps.
    pub fn export(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# profile = {}\n", self.profile_name()));
        for binding in &self.bindings {
            out.push_str(&format!("{} = {}\n", binding.action.id(), binding.chord));
        }
        out
    }

    /// Rows for the keybinding editor: action id, chord (or "unbound"), description, and
    /// whether it is a user override.
    pub fn rows(&self) -> Vec<(String, String, String, bool)> {
        Action::ALL
            .into_iter()
            .map(|action| {
                let chord = self
                    .chord_for(action)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unbound".into());
                let custom = self.bindings.iter().any(|b| b.action == action && b.custom);
                (
                    action.id().to_string(),
                    chord,
                    action.description().to_string(),
                    custom,
                )
            })
            .collect()
    }
}

/// Validate the `[keys]` table: unknown action ids, unparsable chords, protected chords and
/// conflicts the effective map would contain. Returns human-readable findings.
pub fn validate(keys: &Keys) -> Vec<String> {
    let mut out = Vec::new();
    for (id, text) in &keys.bindings {
        if Action::parse(id).is_none() {
            out.push(format!(
                "keys.bindings: unknown action `{id}` (see /keybindings for the list)"
            ));
            continue;
        }
        if text.trim().is_empty() {
            continue;
        }
        match Chord::parse(text) {
            Ok(chord) => {
                if let Some(why) = is_protected(&chord) {
                    out.push(format!("keys.bindings.{id}: `{chord}` {why}"));
                }
            }
            Err(e) => out.push(format!("keys.bindings.{id}: {e}")),
        }
    }
    for conflict in KeyMap::from_config(keys).conflicts() {
        out.push(format!("keys: {conflict}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> KeyMap {
        KeyMap::default()
    }

    #[test]
    fn chords_round_trip_through_their_text_form() {
        for text in [
            "ctrl+p",
            "alt+left",
            "shift+enter",
            "f1",
            "ctrl+shift+left",
            "space",
            "ctrl++",
            "esc",
            "tab",
        ] {
            let chord = Chord::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(chord.to_string(), text, "canonical form drifted");
        }
    }

    #[test]
    fn a_shifted_letter_normalises_to_its_lowercase_chord() {
        // The terminal reports Shift+p as Char('P') plus SHIFT; it must still match ctrl+p
        // would not, but plain "P" and "p" are the same chord.
        assert_eq!(
            Chord::parse("P").unwrap(),
            Chord::parse("p").unwrap(),
            "letter case must not create a second chord"
        );
        assert_ne!(
            Chord::parse("ctrl+p").unwrap(),
            Chord::parse("ctrl+shift+p").unwrap(),
            "an explicit shift modifier must remain distinct"
        );
    }

    #[test]
    fn unknown_keys_and_modifiers_are_refused_with_the_offending_text() {
        assert!(matches!(
            Chord::parse("hyper+q"),
            Err(KeymapError::UnknownModifier(_))
        ));
        assert!(matches!(
            Chord::parse("nope"),
            Err(KeymapError::UnknownKey(_))
        ));
        assert!(matches!(Chord::parse("   "), Err(KeymapError::Empty)));
        assert!(Chord::parse("f25").is_err(), "out-of-range function key");
    }

    /// The documented default keymap must be free of ambiguity: a chord that means two
    /// things is a bug the user would discover as a silent misbinding.
    #[test]
    fn the_default_keymap_has_no_conflicts() {
        assert_eq!(map().conflicts(), Vec::new());
        for profile in [Profile::Emacs, Profile::Vim] {
            let map = KeyMap::new(profile, &BTreeMap::new());
            assert_eq!(
                map.conflicts(),
                Vec::new(),
                "{} profile is ambiguous",
                map.profile_name()
            );
        }
    }

    #[test]
    fn the_documented_defaults_resolve_in_the_prompt() {
        let map = map();
        let cases = [
            (Chord::plain(Code::Enter), Action::SubmitPrompt),
            (Chord::new(Code::Enter, Mods::SHIFT), Action::NewLine),
            (Chord::new(Code::Enter, Mods::CTRL), Action::InterruptSend),
            (Chord::plain(Code::Esc), Action::StopTurn),
            (Chord::ctrl('p'), Action::OpenPalette),
            (Chord::plain(Code::F(1)), Action::Help),
            (Chord::ctrl('r'), Action::HistorySearch),
            (Chord::ctrl('o'), Action::ExpandToolOutput),
            (Chord::ctrl('y'), Action::CopyLastResponse),
            (Chord::new(Code::Backspace, Mods::CTRL), Action::DeleteWord),
            (Chord::new(Code::Left, Mods::CTRL), Action::WordLeft),
            (Chord::new(Code::Right, Mods::CTRL), Action::WordRight),
        ];
        for (chord, action) in cases {
            assert_eq!(
                map.resolve(chord, Scope::Prompt),
                Some(action),
                "{chord} should be {action:?}"
            );
        }
    }

    /// Escape is contextual: it stops a turn in the prompt and dismisses a dialog in a
    /// manager. Both bindings are defaults and neither is a conflict, because the scopes
    /// cannot be active at once.
    #[test]
    fn escape_is_scoped_rather_than_ambiguous() {
        let map = map();
        assert_eq!(
            map.resolve(Chord::plain(Code::Esc), Scope::Prompt),
            Some(Action::StopTurn)
        );
        assert_eq!(
            map.resolve(Chord::plain(Code::Esc), Scope::Manager),
            Some(Action::Dismiss)
        );
        assert!(map.conflicts().is_empty());
    }

    #[test]
    fn a_global_action_resolves_from_every_scope() {
        let map = map();
        for scope in [Scope::Prompt, Scope::Manager, Scope::Global] {
            assert_eq!(
                map.resolve(Chord::ctrl('o'), scope),
                Some(Action::ExpandToolOutput)
            );
        }
    }

    #[test]
    fn user_bindings_override_defaults_and_an_empty_value_unbinds() {
        let mut user = BTreeMap::new();
        user.insert("open_palette".to_string(), "ctrl+k".to_string());
        user.insert("help".to_string(), String::new());
        let map = KeyMap::new(Profile::Default, &user);
        assert_eq!(
            map.resolve(Chord::ctrl('k'), Scope::Prompt),
            Some(Action::OpenPalette)
        );
        assert_eq!(
            map.resolve(Chord::ctrl('p'), Scope::Prompt),
            None,
            "the default binding must be replaced, not duplicated"
        );
        assert_eq!(
            map.resolve(Chord::plain(Code::F(1)), Scope::Prompt),
            None,
            "an empty value unbinds"
        );
        assert!(map.conflicts().is_empty());
    }

    #[test]
    fn an_unknown_action_id_in_config_is_ignored_but_reported() {
        let mut user = BTreeMap::new();
        user.insert("not_an_action".into(), "ctrl+k".into());
        let keys = Keys {
            profile: Profile::Default,
            bindings: user,
        };
        let map = KeyMap::from_config(&keys);
        assert_eq!(map.resolve(Chord::ctrl('k'), Scope::Prompt), None);
        let findings = validate(&keys);
        assert!(
            findings.iter().any(|f| f.contains("not_an_action")),
            "{findings:?}"
        );
    }

    #[test]
    fn rebinding_refuses_a_chord_another_action_owns() {
        let map = map();
        let err = map
            .rebind(Action::Help, Chord::ctrl('p'))
            .expect_err("ctrl+p belongs to the palette");
        match err {
            KeymapError::Conflict(c) => {
                assert!(c.actions.contains(&Action::OpenPalette));
                assert!(c.actions.contains(&Action::Help));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // A different scope is not a collision: `Help` may take Escape, which then still
        // dismisses a dialog because the manager binding is the more specific one.
        let scoped = map
            .rebind(Action::Help, Chord::plain(Code::Esc))
            .expect("a manager-scoped chord is not owned by a global action");
        assert_eq!(
            scoped.resolve(Chord::plain(Code::Esc), Scope::Manager),
            Some(Action::Dismiss),
            "the specific scope still wins"
        );
    }

    #[test]
    fn rebinding_refuses_a_chord_the_terminal_needs() {
        let map = map();
        let err = map
            .rebind(Action::Help, Chord::ctrl('z'))
            .expect_err("ctrl+z suspends the process");
        assert!(format!("{err}").contains("reserved"), "{err}");
    }

    #[test]
    fn explicit_steal_takes_the_chord_and_frees_the_other_action() {
        let map = map();
        let stolen = map.steal(Action::Help, Chord::ctrl('p')).unwrap();
        assert_eq!(
            stolen.resolve(Chord::ctrl('p'), Scope::Prompt),
            Some(Action::Help)
        );
        assert_eq!(stolen.chord_for(Action::OpenPalette), None);
        assert!(stolen.conflicts().is_empty());
    }

    #[test]
    fn resetting_one_action_restores_its_default_without_touching_others() {
        let mut user = BTreeMap::new();
        user.insert("open_palette".into(), "ctrl+k".into());
        user.insert("help".into(), "f9".into());
        let map = KeyMap::new(Profile::Default, &user);
        let reset = map.reset(Action::OpenPalette);
        assert_eq!(reset.chord_for(Action::OpenPalette), Some(Chord::ctrl('p')));
        assert_eq!(
            reset.chord_for(Action::Help),
            Some(Chord::plain(Code::F(9))),
            "an unrelated override must survive"
        );
    }

    #[test]
    fn reset_all_returns_to_the_profile_defaults() {
        let mut user = BTreeMap::new();
        user.insert("open_palette".into(), "ctrl+k".into());
        let map = KeyMap::new(Profile::Vim, &user);
        let reset = map.reset_all();
        assert_eq!(reset.profile(), Profile::Vim);
        assert_eq!(
            reset.chord_for(Action::HistoryPrevious),
            Some(Chord::ctrl('k'))
        );
        assert_eq!(reset.chord_for(Action::OpenPalette), Some(Chord::ctrl('p')));
    }

    #[test]
    fn the_exported_overrides_are_exactly_what_reproduces_the_map() {
        let mut user = BTreeMap::new();
        user.insert("open_palette".into(), "ctrl+k".into());
        let map = KeyMap::new(Profile::Default, &user);
        let defaults = KeyMap::new(Profile::Default, &BTreeMap::new());
        let diff = defaults.overrides_of(&map);
        assert_eq!(diff.get("open_palette").map(String::as_str), Some("ctrl+k"));
        assert_eq!(
            diff.len(),
            1,
            "a default binding must not be exported: {diff:?}"
        );

        // Applying the exported diff to the defaults reproduces the map.
        let rebuilt = KeyMap::new(Profile::Default, &diff);
        for action in Action::ALL {
            assert_eq!(
                rebuilt.chord_for(action),
                map.chord_for(action),
                "{} does not survive export/import",
                action.id()
            );
        }
    }

    #[test]
    fn an_unbound_action_is_exported_as_an_empty_value() {
        let map = KeyMap::default().unbind(Action::Help);
        let diff = KeyMap::default().overrides_of(&map);
        assert_eq!(diff.get("help").map(String::as_str), Some(""));
        let rebuilt = KeyMap::new(Profile::Default, &diff);
        assert_eq!(rebuilt.chord_for(Action::Help), None);
    }

    #[test]
    fn profiles_change_the_documented_few_and_keep_the_rest() {
        let emacs = KeyMap::new(Profile::Emacs, &BTreeMap::new());
        assert_eq!(
            emacs.chord_for(Action::HistoryPrevious),
            Some(Chord::ctrl('p'))
        );
        assert_eq!(emacs.chord_for(Action::OpenPalette), Some(Chord::alt('p')));
        assert_eq!(
            emacs.chord_for(Action::Help),
            Some(Chord::plain(Code::F(1)))
        );

        let vim = KeyMap::new(Profile::Vim, &BTreeMap::new());
        assert_eq!(
            vim.chord_for(Action::HistoryPrevious),
            Some(Chord::ctrl('k'))
        );
        assert_eq!(
            vim.chord_for(Action::DeleteWord),
            Some(Chord::new(Code::Backspace, Mods::CTRL))
        );
    }

    #[test]
    fn every_action_has_a_description_a_scope_and_an_id() {
        for action in Action::ALL {
            assert!(!action.description().is_empty(), "{action:?}");
            assert!(!action.id().is_empty(), "{action:?}");
            assert_eq!(Action::parse(action.id()), Some(action));
        }
        let mut ids: Vec<&str> = Action::ALL.iter().map(|a| a.id()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "action ids must be unique");
    }

    #[test]
    fn validation_reports_protected_chords_and_conflicts() {
        let keys = Keys {
            profile: Profile::Default,
            bindings: BTreeMap::from([
                ("help".to_string(), "ctrl+z".to_string()),
                ("open_models".to_string(), "ctrl+p".to_string()),
            ]),
        };
        let findings = validate(&keys);
        assert!(
            findings.iter().any(|f| f.contains("suspend")),
            "protected chord must be reported: {findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.contains("open_palette")),
            "conflict must be reported: {findings:?}"
        );
    }

    #[test]
    fn the_default_config_is_valid() {
        assert!(validate(&Keys::default()).is_empty());
    }

    /// Every default binding must name a real key, so a typo in a default is caught here
    /// instead of silently making the action unreachable.
    #[test]
    fn every_default_chord_is_a_real_chord() {
        let map = map();
        for action in Action::ALL {
            if let Some(chord) = map.chord_for(action) {
                let text = chord.to_string();
                assert_eq!(
                    Chord::parse(&text).as_ref(),
                    Ok(&chord),
                    "{} default `{text}` does not parse",
                    action.id()
                );
            }
        }
    }
}
