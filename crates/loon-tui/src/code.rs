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
    Yaml,
    Dockerfile,
    Sql,
    Diff,
    Go,
    Java,
    Kotlin,
    Swift,
    C,
    Cpp,
    CSharp,
    Css,
    Html,
    Markdown,
    Makefile,
    Ruby,
    Lua,
    Plain,
}

impl Lang {
    /// Map a fence tag (`rust`, `rs`, `py`, …) to a family. Unknown tags are `Plain`,
    /// which still gets comment and string colouring via the C-like default.
    pub fn from_tag(tag: &str) -> Lang {
        let tag = tag
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| matches!(c, '{' | '}' | '.'));
        match tag.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Lang::Rust,
            "python" | "py" | "python3" => Lang::Python,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "typescript" | "ts" | "tsx" => Lang::JavaScript,
            "bash" | "sh" | "shell" | "zsh" | "console" => Lang::Shell,
            "json" | "jsonc" => Lang::Json,
            "toml" => Lang::Toml,
            "yaml" | "yml" => Lang::Yaml,
            "dockerfile" | "docker" => Lang::Dockerfile,
            "sql" => Lang::Sql,
            "diff" | "patch" => Lang::Diff,
            "go" | "golang" => Lang::Go,
            "java" => Lang::Java,
            "kotlin" | "kt" | "kts" => Lang::Kotlin,
            "swift" => Lang::Swift,
            "c" => Lang::C,
            "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" => Lang::Cpp,
            "csharp" | "c#" | "cs" => Lang::CSharp,
            "css" | "scss" | "sass" | "less" => Lang::Css,
            "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => Lang::Html,
            "markdown" | "md" | "mdx" => Lang::Markdown,
            "makefile" | "make" | "mk" => Lang::Makefile,
            "ruby" | "rb" => Lang::Ruby,
            "lua" => Lang::Lua,
            _ => Lang::Plain,
        }
    }

    /// Infer the family from a file path's extension. Used for tool excerpts, which
    /// carry a path but no fence tag.
    pub fn from_path(path: &str) -> Lang {
        // A file named for its kind rather than its extension: `Makefile`, `Dockerfile`,
        // not `Makefile.txt`.
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let base_lower = base.to_ascii_lowercase();
        if base_lower == "makefile" || base_lower == "gnumakefile" {
            return Lang::Makefile;
        }
        if base_lower.starts_with("dockerfile") {
            return Lang::Dockerfile;
        }
        let ext = base
            .rsplit_once('.')
            .map(|(_, e)| e)
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => Lang::JavaScript,
            "sh" | "bash" | "zsh" | "fish" => Lang::Shell,
            "json" | "jsonc" => Lang::Json,
            "toml" => Lang::Toml,
            "yaml" | "yml" => Lang::Yaml,
            "go" => Lang::Go,
            "java" => Lang::Java,
            "kt" | "kts" => Lang::Kotlin,
            "swift" => Lang::Swift,
            "c" => Lang::C,
            "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" => Lang::Cpp,
            "cs" => Lang::CSharp,
            "css" | "scss" | "sass" | "less" => Lang::Css,
            "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => Lang::Html,
            "md" | "mdx" | "markdown" => Lang::Markdown,
            "rb" | "rake" => Lang::Ruby,
            "lua" => Lang::Lua,
            "mk" => Lang::Makefile,
            "sql" => Lang::Sql,
            "diff" | "patch" => Lang::Diff,
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
                "protected",
                "switch",
                "case",
                "break",
                "continue",
                "delete",
                "in",
                "of",
                "yield",
                "static",
                "readonly",
                "enum",
                "declare",
                "namespace",
                "satisfies",
                "keyof",
            ],
            Lang::Go => &[
                "func",
                "package",
                "import",
                "var",
                "const",
                "type",
                "struct",
                "interface",
                "map",
                "chan",
                "go",
                "defer",
                "select",
                "range",
                "return",
                "if",
                "else",
                "for",
                "switch",
                "case",
                "default",
                "break",
                "continue",
                "fallthrough",
                "goto",
                "nil",
                "true",
                "false",
                "make",
                "new",
                "len",
                "cap",
                "append",
                "copy",
                "delete",
                "panic",
                "recover",
                "any",
            ],
            Lang::Java => &[
                "class",
                "interface",
                "enum",
                "record",
                "extends",
                "implements",
                "public",
                "private",
                "protected",
                "static",
                "final",
                "abstract",
                "native",
                "synchronized",
                "volatile",
                "transient",
                "void",
                "int",
                "long",
                "short",
                "byte",
                "char",
                "float",
                "double",
                "boolean",
                "new",
                "return",
                "if",
                "else",
                "for",
                "while",
                "do",
                "switch",
                "case",
                "default",
                "break",
                "continue",
                "try",
                "catch",
                "finally",
                "throw",
                "throws",
                "this",
                "super",
                "null",
                "true",
                "false",
                "import",
                "package",
                "instanceof",
                "var",
            ],
            Lang::Kotlin => &[
                "fun",
                "val",
                "var",
                "class",
                "object",
                "interface",
                "data",
                "sealed",
                "enum",
                "companion",
                "init",
                "constructor",
                "override",
                "open",
                "abstract",
                "private",
                "protected",
                "internal",
                "public",
                "suspend",
                "inline",
                "when",
                "is",
                "in",
                "as",
                "if",
                "else",
                "for",
                "while",
                "do",
                "return",
                "try",
                "catch",
                "finally",
                "throw",
                "this",
                "super",
                "null",
                "true",
                "false",
                "import",
                "package",
                "lateinit",
                "by",
                "it",
            ],
            Lang::Swift => &[
                "func",
                "let",
                "var",
                "class",
                "struct",
                "enum",
                "protocol",
                "extension",
                "import",
                "init",
                "deinit",
                "self",
                "Self",
                "guard",
                "if",
                "else",
                "for",
                "while",
                "repeat",
                "switch",
                "case",
                "default",
                "return",
                "throw",
                "throws",
                "rethrows",
                "do",
                "catch",
                "try",
                "defer",
                "nil",
                "true",
                "false",
                "in",
                "as",
                "is",
                "where",
                "let",
                "mutating",
                "override",
                "final",
                "open",
                "public",
                "private",
                "internal",
                "fileprivate",
                "static",
                "lazy",
                "weak",
                "unowned",
                "async",
                "await",
                "some",
                "any",
            ],
            Lang::C => &[
                "int", "char", "float", "double", "void", "long", "short", "unsigned", "signed",
                "const", "static", "extern", "register", "volatile", "struct", "union", "enum",
                "typedef", "sizeof", "return", "if", "else", "for", "while", "do", "switch",
                "case", "default", "break", "continue", "goto", "NULL", "true", "false", "include",
                "define", "ifdef", "ifndef", "endif", "pragma",
            ],
            Lang::Cpp => &[
                "int",
                "char",
                "float",
                "double",
                "void",
                "bool",
                "long",
                "short",
                "unsigned",
                "signed",
                "const",
                "constexpr",
                "static",
                "extern",
                "volatile",
                "mutable",
                "struct",
                "class",
                "union",
                "enum",
                "typedef",
                "using",
                "namespace",
                "template",
                "typename",
                "sizeof",
                "return",
                "if",
                "else",
                "for",
                "while",
                "do",
                "switch",
                "case",
                "default",
                "break",
                "continue",
                "goto",
                "new",
                "delete",
                "this",
                "nullptr",
                "true",
                "false",
                "try",
                "catch",
                "throw",
                "public",
                "private",
                "protected",
                "virtual",
                "override",
                "final",
                "friend",
                "operator",
                "noexcept",
                "auto",
                "concept",
                "requires",
                "co_await",
                "co_return",
                "co_yield",
                "include",
                "define",
                "pragma",
            ],
            Lang::CSharp => &[
                "namespace",
                "using",
                "class",
                "struct",
                "interface",
                "enum",
                "record",
                "delegate",
                "event",
                "public",
                "private",
                "protected",
                "internal",
                "static",
                "readonly",
                "const",
                "virtual",
                "override",
                "abstract",
                "sealed",
                "partial",
                "async",
                "await",
                "var",
                "new",
                "this",
                "base",
                "return",
                "if",
                "else",
                "for",
                "foreach",
                "while",
                "do",
                "switch",
                "case",
                "default",
                "break",
                "continue",
                "try",
                "catch",
                "finally",
                "throw",
                "null",
                "true",
                "false",
                "string",
                "int",
                "bool",
                "double",
                "void",
                "object",
                "in",
                "is",
                "as",
                "typeof",
                "nameof",
                "get",
                "set",
                "init",
            ],
            Lang::Css => &[
                "important",
                "inherit",
                "initial",
                "unset",
                "revert",
                "auto",
                "none",
                "flex",
                "grid",
                "block",
                "inline",
                "absolute",
                "relative",
                "fixed",
                "sticky",
                "solid",
                "dashed",
                "bold",
                "italic",
                "hidden",
                "visible",
                "transparent",
                "currentColor",
                "var",
                "calc",
                "rgb",
                "rgba",
                "hsl",
                "url",
                "repeat",
                "cover",
                "contain",
                "nowrap",
            ],
            Lang::Html => &[
                "html", "head", "body", "div", "span", "a", "p", "ul", "ol", "li", "table", "tr",
                "td", "th", "form", "input", "button", "script", "style", "link", "meta", "title",
                "class", "id", "href", "src", "type", "name", "value", "rel", "alt",
            ],
            Lang::Markdown => &[],
            Lang::Makefile => &[
                "ifeq",
                "ifneq",
                "ifdef",
                "ifndef",
                "else",
                "endif",
                "include",
                "export",
                "unexport",
                "override",
                "define",
                "endef",
                "SHELL",
                "MAKEFLAGS",
                "CC",
                "CFLAGS",
                "CXX",
                "CXXFLAGS",
                "LDFLAGS",
                "PREFIX",
                "DESTDIR",
                "all",
                "clean",
                "install",
                "test",
                "build",
            ],
            Lang::Ruby => &[
                "def",
                "end",
                "class",
                "module",
                "if",
                "elsif",
                "else",
                "unless",
                "while",
                "until",
                "for",
                "do",
                "begin",
                "rescue",
                "ensure",
                "raise",
                "return",
                "yield",
                "block_given?",
                "nil",
                "true",
                "false",
                "self",
                "super",
                "module_function",
                "require",
                "require_relative",
                "attr_accessor",
                "attr_reader",
                "attr_writer",
                "include",
                "extend",
                "lambda",
                "proc",
                "then",
                "case",
                "when",
                "and",
                "or",
                "not",
                "next",
                "break",
                "retry",
                "ensure",
                "new",
                "puts",
                "freeze",
            ],
            Lang::Lua => &[
                "function",
                "end",
                "local",
                "if",
                "then",
                "else",
                "elseif",
                "for",
                "while",
                "do",
                "repeat",
                "until",
                "return",
                "break",
                "goto",
                "in",
                "and",
                "or",
                "not",
                "nil",
                "true",
                "false",
                "require",
                "print",
                "pairs",
                "ipairs",
                "type",
                "tostring",
                "tonumber",
                "setmetatable",
                "self",
            ],
            Lang::Yaml => &["true", "false", "null", "yes", "no", "on", "off"],
            Lang::Dockerfile => &[
                "FROM",
                "RUN",
                "CMD",
                "LABEL",
                "EXPOSE",
                "ENV",
                "ADD",
                "COPY",
                "ENTRYPOINT",
                "VOLUME",
                "USER",
                "WORKDIR",
                "ARG",
                "ONBUILD",
                "STOPSIGNAL",
                "HEALTHCHECK",
                "SHELL",
                "AS",
            ],
            Lang::Shell => &[
                "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
                "function", "return", "export", "local", "set", "unset", "source", "cd", "echo",
            ],
            Lang::Sql => &[
                "select",
                "from",
                "where",
                "insert",
                "into",
                "update",
                "delete",
                "create",
                "table",
                "join",
                "left",
                "right",
                "on",
                "as",
                "and",
                "or",
                "not",
                "null",
                "values",
                "set",
                "order",
                "by",
                "group",
                "limit",
                "having",
                "drop",
                "alter",
                "primary",
                "key",
                "references",
            ],
            Lang::Json | Lang::Toml | Lang::Diff | Lang::Plain => &["true", "false", "null"],
        }
    }

    /// The comment prefix, if the family has one. Several languages have more than one
    /// marker; this is the primary one the lexer scans for.
    fn comment(self) -> Option<&'static str> {
        match self {
            Lang::Rust
            | Lang::JavaScript
            | Lang::Go
            | Lang::Java
            | Lang::Kotlin
            | Lang::Swift
            | Lang::C
            | Lang::Cpp
            | Lang::CSharp => Some("//"),
            Lang::Python | Lang::Shell | Lang::Toml | Lang::Yaml | Lang::Makefile | Lang::Ruby => {
                Some("#")
            }
            Lang::Dockerfile => Some("#"),
            Lang::Sql | Lang::Lua => Some("--"),
            // Markdown's `#` is a heading, not a comment, and `<!-- -->` is not line-based;
            // treating it as a comment marker would swallow every heading.
            Lang::Json | Lang::Diff | Lang::Plain | Lang::Css | Lang::Html | Lang::Markdown => None,
        }
    }

    /// String delimiters for the family.
    fn quotes(self) -> &'static [char] {
        match self {
            Lang::Rust => &['"'],
            Lang::Python | Lang::Ruby => &['"', '\''],
            Lang::JavaScript | Lang::Json | Lang::Go => &['"', '\'', '`'],
            Lang::Shell => &['"', '\''],
            Lang::Toml | Lang::Yaml | Lang::Sql => &['"', '\''],
            Lang::Kotlin => &['"', '\''],
            Lang::Lua => &['"', '\''],
            Lang::Html => &['"', '\''],
            Lang::C | Lang::Cpp | Lang::CSharp | Lang::Java => &['"', '\''],
            Lang::Swift => &['"'],
            Lang::Css => &['"', '\''],
            Lang::Dockerfile | Lang::Makefile => &['"', '\''],
            Lang::Markdown | Lang::Diff | Lang::Plain => &['"', '\''],
        }
    }

    /// True when `#`-prefixed text is an attribute or preprocessor directive rather than a
    /// comment: Rust (`#[derive]`), C/C++ (`#include`, `#pragma`). Python, Ruby, Shell and
    /// the config formats use `#` for comments, so treating it as an attribute there would
    /// colour every comment as an attribute.
    fn attribute_hash(self) -> bool {
        matches!(self, Lang::Rust | Lang::C | Lang::Cpp)
    }

    /// True when `@name` is a decorator or annotation: Python, Java, Kotlin, C#, Swift and
    /// TypeScript. Not Rust, where `@` is a pattern binding, not Lua, where it is a length
    /// operator, and not Shell, where it is part of a variable expansion.
    fn attribute_at(self) -> bool {
        matches!(
            self,
            Lang::Python
                | Lang::JavaScript
                | Lang::Java
                | Lang::Kotlin
                | Lang::CSharp
                | Lang::Swift
        )
    }
}

