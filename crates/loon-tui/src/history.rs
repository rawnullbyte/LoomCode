//! Persistent prompt history.
//!
//! The in-memory recall added with `Up`/`Down` is per-process: quitting loses it, so the
//! prompt you were refining yesterday is gone. This module keeps a bounded, deduplicated
//! list on disk under the state directory, one prompt per line, so recall survives a
//! restart.
//!
//! Format: one prompt per line with `\n` escaped as `\x1f` (unit separator) and backslash
//! as `\\`, so a multi-line prompt round-trips without inventing a second record. The file
//! is append-only while running and rewritten only when it exceeds the cap, which keeps the
//! common path a single append. A corrupt or truncated file yields whatever lines still
//! parse rather than failing the session: history is a convenience, never a blocker.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Most prompts retained on disk. The in-memory recall is smaller; this is the outer bound
/// so a long-lived install cannot grow the file without limit.
pub const MAX_HISTORY: usize = 500;

/// Escape a prompt for one-line storage.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push('\u{1f}'),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Reverse [`encode`]. Unknown escapes keep the backslash, so a hand-edited file is not
/// mangled.
fn decode(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            if c == '\u{1f}' {
                out.push('\n');
            } else {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The history file for a given state directory.
pub fn history_path(state_dir: &Path) -> PathBuf {
    state_dir.join("prompt-history")
}

/// Load history, oldest first. Missing or unreadable files are an empty history, not an
/// error: a fresh install has none, and a permissions problem must not stop the session.
pub fn load(state_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(history_path(state_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(decode)
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Append one prompt, trimming the file to [`MAX_HISTORY`] when it is over. An adjacent
/// duplicate is skipped so a re-sent prompt does not fill the file. Returns false when the
/// write failed (reported, never fatal).
pub fn append(state_dir: &Path, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.starts_with('/') {
        // Slash commands are not recallable (recalling one opens the completion popup),
        // so they are not worth a record.
        return false;
    }
    let path = history_path(state_dir);
    if std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.lines().next_back().map(|l| decode(l) == text))
        .unwrap_or(false)
    {
        return false;
    }
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        tracing::warn!(error = %e, "prompt history: cannot create state dir");
        return false;
    }
    let line = format!("{}\n", encode(text));
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = write {
        tracing::warn!(error = %e, "prompt history: cannot append");
        return false;
    }
    trim(&path);
    true
}

/// Rewrite the file keeping only the newest [`MAX_HISTORY`] entries. Best effort: a failure
/// leaves the longer file in place, which is preferable to losing history.
fn trim(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= MAX_HISTORY {
        return;
    }
    let keep = &lines[lines.len() - MAX_HISTORY..];
    let body: String = keep.iter().map(|l| format!("{l}\n")).collect();
    // Temp + rename so a crash cannot leave a half-written history.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_round_trip_including_newlines_and_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        for prompt in [
            "a simple prompt",
            "multi\nline\nprompt",
            "path with \\ backslash",
            "crlf\r\nprompt",
            "unicode ✓ 日本語",
            "  leading and trailing  ",
        ] {
            append(dir.path(), prompt);
        }
        let loaded = load(dir.path());
        assert_eq!(
            loaded,
            vec![
                "a simple prompt".to_string(),
                "multi\nline\nprompt".to_string(),
                "path with \\ backslash".to_string(),
                "crlf\r\nprompt".to_string(),
                "unicode ✓ 日本語".to_string(),
                // trailing/leading whitespace is trimmed on the way in
                "leading and trailing".to_string(),
            ],
            "every shape must survive a round trip"
        );
    }

    #[test]
    fn a_multi_line_prompt_is_still_one_line_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "first\nsecond");
        let raw = std::fs::read_to_string(history_path(dir.path())).unwrap();
        assert_eq!(
            raw.lines().count(),
            1,
            "a newline inside a prompt must not forge a second record: {raw:?}"
        );
    }

    #[test]
    fn an_adjacent_duplicate_is_not_stored_twice() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "same");
        append(dir.path(), "same");
        append(dir.path(), "other");
        append(dir.path(), "same");
        assert_eq!(
            load(dir.path()),
            vec!["same".to_string(), "other".to_string(), "same".to_string()],
            "only immediately repeated entries collapse"
        );
    }

    #[test]
    fn slash_commands_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "/quit");
        append(dir.path(), "/model x");
        append(dir.path(), "a real prompt");
        assert_eq!(load(dir.path()), vec!["a real prompt".to_string()]);
    }

    #[test]
    fn the_file_is_bounded_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..MAX_HISTORY + 25 {
            append(dir.path(), &format!("prompt {n}"));
        }
        let loaded = load(dir.path());
        assert_eq!(loaded.len(), MAX_HISTORY, "the file stays bounded");
        assert_eq!(loaded.first().unwrap(), "prompt 25");
        assert_eq!(
            loaded.last().unwrap(),
            &format!("prompt {}", MAX_HISTORY + 24)
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_history_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn a_corrupt_line_is_survivable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            history_path(dir.path()),
            "good prompt\nbad\u{0}\u{7f} noise\nafter\n",
        )
        .unwrap();
        // Must not panic, and must return something usable.
        let loaded = load(dir.path());
        assert!(
            loaded.iter().any(|s| s == "good prompt"),
            "readable lines survive: {loaded:?}"
        );
    }

    #[test]
    fn a_truncated_final_line_does_not_lose_earlier_entries() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "first complete");
        append(dir.path(), "second complete");
        // Simulate a crash mid-append: a partial line with no newline.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(history_path(dir.path()))
            .unwrap();
        f.write_all(b"partial with no newline").unwrap();
        drop(f);
        let loaded = load(dir.path());
        assert!(loaded.contains(&"first complete".to_string()));
        assert!(loaded.contains(&"second complete".to_string()));
        // The partial line is still readable text, so it is kept rather than dropped.
        assert!(loaded.iter().any(|s| s.starts_with("partial")));
    }
}
