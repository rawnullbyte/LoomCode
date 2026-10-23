//! Lightweight syntax highlighting for code in the transcript.
//!
//! This is deliberately not a parser: the transcript shows *excerpts* of files and
//! model output, often truncated mid-line, so a lexer that can tolerate partial input
//! and never panics is more useful than an accurate one. The goal is that a block of
//! Rust, Python or JSON is readable at a glance — comments, strings and keywords
//! separated from the noise — not that it round-trips.

use ratatui::style::{Color, Modifier, Style};

/// Language family, chosen from the fence's info string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    Shell,
    Json,
    Toml,
    Plain,
}

impl Lang {
    /// Map a fence tag (`rust`, `rs`, `py`, …) to a family. Unknown tags are `Plain`,
    /// which still gets comment and string colouring via the C-like default.
    pub fn from_tag(tag: &str) -> Lang {
        match tag.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Lang::Rust,
            "python" | "py" | "python3" => Lang::Python,
            "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "c" | "cpp" | "c++"
            | "java" | "go" | "kotlin" | "swift" | "scala" => Lang::JavaScript,
            "bash" | "sh" | "shell" | "zsh" | "console" => Lang::Shell,
            "json" | "jsonc" => Lang::Json,
            "toml" => Lang::Toml,
            _ => Lang::Plain,
        }
    }

    /// Infer the family from a file path's extension. Used for tool excerpts, which
    /// carry a path but no fence tag.
    pub fn from_path(path: &str) -> Lang {
        let ext = path
            .rsplit_once('.')
            .map(|(_, e)| e)
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "java" | "kt" | "swift" | "c"
            | "h" | "cc" | "cpp" | "hpp" | "cs" | "scala" => Lang::JavaScript,
            "sh" | "bash" | "zsh" | "fish" => Lang::Shell,
            "json" | "jsonc" => Lang::Json,
            "toml" => Lang::Toml,
            _ => Lang::Plain,
        }
    }

    /// Keywords worth standing out, per family. Short and high-signal on purpose.
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "fn", "let", "mut", "pub", "impl", "struct", "enum", "trait", "match", "if",
                "else", "for", "while", "loop", "return", "use", "mod", "crate", "self", "Self",
                "async", "await", "move", "where", "dyn", "ref", "const", "static", "unsafe", "as",
                "in", "break", "continue", "type", "box", "super", "extern",
            ],
            Lang::Python => &[
                "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from",
                "as", "with", "try", "except", "finally", "raise", "yield", "lambda", "None",
                "True", "False", "and", "or", "not", "in", "is", "pass", "global", "nonlocal",
                "assert", "del", "async", "await",
            ],
            Lang::JavaScript => &[
                "function",
                "const",
                "let",
                "var",
                "return",
                "if",
                "else",
                "for",
                "while",
                "class",
                "new",
                "this",
                "import",
                "export",
                "from",
                "default",
                "async",
                "await",
                "try",
                "catch",
                "finally",
                "throw",
                "typeof",
                "instanceof",
                "null",
                "undefined",
                "true",
                "false",
                "interface",
                "type",
                "extends",
                "implements",
                "public",
                "private",
            ],
            Lang::Shell => &[
                "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
                "function", "return", "export", "local", "set", "unset", "source", "cd", "echo",
            ],
            Lang::Json | Lang::Toml | Lang::Plain => &["true", "false", "null"],
        }
    }

    /// The comment prefix, if the family has one.
    fn comment(self) -> Option<&'static str> {
        match self {
            Lang::Rust | Lang::JavaScript => Some("//"),
            Lang::Python | Lang::Shell | Lang::Toml => Some("#"),
            Lang::Json | Lang::Plain => None,
        }
    }

    /// String delimiters for the family.
    fn quotes(self) -> &'static [char] {
        match self {
            Lang::Rust => &['"'],
            Lang::Python => &['"', '\''],
            Lang::JavaScript | Lang::Json => &['"', '\'', '`'],
            Lang::Shell => &['"', '\''],
            Lang::Toml => &['"', '\''],
            Lang::Plain => &['"', '\''],
        }
    }
}

/// Colours for one highlighted line. Kept as free functions so the transcript can use
/// them without threading a theme through the lexer.
pub fn keyword_style() -> Style {
    Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
}
pub fn string_style() -> Style {
    Style::default().fg(Color::Green)
}
pub fn comment_style() -> Style {
    Style::default().fg(Color::DarkGray)
}
pub fn number_style() -> Style {
    Style::default().fg(Color::Yellow)
}
pub fn plain_style() -> Style {
    Style::default().fg(Color::Reset)
}

/// One highlighted span: text plus its style.
pub type Piece = (String, Style);

/// Split a `read` result line into its line-number gutter and the code after it.
///
/// The tool emits `{:>6}\t{line}`, and `display` turns the tab into spaces, so a
/// gutter looks like a run of spaces, a number, then spaces. Returning the two halves
/// lets the caller highlight only the code: styling the gutter would paint every line
/// number as a numeric literal.
pub fn split_gutter(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    // The number must be followed by whitespace, not by more code.
    let mut gap = i;
    while gap < bytes.len() && bytes[gap] == b' ' {
        gap += 1;
    }
    if gap == i {
        return None;
    }
    Some(line.split_at(gap))
}

