//! The command palette (Ctrl+P): one search box over everything the application can do.
//!
//! The palette is a *dispatcher*, not a second command list. Every entry carries the input it
//! sends when accepted — a slash command, or a prompt — so there is no parallel registry to
//! drift out of sync with [`loon_core::commands::command_list`]. Scoring is subsequence
//! matching with bonuses for word starts and contiguous runs, which is what makes
//! `opp` find `open_palette` and `mdl` find `model`.

use loon_config::keymap::Action;

/// What accepting an entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accept {
    /// Submit this input exactly as if it had been typed (slash commands included).
    Input(String),
    /// Run a keymap action directly, for surfaces that have no slash command.
    Action(Action),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What the user reads.
    pub label: String,
    /// Where it lives, so two identical labels stay distinguishable.
    pub category: &'static str,
    pub hint: String,
    pub accept: Accept,
    /// Extra text matched but not shown (aliases, argument names).
    pub keywords: String,
}

impl Entry {
    pub fn command(label: &str, hint: &str, input: &str) -> Entry {
        Entry {
            label: label.to_string(),
            category: "Command",
            hint: hint.to_string(),
            accept: Accept::Input(input.to_string()),
            keywords: String::new(),
        }
    }
    pub fn action(action: Action, hint: &str) -> Entry {
        Entry {
            label: action.id().to_string(),
            category: "Shortcut",
            hint: hint.to_string(),
            accept: Accept::Action(action),
            keywords: String::new(),
        }
    }
    pub fn with_keywords(mut self, keywords: &str) -> Entry {
        self.keywords = keywords.to_string();
        self
    }
}

/// One scored result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub entry: Entry,
    pub score: i32,
}

/// Subsequence score, or `None` when the query is not a subsequence of the text.
///
/// Rules that make the ranking feel right rather than merely correct:
/// - a match at a word start (start of string, after `_`, `-`, space or `/`) scores highest;
/// - a contiguous run beats the same characters scattered;
/// - a prefix match dominates everything.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let needle: Vec<char> = query.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    let lower = text.to_lowercase();
    let starts_with = lower.starts_with(&query.to_lowercase());
    for &want in &needle {
        let mut found = None;
        while at < hay.len() {
            if hay[at] == want {
                found = Some(at);
                break;
            }
            at += 1;
        }
        let pos = found?;
        // Word boundary: the previous character separates words.
        let boundary = pos == 0
            || hay
                .get(pos - 1)
                .is_some_and(|c| matches!(c, '_' | '-' | ' ' | '/' | '.' | '('));
        score += if boundary { 12 } else { 2 };
        if previous == Some(pos.wrapping_sub(1)) {
            score += 8; // contiguous run
        }
        previous = Some(pos);
        at = pos + 1;
    }
    if starts_with {
        score += 40;
    }
    // Shorter labels are more likely what the user meant.
    score -= text.chars().count() as i32 / 8;
    Some(score)
}

/// The open palette: query, ranked hits and the selection.
#[derive(Debug, Clone)]
pub struct Palette {
    entries: Vec<Entry>,
    pub query: String,
    pub selected: usize,
    /// Cap on rendered rows; the count of the rest is shown.
    pub max_rows: usize,
}

impl Palette {
    pub fn new(entries: Vec<Entry>) -> Palette {
        Palette {
            entries,
            query: String::new(),
            selected: 0,
            max_rows: 8,
        }
    }

    pub fn with_query(mut self, query: &str) -> Palette {
        self.query = query.to_string();
        self
    }

    /// Ranked results. With an empty query the natural order is kept, so the palette opens on
    /// a stable, recognisable list instead of a random one.
    pub fn hits(&self) -> Vec<Hit> {
        let mut hits: Vec<Hit> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let label = fuzzy_score(&self.query, &entry.label);
                let keywords = fuzzy_score(&self.query, &entry.keywords);
                let category = fuzzy_score(&self.query, entry.category);
                // A label match is worth most; a keyword match can still surface an entry
                // whose visible label does not contain the query.
                let score = match (label, keywords) {
                    (Some(l), k) => l.max(0) + k.unwrap_or(0) / 2,
                    (None, Some(k)) => k / 2,
                    (None, None) => return None,
                };
                let score = score + category.map(|c| c / 4).unwrap_or(0);
                Some(Hit {
                    entry: entry.clone(),
                    score,
                })
            })
            .collect();
        if self.query.is_empty() {
            // Keep entry order but float entries with a real action above the rest? No:
            // stable order is more predictable. Just cap the list.
            hits.truncate(self.max_rows);
            return hits;
        }
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.entry.label.len().cmp(&b.entry.label.len()))
                .then_with(|| a.entry.label.cmp(&b.entry.label))
        });
        hits.truncate(self.max_rows);
        hits
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.hits().get(self.selected).map(|h| h.entry.clone())
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.hits().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.selected = 0;
    }
}