/// Colours for one highlighted line. Kept as free functions so the transcript can use
/// them without threading a theme through the lexer.
///
/// These are ANSI *slots*, not final colours: `ui::normalize_line` translates each one onto
/// the active palette at render time (Magenta/Cyan→accent, Green→ok, Blue→function colour,
/// Yellow→warn, DarkGray→dim). That indirection is what makes highlighting follow the
/// palette, so a new class must reuse an existing slot rather than invent an RGB value.
pub fn keyword_style() -> Style {
    Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD)
}
pub fn string_style() -> Style {
    Style::default().fg(Color::Green)
}
pub fn comment_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}
pub fn number_style() -> Style {
    Style::default().fg(Color::Yellow)
}
pub fn plain_style() -> Style {
    Style::default().fg(Color::Reset)
}
/// A function or method name — the identifier before `(`. `Blue` translates to the palette's
/// dedicated function colour, so a call site reads apart from a keyword.
pub fn func_style() -> Style {
    Style::default().fg(Color::Blue)
}
/// A type, trait or constant: accent + italic, distinct from a keyword (accent + bold)
/// without spending another colour slot.
pub fn type_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::ITALIC)
}
/// `#[derive(...)]`, `@decorator`, `#include`: warn + bold.
pub fn attribute_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}
/// Operators and punctuation: dim, so structure is legible without competing with keywords.
pub fn punctuation_style() -> Style {
    Style::default().fg(Color::DarkGray)
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
    if lang == Lang::Diff {
        let style = if line.starts_with("@@") || line.starts_with("diff ") {
            keyword_style()
        } else if line.starts_with('+') {
            string_style()
        } else if line.starts_with('-') {
            Style::default().fg(Color::Red)
        } else {
            plain_style()
        };
        return vec![(line.into(), style)];
    }
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
            // A leading-dot float (`.5`) and underscores in literals (`1_000`) are part of
            // the number; both are common and would otherwise split the token.
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                text.push(chars[i]);
                i += 1;
            }
            out.push((text, number_style()));
            continue;
        }

        // Attributes and decorators: `#[derive(..)]`, `@Component`, `#include <x.h>`.
        // Checked before identifiers so the marker is coloured with its name, and before
        // punctuation so `#` does not become a lone dim glyph.
        if chars[i] == '#' && i + 1 < chars.len() && (chars[i + 1] == '[' || lang.attribute_hash())
        {
            flush!();
            let mut text = String::new();
            text.push('#');
            i += 1;
            while i < chars.len() && chars[i] != ']' && chars[i] != '\n' {
                text.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                text.push(']');
                i += 1;
            }
            out.push((text, attribute_style()));
            continue;
        }
        if chars[i] == '@' && lang.attribute_at() {
            flush!();
            let mut text = String::new();
            text.push('@');
            i += 1;
            while i < chars.len() && (is_word(chars[i]) || chars[i] == '.' || chars[i] == ':') {
                text.push(chars[i]);
                i += 1;
            }
            out.push((text, attribute_style()));
            continue;
        }

        // Identifiers: keywords, types and function names each get their own style so a
        // line of code is readable rather than keywords-on-a-slab-of-plain.
        if is_word(chars[i]) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let is_keyword = lang.keywords().contains(&word.as_str())
                || (lang == Lang::Sql
                    && lang
                        .keywords()
                        .contains(&word.to_ascii_lowercase().as_str()));
            if is_keyword {
                flush!();
                out.push((word, keyword_style()));
            } else if looks_like_type(&word) {
                // UpperCamelCase and SCREAMING_CASE read as a type, trait or constant.
                flush!();
                out.push((word, type_style()));
            } else if next_nonspace_is_open_paren(&chars, i) {
                // A call site: the name before `(`, which is what makes code scannable.
                flush!();
                out.push((word, func_style()));
            } else {
                buf.push_str(&word);
            }
            continue;
        }

        // Operators and punctuation in a dim colour: braces, arrows and `::` show structure
        // without shouting over the keywords.
        if is_punctuation(chars[i]) && !chars[i].is_whitespace() {
            flush!();
            let start = i;
            while i < chars.len() && is_punctuation(chars[i]) && !chars[i].is_whitespace() {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            out.push((text, punctuation_style()));
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }
    flush!();
    coalesce(out)
}