/// Highlight a single line. Never fails, never panics, and always returns pieces whose
/// concatenation equals `line` exactly — the transcript's copy/selection logic depends
/// on that, so dropping or reordering characters would corrupt copied text.
pub fn highlight(line: &str, lang: Lang) -> Vec<Piece> {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut i = 0usize;
    let mut buf = String::new();

    // Flush the pending plain run into the output.
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push((std::mem::take(&mut buf), plain_style()));
            }
        };
    }

    // Comment markers are fixed ASCII prefixes; precompute them as chars so the check
    // below is an in-place slice comparison. `starts_with` on a `&str` slice would
    // re-materialize the rest of the line on every character (that was quadratic);
    // a char-slice prefix test is constant work.
    let comment: Option<Vec<char>> = lang.comment().map(|m| m.chars().collect());
    while i < chars.len() {
        // A comment runs to end of line.
        if let Some(marker) = comment.as_ref() {
            if chars.len() - i >= marker.len() && chars[i..i + marker.len()] == marker[..] {
                flush!();
                let text: String = chars[i..].iter().collect();
                out.push((text, comment_style()));
                return coalesce(out);
            }
        }

        // Strings: scan to the matching delimiter, tolerating an unterminated line
        // (an excerpt is often cut mid-string).
        if lang.quotes().contains(&chars[i]) {
            let quote = chars[i];
            flush!();
            let mut text = String::new();
            text.push(quote);
            i += 1;
            while i < chars.len() {
                let c = chars[i];
                text.push(c);
                i += 1;
                if c == '\\' && i < chars.len() {
                    text.push(chars[i]);
                    i += 1;
                    continue;
                }
                if c == quote {
                    break;
                }
            }
            out.push((text, string_style()));
            continue;
        }

        // Numbers: a digit that starts a token.
        if chars[i].is_ascii_digit() && (i == 0 || !is_word(chars[i - 1])) {
            flush!();
            let mut text = String::new();
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                text.push(chars[i]);
                i += 1;
            }
            out.push((text, number_style()));
            continue;
        }

        // Identifiers: keywords get their own style, everything else stays plain.
        if is_word(chars[i]) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if lang.keywords().contains(&word.as_str()) {
                flush!();
                out.push((word, keyword_style()));
            } else {
                buf.push_str(&word);
            }
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }
    flush!();
    coalesce(out)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Merge adjacent pieces with the same style so the renderer emits fewer spans.
