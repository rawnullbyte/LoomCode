//! Slash-command autocomplete: prefix-filtered suggestions while the input is a bare
//! `/word`, driven by the canonical command table from loon-core.

use loon_core::commands::command_list;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Command with its argument hint, e.g. `rewind [n] [mode]`.
    pub display: String,
    /// Just the command word, e.g. `rewind` — what Tab inserts.
    pub insert: String,
    pub description: String,
}

/// True when the popup should show for this input: starts with `/` and contains no
/// whitespace yet (once arguments start, the command word is settled).
pub fn popup_active(input: &str) -> bool {
    let line = input.trim_start();
    line.starts_with('/') && !line.contains(char::is_whitespace)
}

/// Completions for the current input, in table order. `/` alone lists everything.
pub fn completions_for(input: &str) -> Vec<Completion> {
    if !popup_active(input) {
        return Vec::new();
    }
    let typed = input.trim_start().trim_start_matches('/');
    command_list()
        .into_iter()
        .filter(|(name, _)| name.split(' ').next().unwrap_or(name).starts_with(typed))
        .map(|(name, desc)| Completion {
            display: name.to_string(),
            insert: name.split(' ').next().unwrap_or(name).to_string(),
            description: desc.to_string(),
        })
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
            vec!["redo", "rewind", "recap"],
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
}