/// True when the identifier is conventionally a type, trait or constant: it starts with an
/// uppercase letter, or is entirely uppercase with underscores (`MAX_BYTES`). A single
/// uppercase letter is left alone because `T` in a generic is indistinguishable from a
/// one-letter variable and colouring it as a type is more confusing than helpful.
fn looks_like_type(word: &str) -> bool {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    if word.chars().count() == 1 {
        return false;
    }
    true
}

/// True when the next non-space character after `from` is `(` — i.e. this identifier is
/// being called. Bounded by the line, so the scan stays linear.
fn next_nonspace_is_open_paren(chars: &[char], from: usize) -> bool {
    let mut j = from;
    while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
        j += 1;
    }
    chars.get(j) == Some(&'(')
}

/// Characters rendered in the dim punctuation colour. Whitespace is excluded so runs of
/// spaces stay plain and a leading indent never picks up a colour.
fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        '{' | '}'
            | '('
            | ')'
            | '['
            | ']'
            | '<'
            | '>'
            | ','
            | ';'
            | ':'
            | '='
            | '+'
            | '-'
            | '*'
            | '/'
            | '%'
            | '!'
            | '&'
            | '|'
            | '^'
            | '~'
            | '?'
            | '.'
            | '@'
            | '#'
    )
}

pub fn slice(pieces: &[Piece], start: usize, len: usize) -> Vec<Piece> {
    let mut offset = 0;
    let mut out = Vec::new();
    for (text, style) in pieces {
        let count = text.chars().count();
        let lo = start.saturating_sub(offset).min(count);
        let hi = (start + len).saturating_sub(offset).min(count);
        if hi > lo {
            out.push((text.chars().skip(lo).take(hi - lo).collect(), *style));
        }
        offset += count;
    }
    out
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
        // A language with its own rules gets its own family; these used to alias to
        // JavaScript or Toml, so a Go file was coloured by JavaScript rules.
        assert_eq!(Lang::from_path("main.go"), Lang::Go);
        assert_eq!(Lang::from_path("Main.java"), Lang::Java);
        assert_eq!(Lang::from_path("App.kt"), Lang::Kotlin);
        assert_eq!(Lang::from_path("View.swift"), Lang::Swift);
        assert_eq!(Lang::from_path("a.c"), Lang::C);
        assert_eq!(Lang::from_path("a.cpp"), Lang::Cpp);
        assert_eq!(Lang::from_path("a.hpp"), Lang::Cpp);
        assert_eq!(Lang::from_path("Program.cs"), Lang::CSharp);
        assert_eq!(Lang::from_path("style.css"), Lang::Css);
        assert_eq!(Lang::from_path("chart.yaml"), Lang::Yaml);
        assert_eq!(Lang::from_path("deploy.yml"), Lang::Yaml);
        assert_eq!(Lang::from_path("index.html"), Lang::Html);
        assert_eq!(Lang::from_path("README.md"), Lang::Markdown);
        assert_eq!(Lang::from_path("script.rb"), Lang::Ruby);
        assert_eq!(Lang::from_path("init.lua"), Lang::Lua);
        assert_eq!(Lang::from_path("unknown.xyz"), Lang::Plain);
    }

    /// A file named for its kind rather than an extension. `Makefile` is a Makefile, and
    /// `Dockerfile.prod` is a Dockerfile — not a plain-text file.
    #[test]
    fn extensionless_filenames_are_recognised() {
        assert_eq!(Lang::from_path("Makefile"), Lang::Makefile);
        assert_eq!(Lang::from_path("GNUmakefile"), Lang::Makefile);
        assert_eq!(Lang::from_path("build/Makefile"), Lang::Makefile);
        assert_eq!(Lang::from_path("Dockerfile"), Lang::Dockerfile);
        assert_eq!(Lang::from_path("docker/Dockerfile.prod"), Lang::Dockerfile);
        assert_eq!(Lang::from_path("src/main.rs"), Lang::Rust);
    }

    /// A comment marker must match the language: Go and Java are `//`, Ruby and YAML are
    /// `#`, and Markdown's `#` is a heading, so Markdown has no comment marker at all.
    #[test]
    fn comment_markers_match_the_language() {
        assert_eq!(Lang::Go.comment(), Some("//"));
        assert_eq!(Lang::Java.comment(), Some("//"));
        assert_eq!(Lang::Ruby.comment(), Some("#"));
        assert_eq!(Lang::Yaml.comment(), Some("#"));
        assert_eq!(Lang::Lua.comment(), Some("--"));
        assert_eq!(
            Lang::Markdown.comment(),
            None,
            "a heading would be swallowed as a comment"
        );
    }

    /// The whole point of splitting the aliases: a Go keyword set must actually differ from
    /// JavaScript's, or nothing was gained.
    #[test]
    fn split_languages_have_their_own_keywords() {
        // `func` and `package` are Go, not JavaScript.
        let js = highlight("func main()", Lang::JavaScript);
        assert!(
            !js.iter().any(|(t, s)| t == "func" && *s == keyword_style()),
            "JavaScript must not claim Go's `func`: {js:?}"
        );
        let go = highlight("func main()", Lang::Go);
        assert!(
            go.iter().any(|(t, s)| t == "func" && *s == keyword_style()),
            "Go must colour its own `func`: {go:?}"
        );
        // A Go comment is `//`; a Java one is too, and YAML's `#` is not.
        let yaml = highlight("key: # not code", Lang::Yaml);
        assert!(
            yaml.iter().any(|(_, s)| *s == comment_style()),
            "a YAML comment is a comment: {yaml:?}"
        );
        // Dockerfile keywords are uppercase instructions.
        let docker = highlight("FROM rust:1.75", Lang::Dockerfile);
        assert!(
            docker
                .iter()
                .any(|(t, s)| t == "FROM" && *s == keyword_style()),
            "Dockerfile instructions are keywords: {docker:?}"
        );
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

/// The new token classes must stay distinguishable. Two things could silently break that:
/// a new class reusing a colour another class already has *with the same weight*, and a
/// colour that `ui::normalize_line` does not translate (which would pass through raw and be
/// remapped by some terminals).
#[cfg(test)]
mod token_class_tests {
    use super::*;

    /// Every class the lexer can emit, so a new one cannot be added without being checked.
    fn classes() -> Vec<(&'static str, Style)> {
        vec![
            ("keyword", keyword_style()),
            ("func", func_style()),
            ("type", type_style()),
            ("string", string_style()),
            ("number", number_style()),
            ("attribute", attribute_style()),
            ("punctuation", punctuation_style()),
            ("comment", comment_style()),
            ("plain", plain_style()),
        ]
    }

    /// Only these ANSI slots may be emitted. Anything else reaches `normalize_line`'s
    /// fall-through arm and renders as a raw 8-bit colour that some terminals remap.
    #[test]
    fn every_class_uses_a_translated_colour_slot() {
        for (name, style) in classes() {
            let fg = style.fg.expect("a class must set a foreground");
            assert!(
                matches!(
                    fg,
                    Color::Reset
                        | Color::Cyan
                        | Color::Magenta
                        | Color::Green
                        | Color::Red
                        | Color::Yellow
                        | Color::DarkGray
                        | Color::Blue
                ),
                "{name} uses {fg:?}, which ui::normalize_line does not translate"
            );
        }
    }

    /// No two classes may look identical: a shared colour is only acceptable when the
    /// weight or slant differs, or the two would be indistinguishable on screen.
    #[test]
    fn no_two_classes_render_identically() {
        let all = classes();
        for (i, (an, a)) in all.iter().enumerate() {
            for (bn, b) in all.iter().skip(i + 1) {
                assert!(
                    a != b,
                    "{an} and {bn} have identical styles, so they cannot be told apart"
                );
            }
        }
    }

    /// A function name is distinguished by colour, not only by weight (that is the whole
    /// point of adding the palette slot).
    #[test]
    fn a_function_name_differs_from_a_keyword_by_colour() {
        let k = keyword_style();
        let f = func_style();
        assert_ne!(k.fg, f.fg, "func must not reuse the keyword colour");
        assert!(f.fg != keyword_style().fg && f.fg != string_style().fg);
    }

    /// `#[derive(..)]` and `@decorator` are attributes; a plain `# comment` in Python or
    /// Shell is not, or every comment in those languages would colour as an attribute.
    #[test]
    fn attributes_are_not_confused_with_comments() {
        let rust = highlight("#[derive(Debug)]", Lang::Rust);
        assert!(
            rust.iter().any(|(_, s)| *s == attribute_style()),
            "a Rust attribute must be styled as one: {rust:?}"
        );
        let py = highlight("# a comment", Lang::Python);
        assert!(
            py.iter().all(|(_, s)| *s != attribute_style()),
            "a Python comment must not be an attribute: {py:?}"
        );
        assert!(py.iter().any(|(_, s)| *s == comment_style()));
        let at = highlight("@app.route(\"/\")", Lang::Python);
        assert!(
            at.iter().any(|(_, s)| *s == attribute_style()),
            "a Python decorator must be styled as one: {at:?}"
        );
    }

    /// The identifier before `(` is a call, so it gets the function colour; a bare word does
    /// not, or every variable would look like a function.
    #[test]
    fn call_sites_are_functions_but_bare_names_are_not() {
        let call = highlight("parse(input)", Lang::Rust);
        assert!(
            call.iter().any(|(t, s)| t == "parse" && *s == func_style()),
            "{call:?}"
        );
        let bare = highlight("let total = 1;", Lang::Rust);
        assert!(
            bare.iter().all(|(_, s)| *s != func_style()),
            "a plain identifier must not be a function: {bare:?}"
        );
    }

    /// UpperCamelCase and SCREAMING_CASE read as types/constants; lowercase names do not.
    #[test]
    fn types_and_constants_are_recognised() {
        for line in ["let x: Result<()>", "const MAX_BYTES = 1;"] {
            let pieces = highlight(line, Lang::Rust);
            assert!(
                pieces.iter().any(|(_, s)| *s == type_style()),
                "{line:?} should carry a type-styled word: {pieces:?}"
            );
        }
        let lower = highlight("let total = 1;", Lang::Rust);
        assert!(lower.iter().all(|(_, s)| *s != type_style()), "{lower:?}");
        // A single uppercase letter is a generic parameter, not a type worth colouring.
        let single = highlight("fn f<T>(x: T)", Lang::Rust);
        assert!(
            !single.iter().any(|(t, s)| t == "T" && *s == type_style()),
            "one-letter generics are left plain: {single:?}"
        );
    }

    /// Punctuation is dimmed, but a run of spaces is not punctuation — an indent must not
    /// pick up a colour.
    #[test]
    fn indentation_is_not_punctuation() {
        let pieces = highlight("    let x = 1;", Lang::Rust);
        let leading = pieces.first().expect("pieces");
        assert!(
            !leading.0.starts_with(' ') || leading.1 != punctuation_style(),
            "the indent must not be styled as punctuation: {pieces:?}"
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