fn coalesce(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for (text, style) in pieces {
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((prev, prev_style)) if *prev_style == style => prev.push_str(&text),
            _ => out.push((text, style)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character must survive highlighting: the transcript copies these spans
    /// verbatim, so a dropped char would corrupt a copied command.
    fn round_trip(line: &str, lang: Lang) {
        let joined: String = highlight(line, lang).into_iter().map(|(t, _)| t).collect();
        assert_eq!(joined, line, "highlighting must not alter the text");
    }

    #[test]
    fn highlighting_never_alters_the_text() {
        let samples = [
            "",
            "let x = 42;",
            "fn main() { println!(\"hi\"); }",
            "    // a comment with \"quotes\" inside",
            "# python comment",
            "{\"key\": \"value\", \"n\": 1}",
            "unterminated \"string",
            "trailing backslash \\",
            "mixed 3.14 and 0xFF and snake_case",
            "日本語のテキスト",
            "emoji 🦀 here",
        ];
        for line in samples {
            for lang in [
                Lang::Rust,
                Lang::Python,
                Lang::JavaScript,
                Lang::Shell,
                Lang::Json,
                Lang::Toml,
                Lang::Plain,
            ] {
                round_trip(line, lang);
            }
        }
    }

    #[test]
    fn keywords_strings_and_comments_get_distinct_styles() {
        let pieces = highlight("fn main() { let s = \"hi\"; // note", Lang::Rust);
        let styled = |want: Style| pieces.iter().any(|(t, s)| *s == want && !t.is_empty());
        assert!(styled(keyword_style()), "keywords: {pieces:?}");
        assert!(styled(string_style()), "strings: {pieces:?}");
        assert!(styled(comment_style()), "comments: {pieces:?}");
    }

    #[test]
    fn the_comment_swallows_the_rest_of_the_line() {
        // A keyword or quote after the comment marker must not be styled as code.
        let pieces = highlight("// fn \"not a string\"", Lang::Rust);
        assert!(
            !pieces.iter().any(|(_, s)| *s == keyword_style()),
            "no keywords inside a comment: {pieces:?}"
        );
        assert!(
            !pieces.iter().any(|(_, s)| *s == string_style()),
            "no strings inside a comment: {pieces:?}"
        );
    }

    #[test]
    fn python_uses_hash_comments_and_rust_does_not() {
        let py = highlight("# note", Lang::Python);
        assert!(py.iter().any(|(_, s)| *s == comment_style()), "{py:?}");
        // `#` is not a comment in Rust (it introduces an attribute).
        let rs = highlight("#[derive(Debug)]", Lang::Rust);
        assert!(
            !rs.iter().any(|(_, s)| *s == comment_style()),
            "`#[` must not be treated as a comment: {rs:?}"
        );
    }

    #[test]
    fn an_unterminated_string_still_renders() {
        let pieces = highlight("let s = \"cut off here", Lang::Rust);
        assert!(pieces.iter().any(|(_, s)| *s == string_style()));
        round_trip("let s = \"cut off here", Lang::Rust);
    }

    #[test]
    fn numbers_inside_identifiers_are_not_numbers() {
        let pieces = highlight("foo2bar", Lang::Rust);
        assert!(
            !pieces.iter().any(|(_, s)| *s == number_style()),
            "a digit inside an identifier is not a numeric literal: {pieces:?}"
        );
        let pieces = highlight("let n = 42;", Lang::Rust);
        assert!(pieces.iter().any(|(_, s)| *s == number_style()));
    }

    #[test]
    fn a_read_gutter_is_separated_from_the_code() {
        assert_eq!(
            split_gutter("     1    pub fn answer()"),
            Some(("     1    ", "pub fn answer()"))
        );
        // No gutter: a plain code line stays whole.
        assert_eq!(split_gutter("pub fn answer()"), None);
        // A number that is part of the code is not a gutter.
        assert_eq!(split_gutter("42"), None);
        // Digits followed immediately by code are not a gutter either.
        assert_eq!(split_gutter("1foo"), None);
    }

    #[test]
    fn paths_infer_families() {
        assert_eq!(Lang::from_path("src/lib.rs"), Lang::Rust);
        assert_eq!(Lang::from_path("a/b/tool.py"), Lang::Python);
        assert_eq!(Lang::from_path("x.tsx"), Lang::JavaScript);
        assert_eq!(Lang::from_path("run.sh"), Lang::Shell);
        assert_eq!(Lang::from_path("Cargo.toml"), Lang::Toml);
        assert_eq!(Lang::from_path("data.json"), Lang::Json);
        assert_eq!(Lang::from_path("README.md"), Lang::Plain);
        assert_eq!(Lang::from_path("Makefile"), Lang::Plain);
    }

    #[test]
    fn fence_tags_map_to_families() {
        assert_eq!(Lang::from_tag("rust"), Lang::Rust);
        assert_eq!(Lang::from_tag("RS"), Lang::Rust);
        assert_eq!(Lang::from_tag("python"), Lang::Python);
        assert_eq!(Lang::from_tag("ts"), Lang::JavaScript);
        assert_eq!(Lang::from_tag("sh"), Lang::Shell);
        assert_eq!(Lang::from_tag("json"), Lang::Json);
        assert_eq!(Lang::from_tag("toml"), Lang::Toml);
        assert_eq!(Lang::from_tag(""), Lang::Plain);
        assert_eq!(Lang::from_tag("brainfuck"), Lang::Plain);
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;

    /// `highlight` runs for every visible code row on every frame, so it must stay
    /// linear. It allocated a fresh `String` of the remaining line on every character
    /// to test for a comment prefix — quadratic, and enough to stall a large read.
    #[test]
    fn highlighting_a_long_line_stays_fast() {
        let line = "let x = 1; ".repeat(400); // ~4400 chars
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let pieces = highlight(&line, Lang::Rust);
            assert!(!pieces.is_empty());
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "highlighting 20 x 4.4k chars took {elapsed:?}; it must stay linear"
        );
    }
}

/// Highlighting must never lose or reorder a character: the transcript's copy and
/// selection logic reassembles pieces by position, so a dropped char would corrupt
/// pastes. Fences, tabs, wide glyphs and unterminated comments/strings are exactly
/// what shifts a char-indexed lexer off the rails.
#[cfg(test)]
mod highlight_invariant_tests {
    use super::*;

    #[test]
    fn pieces_concatenate_to_the_input_exactly() {
        let langs = [
            Lang::Rust,
            Lang::Python,
            Lang::Plain,
            Lang::from_path("a.js"),
            Lang::from_path("a.json"),
            Lang::from_path("a.md"),
        ];
        let lines = [
            "fn main() { /* block */ }",
            "let s = \"a\\\"b\"; // trailing",
            "x = 'c'  # comment",
            "# not a comment in rust",
            "// 日本語 comment",
            "let 日 = 1; // 日本",
            "```rust",
            "    indented // c",
            "a\tb // tab",
            "\"unterminated",
            "/* unterminated",
            "",
        ];
        for lang in langs {
            for line in lines {
                let pieces = highlight(line, lang);
                let joined: String = pieces.iter().map(|(t, _)| t.as_str()).collect();
                assert_eq!(
                    joined, line,
                    "a dropped or reordered char would corrupt copied text: lang={lang:?} line={line:?} pieces={pieces:?}"
                );
            }
        }
    }
}
