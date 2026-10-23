//! Slash-command autocomplete: prefix-filtered suggestions driven by the canonical
//! command table, plus argument suggestions (`/config <key>`, `/model <ref>`,
//! `/effort <level>`, …) exactly like Claude Code's inline suggestions.

use loon_core::commands::{command_list, settings_registry};

/// What kind of thing a completion replaces. Tab's behaviour differs: a command word
/// replaces the whole input (`/re` → `/redo `), while an argument value replaces only
/// the word being typed (`/config ti` → `/config tips `). Deriving that from the input
/// text instead of from the completion is what turned `/autocompact` + Tab into `/90% `.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Argument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Text shown in the popup (command with its argument hint, or a suggested value).
    /// Only [`Kind::Command`] rows carry the leading `/`, which the popup adds itself.
    pub display: String,
    /// What Tab inserts (the command word, or the argument value).
    pub insert: String,
    pub description: String,
    pub kind: Kind,
}

impl Completion {
    fn command(name: &str, description: &str) -> Completion {
        Completion {
            display: name.to_string(),
            insert: name.split(' ').next().unwrap_or(name).to_string(),
            description: description.to_string(),
            kind: Kind::Command,
        }
    }

    fn argument(value: &str) -> Completion {
        Completion {
            display: value.to_string(),
            insert: value.to_string(),
            description: String::new(),
            kind: Kind::Argument,
        }
    }
}

/// True when the popup should show: a `/word` being typed, or a command whose
/// arguments have known suggestions.
pub fn popup_active(input: &str) -> bool {
    let line = input.trim_start();
    if !line.starts_with('/') {
        return false;
    }
    !completions_for(line).is_empty()
}

/// Suggested argument values for the partially-typed argument of `line`
/// (e.g. `/config ti` → tips; `/config tips ` → true, false).
pub fn argument_suggestions(line: &str) -> Vec<Completion> {
    let body = line.trim_start().trim_start_matches('/');
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or_default();
    let args: Vec<&str> = parts.collect();
    // Trailing whitespace means "the next argument starts now". Tabs arrive from
    // pasted input and some terminals, and a trailing tab used to keep suggesting
    // keys instead of the value for the completed key.
    let trailing_space = body.ends_with(char::is_whitespace);
    // With no whitespace yet the command word is still being typed: `/auto` is a
    // command prefix, not the command `auto` with an empty first argument. Treating
    // every bare word as "argument 0" made `/autocompact` offer `90%`, and Tab then
    // replaced the command with the value. Only an exact command name opens its
    // argument list, and then Tab appends the value after the command.
    if args.is_empty() && !trailing_space && !is_exact_command(cmd) {
        return Vec::new();
    }
    let arg_index = if trailing_space {
        args.len()
    } else {
        args.len().saturating_sub(1)
    };
    let partial = if trailing_space {
        ""
    } else {
        args.last().copied().unwrap_or("")
    };
    match (cmd, arg_index) {
        ("config", 0) => settings_registry()
            .into_iter()
            .filter(|d| d.key.starts_with(partial))
            .map(|d| Completion {
                display: format!("{}  {}", d.key, d.description),
                insert: d.key.to_string(),
                description: d.description.to_string(),
                kind: Kind::Argument,
            })
            .collect(),
        ("config", 1) => {
            let key = args.first().copied().unwrap_or_default();
            loon_core::commands::setting_def(key)
                .map(|d| d.kind.suggestions())
                .unwrap_or_default()
                .into_iter()
                .filter(|v| v.starts_with(partial))
                .map(|v| Completion::argument(&v))
                .collect()
        }
        ("effort", 0) => ["low", "medium", "high", "xhigh", "ultra"]
            .into_iter()
            .filter(|v| v.starts_with(partial))
            .map(Completion::argument)
            .collect(),
        ("theme", 0) => [
            "terminal", "dark", "light", "ocean", "violet", "ember", "graphite",
        ]
        .into_iter()
        .filter(|v| v.starts_with(partial))
        .map(Completion::argument)
        .collect(),
        // Token sizes accept shorthand (`1m`, `100k`) and percentages for /autocompact.
        ("autocompact", 0) => [
            "50k", "100k", "200k", "1m", "80%", "90%", "on", "off", "auto",
        ]
        .into_iter()
        .filter(|v| v.starts_with(partial))
        .map(Completion::argument)
        .collect(),
        ("context", 0) => ["1m", "200k", "100k", "50k", "90%", "80%", "off", "auto"]
            .into_iter()
            .filter(|v| v.starts_with(partial))
            .map(Completion::argument)
            .collect(),
        _ => Vec::new(),
    }
}

/// True when `word` is a command name in the canonical table (its first word, since
/// table rows carry an argument hint: `autocompact [1m|200k|90%|on|off|auto]`).
fn is_exact_command(word: &str) -> bool {
    !word.is_empty()
        && command_list()
            .iter()
            .any(|(name, _)| name.split(' ').next() == Some(word))
}

