//! Unified diffs for file mutations, so the chat can show *what changed* instead of
//! only "edited <path>".
//!
//! The diff is deliberately small and bounded: it is display material, not a patch
//! format for re-application (that is `apply_patch`'s job). Lines are emitted with the
//! conventional prefixes so the TUI can colour them without parsing further.

/// Maximum number of changed lines emitted per hunk before the diff is clipped.
pub const MAX_DIFF_LINES: usize = 120;

/// A unified diff between two texts, or `None` when they are identical.
///
/// Uses a single prefix/suffix trim (not a full LCS): for the edits a coding agent
/// makes — a few lines changed in a file — this produces exactly the readable hunk,
/// without pulling in a diff crate. Distant changes in one call collapse into one
/// hunk; that is a display compromise, and the clipped marker says so.
pub fn unified(label: &str, before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let x: Vec<&str> = before.lines().collect();
    let y: Vec<&str> = after.lines().collect();
    let newline_changed = before.ends_with('\n') != after.ends_with('\n');
    let mut prefix = x.iter().zip(&y).take_while(|(a, b)| a == b).count();
    if newline_changed && prefix == x.len() && prefix == y.len() {
        prefix = prefix.saturating_sub(1);
    }
    let suffix = if newline_changed {
        0
    } else {
        x[prefix..]
            .iter()
            .rev()
            .zip(y[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count()
    };
    let removed = &x[prefix..x.len() - suffix];
    let added = &y[prefix..y.len() - suffix];
    let mut out = String::new();
    out.push_str(&format!("--- {label}\n+++ {label}\n"));
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        if removed.is_empty() {
            prefix
        } else {
            prefix + 1
        },
        removed.len(),
        if added.is_empty() { prefix } else { prefix + 1 },
        added.len()
    ));
    let mut shown = 0usize;
    let mut clipped = 0usize;
    for line in removed {
        if shown >= MAX_DIFF_LINES {
            clipped += 1;
            continue;
        }
        out.push_str(&format!("-{line}\n"));
        shown += 1;
    }
    for line in added {
        if shown >= MAX_DIFF_LINES {
            clipped += 1;
            continue;
        }
        out.push_str(&format!("+{line}\n"));
        shown += 1;
    }
    if clipped > 0 {
        out.push_str(&format!("[diff clipped: {clipped} more line(s)]\n"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_produce_no_diff() {
        assert!(unified("a.rs", "same\n", "same\n").is_none());
    }

    #[test]
    fn a_changed_line_is_shown_as_removal_and_addition() {
        let d = unified("a.rs", "one\ntwo\nthree\n", "one\nTWO\nthree\n").unwrap();
        assert!(d.contains("--- a.rs"), "{d}");
        assert!(d.contains("@@ -2,1 +2,1 @@"), "{d}");
        assert!(d.contains("-two"), "{d}");
        assert!(d.contains("+TWO"), "{d}");
        assert!(
            !d.contains("-one"),
            "unchanged context must not appear: {d}"
        );
    }

    #[test]
    fn an_addition_only_edit_has_no_removals() {
        let d = unified("a.rs", "a\n", "a\nb\n").unwrap();
        assert!(d.contains("+b"), "{d}");
        assert!(!d.contains("\n-"), "nothing was removed: {d}");
    }

    #[test]
    fn a_trailing_newline_change_is_visible() {
        let d = unified("a.rs", "x\n", "x").unwrap();
        assert!(d.contains("-x") && d.contains("+x"), "{d}");
    }

    #[test]
    fn a_large_edit_is_clipped_with_a_marker() {
        let before: String = (0..MAX_DIFF_LINES + 40)
            .map(|i| format!("old{i}\n"))
            .collect();
        let after: String = (0..MAX_DIFF_LINES + 40)
            .map(|i| format!("new{i}\n"))
            .collect();
        let d = unified("big.rs", &before, &after).unwrap();
        assert!(d.contains("[diff clipped:"), "must admit the clipping: {d}");
        let shown = d
            .lines()
            .filter(|l| l.starts_with('+') || l.starts_with('-'))
            .count();
        assert!(
            shown <= MAX_DIFF_LINES + 2,
            "clipping must bound the output, got {shown} lines"
        );
    }
}
