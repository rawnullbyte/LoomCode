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

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();

        // A comment runs to end of line.
        if let Some(marker) = lang.comment() {
            if rest.starts_with(marker) {
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