/// Completions for the current input, in table order. `/` alone lists everything.
pub fn completions_for(input: &str) -> Vec<Completion> {
    let line = input.trim_start();
    if !line.starts_with('/') {
        return Vec::new();
    }
    let args = argument_suggestions(line);
    if !args.is_empty() {
        return args;
    }
    let typed = line.trim_start_matches('/');
    let typed = typed.split(char::is_whitespace).next().unwrap_or_default();
    if typed.is_empty() {
        return command_list()
            .into_iter()
            .map(|(name, desc)| Completion::command(name, desc))
            .collect();
    }
    // Command-word rows only complete the bare word being typed. Once whitespace
    // appears the argument list owns the popup (`/undo 2` shows nothing); an exact
    // command name like `/autocompact` already returned its values above, so Tab
    // appends `90%` instead of swapping the command for the value.
    if line.contains(char::is_whitespace) {
        return Vec::new();
    }
    command_list()
        .into_iter()
        .filter(|(name, _)| name.split(' ').next().unwrap_or(name).starts_with(typed))
        .map(|(name, desc)| Completion::command(name, desc))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slash_lists_everything_in_table_order() {
        let c = completions_for("/");
        assert_eq!(
            c.len(),
            command_list().len(),
            "one entry per command_list row"
        );
        assert_eq!(c[0].insert, "help");
        assert_eq!(c[0].description, "this list");
        // order follows the table: help before undo before redo…
        assert!(c[0].insert != c[1].insert);
    }

    #[test]
    fn prefix_filters_by_command_word() {
        let c = completions_for("/re");
        let names: Vec<&str> = c.iter().map(|x| x.insert.as_str()).collect();
        assert_eq!(
            names,
            vec!["redo", "rewind", "recap", "resume", "rename"],
            "prefix on the word, args ignored"
        );
        // argument hints still shown
        assert_eq!(c[1].display, "rewind [n] [mode]");
    }

    #[test]
    fn popup_deactivates_once_arguments_start() {
        assert!(popup_active("/rewind"));
        assert!(!popup_active("/rewind 2"));
        assert!(!popup_active("plain text"));
        assert!(!popup_active(""));
        assert!(popup_active("  /undo")); // leading spaces tolerated
    }

    #[test]
    fn no_space_after_slash_is_needed_for_list() {
        assert!(completions_for("/undo 2").is_empty());
        assert!(completions_for("/und").len() == 1);
    }

    /// A bare word that is *not* a command must not be read as "command X with an
    /// empty first argument". `/auto` is a prefix of `/autocompact`, so it lists
    /// commands — never `90%`/`off`/`auto` as if the command were already chosen.
    #[test]
    fn a_command_prefix_offers_commands_not_argument_values() {
        for typed in ["/auto", "/aut", "/au"] {
            let c = completions_for(typed);
            assert!(
                c.iter().all(|x| x.kind == Kind::Command),
                "{typed:?} must offer command rows, got {c:?}"
            );
            assert!(
                c.iter().any(|x| x.insert == "autocompact"),
                "{typed:?} should still complete the command name, got {c:?}"
            );
            assert!(
                !c.iter().any(|x| x.insert == "90%"),
                "{typed:?} must not offer argument values, got {c:?}"
            );
        }
    }

    /// The command word completed with a space opens its argument list, and those
    /// rows are values (no slash), so the popup shows `90%`, not `/90%`.
    #[test]
    fn a_complete_command_offers_its_arguments_as_values() {
        for line in ["/autocompact", "/autocompact ", "/context", "/context "] {
            let c = completions_for(line);
            assert!(
                c.iter().all(|x| x.kind == Kind::Argument),
                "{line:?} must offer argument rows, got {c:?}"
            );
            let names: Vec<&str> = c.iter().map(|x| x.insert.as_str()).collect();
            assert!(
                names.contains(&"90%"),
                "{line:?} should offer 90%: {names:?}"
            );
            assert!(
                c.iter().all(|x| !x.display.starts_with('/')),
                "argument values must not carry a slash: {c:?}"
            );
        }
    }

    #[test]
    fn command_rows_are_slashed_and_argument_rows_are_not() {
        let cmd = &completions_for("/und")[0];
        assert_eq!(cmd.kind, Kind::Command);
        assert_eq!(cmd.display, "undo");
        let arg = &completions_for("/effort ")[0];
        assert_eq!(arg.kind, Kind::Argument);
        assert_eq!(arg.display, "low");
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    #[test]
    fn config_key_then_value_suggestions() {
        let keys = completions_for("/config ");
        assert!(keys.iter().any(|c| c.insert == "tips"), "{keys:?}");
        assert!(keys.iter().any(|c| c.insert == "permission_mode"));
        // prefix narrows keys
        let narrowed = completions_for("/config per");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].insert, "permission_mode");
        // after a key + space, values are suggested
        let vals = completions_for("/config permission_mode ");
        let names: Vec<&str> = vals.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(names, vec!["bypass", "auto", "ask", "allowlist"]);
        // bool setting
        let bools = completions_for("/config tips ");
        let names: Vec<&str> = bools.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(names, vec!["true", "false"]);
        // partial value filters
        let partial = completions_for("/config theme tr");
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].insert, "transparent");
    }

    /// "The next argument starts now" is signalled by trailing whitespace of any
    /// kind. Only a literal space was checked, so after `/config tips` + Tab the
    /// popup kept suggesting *keys* instead of that setting's values — the same
    /// input with a trailing space behaved correctly.
    #[test]
    fn a_trailing_tab_advances_to_value_suggestions() {
        for line in ["/config tips\t", "/config\ttips\t"] {
            let vals = completions_for(line);
            let names: Vec<&str> = vals.iter().map(|c| c.insert.as_str()).collect();
            assert_eq!(
                names,
                vec!["true", "false"],
                "trailing tab must mean the next argument, got {names:?} for {line:?}"
            );
        }
    }

    #[test]
    fn effort_and_theme_argument_suggestions() {
        let e = completions_for("/effort ");
        let names: Vec<&str> = e.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(names, vec!["low", "medium", "high", "xhigh", "ultra"]);
        let t = completions_for("/theme oc");
        assert_eq!(t[0].insert, "ocean");
    }
}