/// Everything the palette lists, gathered by the caller from live application state. A struct
/// rather than a long argument list: each field is a distinct data source, and adding one must not
/// require touching every call site positionally.
pub struct Sources<'a> {
    /// The canonical slash-command table.
    pub commands: &'a [(&'static str, &'static str)],
    pub models: &'a [String],
    pub recent_models: &'a [String],
    pub themes: &'a [&'a str],
    /// Session id and title.
    pub sessions: &'a [(String, String)],
    pub settings: &'a [(&'static str, &'static str)],
    /// Execution mode id and description.
    pub modes: &'a [(&'static str, &'static str)],
    /// Resolve an action's current chord, so shortcut entries show the real key.
    pub bound: &'a dyn Fn(Action) -> Option<String>,
}

/// Build the palette's entries from the live application state.
pub fn entries(sources: &Sources<'_>) -> Vec<Entry> {
    let mut out = command_entries(sources);
    for (id, description) in sources.modes {
        out.push(
            Entry::command(&format!("/mode {id}"), description, &format!("/mode {id} "))
                .with_keywords(
                    "mode ultrawork team plan execute research review debug security repair",
                ),
        );
    }
    out
}

fn command_entries(sources: &Sources<'_>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for (name, description) in sources.commands {
        // The command table carries its usage in the same string; the palette shows the
        // action word and matches the argument shape through keywords.
        let word = name.split_whitespace().next().unwrap_or(name);
        out.push(
            Entry::command(&format!("/{word}"), description, &format!("/{name} "))
                .with_keywords(name),
        );
    }
    for model in sources.recent_models.iter().take(5) {
        out.push(Entry::command(
            &format!("/model {model}"),
            "recent model",
            &format!("/model {model}"),
        ));
    }
    for model in sources.models.iter() {
        out.push(
            Entry::command(
                &format!("/model {model}"),
                "switch model",
                &format!("/model {model}"),
            )
            .with_keywords("model provider"),
        );
    }
    for theme in sources.themes.iter() {
        out.push(
            Entry::command(
                &format!("/theme {theme}"),
                "apply this palette",
                &format!("/theme {theme}"),
            )
            .with_keywords("theme colour color appearance"),
        );
    }
    for (id, title) in sources.sessions.iter() {
        out.push(
            Entry::command(&format!("/resume {id}"), title, &format!("/resume {id}"))
                .with_keywords("session resume"),
        );
    }
    for (key, description) in sources.settings.iter() {
        out.push(
            Entry::command(
                &format!("/config edit {key}"),
                description,
                &format!("/config edit {key}"),
            )
            .with_keywords("setting config"),
        );
    }
    // Shortcuts: shown only when actually bound, with the chord in the hint so the palette
    // teaches the keyboard along the way.
    for action in [
        Action::OpenKeybindings,
        Action::ToggleDetails,
        Action::ExpandToolOutput,
        Action::CopyLastResponse,
        Action::Help,
        Action::OpenThemes,
    ] {
        if let Some(chord) = (*sources.bound)(action) {
            out.push(Entry {
                label: action.id().to_string(),
                category: "Shortcut",
                hint: format!("{chord} — {}", action.description()),
                accept: Accept::Action(action),
                keywords: "keyboard shortcut".into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::new(vec![
            Entry::command("/model", "show or switch model", "/model "),
            Entry::command("/mode", "set the execution mode", "/mode "),
            Entry::command("/compact", "summarize older context", "/compact "),
            Entry::command("/resume", "open the session browser", "/resume "),
            Entry::action(Action::OpenPalette, "Ctrl+P"),
        ])
    }

    #[test]
    fn an_empty_query_keeps_the_stable_order_and_caps_the_list() {
        let mut p = palette();
        p.max_rows = 3;
        let hits = p.hits();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].entry.label, "/model", "order must be stable");
    }

    #[test]
    fn a_prefix_match_outranks_a_scattered_one() {
        let hits = palette().with_query("mode").hits();
        assert_eq!(hits[0].entry.label, "/mode", "exact prefix must win");
        // `/model` contains m-o-d-e as a subsequence; it must still be offered.
        assert!(hits.iter().any(|h| h.entry.label == "/model"));
    }

    #[test]
    fn an_acronym_finds_a_multi_word_action() {
        let p = Palette::new(vec![Entry::action(Action::OpenPalette, "open the palette")]);
        // The action id is snake_case, so `op` matches at a word start.
        let hits = p.with_query("op").hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.label, "open_palette");
    }

    #[test]
    fn a_query_that_is_not_a_subsequence_matches_nothing() {
        assert_eq!(palette().with_query("zzz").hits(), Vec::new());
        assert_eq!(fuzzy_score("abc", "acb"), None);
    }

    #[test]
    fn matching_is_case_insensitive_and_respects_word_starts() {
        assert!(fuzzy_score("MODEL", "/model").unwrap() > 0);
        let boundary = fuzzy_score("m", "/model").unwrap();
        let inner = fuzzy_score("o", "/model").unwrap();
        assert!(
            boundary > inner,
            "a word start must outrank a mid-word character ({boundary} vs {inner})"
        );
    }

    #[test]
    fn keywords_surface_an_entry_whose_label_does_not_match() {
        let p = Palette::new(vec![Entry::command("/resume", "open sessions", "/resume ")
            .with_keywords("session switcher")]);
        let hits = p.with_query("switcher").hits();
        assert_eq!(hits.len(), 1, "keyword text must be searchable");
    }

    #[test]
    fn selection_is_bounded_and_resets_when_the_query_changes() {
        let mut p = palette();
        p.move_by(10);
        assert_eq!(p.selected, p.hits().len().saturating_sub(1));
        p.move_by(-100);
        assert_eq!(p.selected, 0);
        p.selected = 2;
        p.push('m');
        assert_eq!(p.selected, 0, "typing must reset the selection");
        assert_eq!(p.query, "m");
        p.backspace();
        assert_eq!(p.query, "");
        p.clear();
        assert!(p.query.is_empty());
    }

    #[test]
    fn accepting_a_command_entry_yields_the_exact_input() {
        let p = palette();
        let entry = p
            .hits()
            .into_iter()
            .map(|h| h.entry)
            .find(|e| e.label == "/model")
            .unwrap();
        assert_eq!(entry.accept, Accept::Input("/model ".into()));
    }

    /// The palette must not invent entries: a command in the table is reachable through
    /// `/word`, and the usage text is searchable.
    #[test]
    fn entries_come_from_the_real_command_table() {
        let commands: Vec<(&'static str, &'static str)> = loon_core::commands::command_list();
        let models = ["mock/one".to_string()];
        let themes = ["dark", "light"];
        let sessions = [("01abc".to_string(), "a session".to_string())];
        let settings = [("effort", "coding effort")];
        let bound = |_: Action| Some("ctrl+x".to_string());
        let built = entries(&Sources {
            commands: &commands,
            models: &models,
            recent_models: &models,
            themes: &themes,
            sessions: &sessions,
            settings: &settings,
            modes: &[],
            bound: &bound,
        });
        assert!(
            built.len() > commands.len(),
            "models/themes/settings add entries"
        );
        for command in &commands {
            let word = command.0.split_whitespace().next().unwrap();
            assert!(
                built.iter().any(|e| e.label == format!("/{word}")),
                "`/{word}` is missing from the palette"
            );
        }
    }

    #[test]
    fn shortcuts_are_listed_only_when_bound() {
        let commands = [("help", "help")];
        let none = |_: Action| None;
        let built = entries(&Sources {
            commands: &commands,
            models: &[],
            recent_models: &[],
            themes: &[],
            sessions: &[],
            settings: &[],
            modes: &[],
            bound: &none,
        });
        assert!(
            !built.iter().any(|e| e.category == "Shortcut"),
            "an unbound action must not be offered"
        );
        let f1 = |_: Action| Some("f1".to_string());
        let built = entries(&Sources {
            commands: &commands,
            models: &[],
            recent_models: &[],
            themes: &[],
            sessions: &[],
            settings: &[],
            modes: &[],
            bound: &f1,
        });
        assert!(built.iter().any(|e| e.category == "Shortcut"));
    }
}
