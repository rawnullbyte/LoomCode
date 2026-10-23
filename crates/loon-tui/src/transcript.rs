//! Rendering and copying share one source map. Soft wraps never become copied newlines.
use crate::{theme, App, Block};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;
#[derive(Debug, Clone)]
pub struct TranscriptRow {
    pub line: Line<'static>,
    pub raw: String,
    pub prefix_cells: usize,
    pub soft: bool,
}
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                }
                // String sequences: DCS `P`, SOS `X`, PM `^`, APC `_`. Their payload
                // is data (a sixel image, a terminal query), not text — dumping it
                // painted control payload into the transcript as if the command had
                // printed it. Terminated by ST (`ESC \`) or, in practice, BEL.
                Some('P' | 'X' | '^' | '_') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    out
}
fn cells(c: char, column: usize) -> usize {
    if c == '\t' {
        4 - column % 4
    } else {
        c.width().unwrap_or(0)
    }
}
fn display(raw: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for c in raw.chars() {
        if c == '\t' {
            let n = cells(c, col);
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            out.push(c);
            col += cells(c, col);
        }
    }
    out
}
/// Split at display-cell boundaries and retain every source character (including spaces).
pub fn wrap(raw: &str, width: usize) -> Vec<String> {
    let mut rows = vec![];
    let width = width.max(1);
    for para in raw.split('\n') {
        let mut remaining = para;
        loop {
            let mut used = 0;
            let mut cut = remaining.len();
            let mut boundary = None;
            let mut nonspace = false;
            for (i, c) in remaining.char_indices() {
                let n = cells(c, used);
                if used + n > width && i > 0 {
                    cut = boundary.unwrap_or(i);
                    break;
                }
                used += n;
                if !c.is_whitespace() {
                    nonspace = true;
                }
                if c.is_whitespace() && nonspace {
                    boundary = Some(i + c.len_utf8());
                }
            }
            rows.push(remaining[..cut].to_string());
            if cut == remaining.len() {
                break;
            }
            remaining = &remaining[cut..];
        }
    }
    rows
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    Tool,
    Note,
    Rich,
}

fn kind(block: &Block) -> Kind {
    match block {
        Block::User(_) => Kind::User,
        Block::Assistant(_) => Kind::Assistant,
        Block::Tool { .. } => Kind::Tool,
        Block::System(_) | Block::Member(_) => Kind::Note,
        Block::Rich(_) => Kind::Rich,
        Block::Banner => Kind::Rich,
    }
}

/// Consecutive tool calls and consecutive notes form one visual group: no blank
/// row inside them (matching the [CC] transcript).
fn grouped(prev: Kind, next: Kind) -> bool {
    matches!(
        (prev, next),
        (Kind::Tool, Kind::Tool) | (Kind::Note, Kind::Note)
    )
}

fn blank_row() -> TranscriptRow {
    TranscriptRow {
        line: Line::default(),
        raw: String::new(),
        prefix_cells: 0,
        soft: false,
    }
}

fn empty_line(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.is_empty())
}

fn cell_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn take_cells(text: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if n + w > cells {
            break;
        }
        out.push(c);
        n += w;
    }
    out
}

/// One text block: `marker` on the first rendered line, a same-width indent on
/// every continuation (wrapped or hard-newline) line.
fn push_text(rows: &mut Vec<TranscriptRow>, marker: &str, text: &str, style: Style, width: usize) {
    let prefix_cells = cell_width(marker).min(width.saturating_sub(1));
    let marker = take_cells(marker, prefix_cells);
    let indent = " ".repeat(prefix_cells);
    let avail = width.saturating_sub(prefix_cells).max(1);
    let text = clean(text);
    let mut first = true;
    let markdown = style == theme::assistant_fg();
    let mut code = false;
    // The fence's info string picks the highlighting family; it is remembered until
    // the closing fence so every line of the block is tokenized the same way.
    let mut lang = crate::code::Lang::Plain;
    for para in text.split('\n') {
        let trimmed = para.trim_start();
        let fence = markdown && (trimmed.starts_with("```") || trimmed.starts_with("~~~"));
        if fence {
            lang = if code {
                crate::code::Lang::Plain
            } else {
                crate::code::Lang::from_tag(trimmed.trim_start_matches(['`', '~']))
            };
        }
        let body_style = if markdown && (fence || code) {
            theme::tool_ok_fg()
        } else if markdown && trimmed.starts_with('#') {
            theme::accent_fg().add_modifier(Modifier::BOLD)
        } else {
            style
        };
        let highlighted = (code && !fence).then(|| crate::code::highlight(para, lang));
        let mut code_offset = 0;
        for (i, raw) in wrap(para, avail).into_iter().enumerate() {
            let lead = if first {
                marker.clone()
            } else {
                indent.clone()
            };
            first = false;
            rows.push(TranscriptRow {
                line: Line::from({
                    let mut spans = vec![Span::styled(lead, style)];
                    let text = display(&raw);
                    // A diff line is unambiguous whatever the surrounding prose is:
                    // colour it before any inline-code splitting can hide the prefix.
                    let diff = if code || fence {
                        None
                    } else {
                        diff_style(&text)
                    };
                    if let Some(diff) = diff {
                        spans.push(Span::styled(text, diff));
                    } else if markdown && !code && !fence {
                        let mut inline = false;
                        for (i, part) in text.split('`').enumerate() {
                            if i > 0 {
                                spans.push(Span::styled("`", theme::accent_fg()));
                                inline = !inline;
                            }
                            spans.push(Span::styled(
                                part.to_string(),
                                if inline {
                                    theme::accent_fg()
                                } else {
                                    body_style
                                },
                            ));
                        }
                    } else if code && !fence {
                        // Real syntax highlighting inside a fenced block: keywords,
                        // strings, numbers and comments are separated by language, so
                        // a Rust block does not read like a wall of one colour.
                        let mut column = 0;
                        for (piece, style) in crate::code::slice(
                            highlighted.as_ref().unwrap(),
                            code_offset,
                            raw.chars().count(),
                        ) {
                            let mut shown = String::new();
                            for ch in piece.chars() {
                                let n = cells(ch, column);
                                if ch == '\t' {
                                    shown.push_str(&" ".repeat(n));
                                } else {
                                    shown.push(ch);
                                }
                                column += n;
                            }
                            spans.push(Span::styled(shown, style));
                        }
                    } else {
                        // Diff lines inside prose keep their own colour: a pasted patch
                        // in an answer reads like a patch.
                        let line_style = diff_style(&text).unwrap_or(body_style);
                        spans.push(Span::styled(text, line_style));
                    }
                    spans
                }),
                raw,
                prefix_cells,
                soft: i > 0,
            });
            code_offset += rows.last().unwrap().raw.chars().count();
        }
        if fence {
            code = !code;
        }
    }
}

/// Style for one visual line of a diff, or `None` when it is not a diff line.
fn diff_style(line: &str) -> Option<Style> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("--- ") || trimmed.starts_with("+++ ") {
        Some(theme::accent_fg().add_modifier(Modifier::BOLD))
    } else if trimmed.starts_with("@@") && trimmed.contains("@@") {
        Some(theme::member_fg())
    } else if trimmed.starts_with('+') && !trimmed.starts_with("+++") {
        Some(theme::tool_ok_fg())
    } else if trimmed.starts_with('-') && !trimmed.starts_with("---") {
        Some(theme::tool_err_fg())
    } else {
        None
    }
}

/// One excerpt of a *finished* tool call: a bounded diff or code sample, coloured
/// line by line. Diff lines get red/green/headers, fenced code inside excerpts keeps
/// its green, and plain output stays in the caller's style.
fn push_colored_excerpt(
    rows: &mut Vec<TranscriptRow>,
    marker: &str,
    excerpt: &str,
    style: Style,
    width: usize,
) {
    push_tool_excerpt(
        rows,
        marker,
        excerpt,
        style,
        width,
        crate::code::Lang::Plain,
    )
}

/// An excerpt that may be source code: `lang` comes from the tool's path argument, so a
/// `read` of `lib.rs` renders with Rust highlighting instead of flat grey.
fn push_tool_excerpt(
    rows: &mut Vec<TranscriptRow>,
    marker: &str,
    excerpt: &str,
    style: Style,
    width: usize,
    lang: crate::code::Lang,
) {
    let prefix_cells = cell_width(marker).min(width.saturating_sub(1));
    let marker = take_cells(marker, prefix_cells);
    let indent = " ".repeat(prefix_cells);
    let avail = width.saturating_sub(prefix_cells).max(1);
    let lines = clean(excerpt);
    let mut first = true;
    let mut in_code = false;
    // Stored excerpts join their lines with `⏎` (see `tool_finished`), so both that
    // separator and a real newline end a line. Splitting only on newlines would turn a
    // whole diff into one wrapped paragraph and lose its per-line colouring.
    for para in lines.split('\n').flat_map(|l| l.split(" ⏎ ")) {
        let trimmed = para.trim_start();
        let fence = trimmed.starts_with("```");
        if fence {
            in_code = !in_code;
        }
        // Diff state is per-paragraph: a blank line between hunks must not carry
        // the addition colour onto following prose.
        // Fences and their contents are code; everything else may be a diff line.
        let body_style = if fence || in_code {
            theme::tool_ok_fg()
        } else {
            diff_style(para).unwrap_or(style)
        };
        // A source file read is code even without a fence: the tool's path says what
        // language it is. Only plain (non-diff, non-fenced) lines are highlighted, so a
        // diff hunk keeps its red/green rather than being tokenized as source.
        let as_code = lang != crate::code::Lang::Plain
            && !fence
            && !in_code
            && diff_style(para).is_none()
            && !metadata_line(trimmed);
        // Lex the *whole* logical line once, then cut the styled pieces at the same
        // display columns `wrap` uses. Tokenizing each visual row separately loses the
        // state that crosses a break: the tail of a long comment was re-lexed as code,
        // and a string split across rows lost its colour on the continuation.
        // The line-number gutter is metadata: highlight only the code after it, so a
        // file of prose does not read as a column of yellow numbers.
        let shown = display(para);
        let (gutter, code_part) = match crate::code::split_gutter(&shown) {
            Some((g, c)) => (g.to_string(), c.to_string()),
            None => (String::new(), shown.clone()),
        };
        let styled = as_code.then(|| {
            let mut pieces: Vec<crate::code::Piece> = Vec::new();
            if !gutter.is_empty() {
                pieces.push((gutter.clone(), body_style));
            }
            pieces.extend(crate::code::highlight(&code_part, lang));
            pieces
        });
        let mut pieces = styled.into_iter().flatten().peekable();
        // Wrap the *expanded* line, so the rows and the highlighted pieces are cut from
        // the same text. Wrapping `para` while lexing `display(para)` made a tab or a
        // wide character shift the boundaries and the styling drifted off its text.
        for (i, raw) in wrap(&shown, avail).into_iter().enumerate() {
            let lead = if first {
                marker.clone()
            } else {
                indent.clone()
            };
            first = false;
            let mut spans = vec![Span::styled(lead, style)];
            let text = display(&raw);
            match pieces.peek().is_some() {
                // Split on display cells, not chars: `wrap` cuts rows by cells, and a
                // width-2 character makes the two counts disagree. Count what the row
                // actually shows with `cell_width`, so styling never drifts right of
                // its text when CJK or emoji is present.
                true => {
                    let mut want = cell_width(&text);
                    while want > 0 {
                        let Some((piece, piece_style)) = pieces.peek_mut() else {
                            break;
                        };
                        let mut take = 0usize;
                        let mut taken = 0usize;
                        for c in piece.chars() {
                            let w = cells(c, 0);
                            if taken + w > want {
                                break;
                            }
                            taken += w;
                            take += 1;
                        }
                        if take == 0 {
                            pieces.next();
                            continue;
                        }
                        let head: String = piece.chars().take(take).collect();
                        let tail: String = piece.chars().skip(take).collect();
                        spans.push(Span::styled(head, *piece_style));
                        want = want.saturating_sub(taken);
                        if tail.is_empty() {
                            pieces.next();
                        } else {
                            *piece = tail;
                        }
                    }
                }
                false => spans.push(Span::styled(text, body_style)),
            }
            rows.push(TranscriptRow {
                line: Line::from(spans),
                raw,
                prefix_cells,
                soft: i > 0,
            });
        }
    }
}

/// One tool call as the transcript sees it.
struct ToolCall {
    name: String,
    ok: bool,
    pending: bool,
    excerpt: String,
    detail: String,
}

fn section_marker(line: &str) -> bool {
    line.starts_with("---") && line.ends_with("---") && line.len() > 6
}

fn metadata_line(line: &str) -> bool {
    line.starts_with("[exit ")
        || line.starts_with("[truncated:")
        || (line.starts_with("[sha256 ") && line.contains(" | "))
}

/// Logical output lines of a tool result: the `⏎` join and section headers from
/// the raw payload are both treated as separators, never rendered.
fn tool_lines(excerpt: &str) -> Vec<String> {
    clean(excerpt)
        .split('\n')
        .flat_map(|l| l.split(" ⏎ "))
        .map(|l| l.trim().replace('\t', " "))
        .filter(|l| !l.is_empty() && !section_marker(l))
        .collect()
}

/// `lines 1–2000 of 240` trailers carry the real file length; prefer it.
fn total_lines(lines: &[String]) -> Option<usize> {
    lines.iter().find_map(|l| {
        if !l.contains("lines ") {
            return None;
        }
        let tail = l.rsplit_once(" of ")?.1;
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    })
}

/// `+12 −3` when the excerpt is a diff, so a collapsed edit still says how big it was.
/// Counting is done on the raw excerpt (before the `⏎` join is split) so the numbers
/// match what the tool actually reported.
fn diff_stat(excerpt: &str) -> Option<String> {
    let lines: Vec<String> = excerpt
        .split('\n')
        .flat_map(|l| l.split(" ⏎ "))
        .map(|l| l.trim_start().to_string())
        .collect();
    // A hunk header is what makes a result a diff. Without one, leading `-`/`+` is
    // ordinary text — markdown bullets, `git log` decoration, a directory listing —
    // and reporting "+0 −3" for a file read is worse than reporting nothing.
    let hunk = lines
        .iter()
        .position(|l| l.starts_with("@@") && l.contains("@@"));
    let start = hunk?;
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in &lines[start + 1..] {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    if added == 0 && removed == 0 {
        return None;
    }
    Some(format!("+{added} −{removed}"))
}

fn tool_detail(excerpt: &str, ok: bool, pending: bool) -> String {
    let lines = tool_lines(excerpt);
    if pending {
        return lines
            .first()
            .map(|l| l.trim_start_matches('…').trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "running".into());
    }
    if !ok {
        return if lines.join(" ").to_lowercase().contains("cancel") {
            "cancelled".into()
        } else {
            lines
                .iter()
                .find(|l| !metadata_line(l))
                .cloned()
                .unwrap_or_else(|| "failed (no diagnostic output)".into())
        };
    }
    // A diff is more useful as a size than as a line count.
    if let Some(stat) = diff_stat(excerpt) {
        return stat;
    }
    if let Some(n) = total_lines(&lines) {
        return format!("{n} lines");
    }
    let content: Vec<&String> = lines.iter().filter(|l| !metadata_line(l)).collect();
    match content.as_slice() {
        [] => "(no output)".into(),
        [only] => (*only).clone(),
        many => format!("{} lines", many.len()),
    }
}

/// One readable phrase for a run of tool calls: "ran 3 shell commands · read 2 files".
/// Pending calls are named individually so the line says what is happening now.
fn call_summary(calls: &[ToolCall]) -> String {
    if calls.len() == 1 {
        let c = &calls[0];
        let verb = if c.pending {
            format!("running {}", c.name)
        } else if c.ok {
            format!("{} · {}", c.name, tool_detail(&c.excerpt, c.ok, c.pending))
        } else {
            format!("{} · failed", c.name)
        };
        return verb;
    }
    let mut groups: Vec<(&'static str, usize)> = Vec::new();
    let mut named: Vec<String> = Vec::new();
    let mut failed = 0;
    for call in calls {
        if !call.ok && !call.pending {
            failed += 1;
        }
        match category(&call.name) {
            Some((verb, singular)) => {
                let _ = singular;
                match groups.iter_mut().find(|(v, _)| *v == verb) {
                    Some((_, n)) => *n += 1,
                    None => groups.push((verb, 1)),
                }
            }
            None => named.push(call.name.clone()),
        }
    }
    let mut parts: Vec<String> = groups
        .iter()
        .map(|(verb, n)| format!("{verb} {n} {}", plural(verb, *n)))
        .collect();
    // Unrecognised tools are named, deduplicated, in first-appearance order.
    let mut seen: Vec<&str> = Vec::new();
    for name in &named {
        if !seen.contains(&name.as_str()) {
            seen.push(name);
        }
    }
    if !seen.is_empty() {
        parts.push(format!("called {}", seen.join(", ")));
    }
    let pending = calls.iter().any(|c| c.pending);
    if pending {
        parts.insert(0, "running".into());
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if parts.is_empty() {
        return format!("{} calls", calls.len());
    }
    parts.join(" · ")
}

/// Verb and noun for tools that fit a category; `None` means "name it explicitly".
fn category(name: &str) -> Option<(&'static str, &'static str)> {
    if name.starts_with("mcp__") {
        return Some(("called", "MCP tool"));
    }
    let base = name.rsplit("__").next().unwrap_or(name);
    match base {
        "bash" | "exec" | "shell" | "sh" => Some(("ran", "shell command")),
        "read" | "read_file" | "read_output" => Some(("read", "file")),
        "write" | "edit" | "apply_patch" | "multiedit" => Some(("edited", "file")),
        "search" | "grep" | "glob" | "find" => Some(("searched", "time")),
        "tree" | "ls" | "list" => Some(("listed", "directory")),
        "web_search" | "web_fetch" | "fetch" => Some(("fetched", "page")),
        _ => None,
    }
}

fn plural(verb: &str, n: usize) -> &'static str {
    if n == 1 {
        return match verb {
            "ran" => "shell command",
            "read" | "edited" => "file",
            "searched" => "time",
            "listed" => "directory",
            "fetched" => "page",
            "called" => "MCP tool",
            _ => "call",
        };
    }
    match verb {
        "ran" => "shell commands",
        "read" | "edited" => "files",
        "searched" => "times",
        "listed" => "directories",
        "fetched" => "pages",
        "called" => "MCP tools",
        _ => "calls",
    }
}

/// Render a run of tool calls the way a person reads them: what was run, how long it
/// took, and one line of outcome. A single call shows its command; a run of them shows
/// the summary phrase.
fn push_calls(
    rows: &mut Vec<TranscriptRow>,
    calls: &[ToolCall],
    elapsed: Option<std::time::Duration>,
    tick: u64,
    width: usize,
) {
    let pending = calls.iter().any(|c| c.pending);
    let failed = calls.iter().any(|c| !c.ok && !c.pending);
    let (mark, mark_style) = if pending {
        // A two-frame blink, so a running call is obviously alive.
        let glyph = crate::motion::spinner(tick);
        (glyph, theme::pending_fg())
    } else if failed {
        ('✗', theme::tool_err_fg())
    } else {
        ('✓', theme::tool_ok_fg())
    };

    // The head is the command for one call and a summary phrase for a run. A multi-line
    // command (heredocs) continues on indented rows; the timing stays on the head.
    let single_detail = calls.len() == 1 && !calls[0].detail.is_empty();
    let head = if single_detail {
        calls[0].detail.lines().next().unwrap_or("").to_string()
    } else if calls.len() == 1 && calls[0].pending {
        format!("running {}", calls[0].name)
    } else {
        call_summary(calls)
    };
    let mut head_spans = vec![
        Span::styled(format!("  {mark} "), mark_style),
        Span::styled(head, theme::assistant_fg()),
    ];
    if let Some(d) = elapsed {
        head_spans.push(Span::styled(
            format!(" · {}", format_duration(d)),
            theme::system_fg(),
        ));
    }
    push_spans(rows, head_spans, width);

    if single_detail {
        for line in calls[0].detail.lines().skip(1) {
            push_spans(
                rows,
                vec![Span::styled(format!("    {line}"), theme::system_fg())],
                width,
            );
        }
    }

    // Errors remain visible even when successful neighboring calls are collapsed.
    let shown: Vec<&ToolCall> = if calls.iter().any(|c| !c.ok && !c.pending) {
        calls.iter().filter(|c| !c.ok && !c.pending).collect()
    } else {
        calls.iter().take(1).collect()
    };
    for call in shown {
        let lines = tool_lines(&call.excerpt);
        let body = if call.pending {
            "running…".to_string()
        } else {
            tool_detail(&call.excerpt, call.ok, false)
        };
        if calls.len() > 1 && !call.ok {
            push_text(
                rows,
                "    ",
                &format!("{} · {}", call.name, call.detail),
                theme::tool_err_fg(),
                width,
            );
        }
        push_text(
            rows,
            "    ⎿  ",
            &body,
            if call.ok {
                theme::system_fg()
            } else {
                theme::tool_err_fg()
            },
            width,
        );
        if !call.ok {
            for line in lines.iter().filter(|l| !metadata_line(l)).skip(1).take(4) {
                // Failed-call context is an excerpt too: diffs from a refused patch
                // read the same as diffs from an applied one.
                push_colored_excerpt(rows, "       ", line, theme::system_fg(), width);
            }
        }
    }
    if calls.len() > 1 {
        push_text(
            rows,
            "       ",
            "Ctrl+O expands tool output",
            theme::system_fg(),
            width,
        );
    }
}

/// Push a styled line, wrapping it to the transcript width.
fn push_spans(rows: &mut Vec<TranscriptRow>, spans: Vec<Span<'static>>, width: usize) {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let style = spans
        .first()
        .map(|s| s.style)
        .unwrap_or_else(theme::assistant_fg);
    let prefix_cells = spans
        .first()
        .map(|s| {
            s.content
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
        })
        .unwrap_or(0);
    let mut first = true;
    for para in text.split('\n') {
        for (i, raw) in wrap(para, width.max(1)).into_iter().enumerate() {
            let lead = if first {
                String::new()
            } else {
                " ".repeat(prefix_cells)
            };
            first = false;
            rows.push(TranscriptRow {
                line: Line::styled(format!("{lead}{}", display(&raw)), style),
                raw,
                prefix_cells,
                soft: i > 0,
            });
        }
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

impl App {
    pub fn transcript_rows(&self, width: u16) -> Vec<TranscriptRow> {
        if let Some(rows) = &self.selection_snapshot {
            return rows.clone();
        }
        let mut rows: Vec<TranscriptRow> = vec![];
        let width = width.max(1) as usize;
        let mut prev = None;
        let mut index = 0;
        while index < self.blocks.len() {
            let block = &self.blocks[index];
            let kind = kind(block);
            if let Some(p) = prev {
                let has_gap = rows.last().is_some_and(|r| empty_line(&r.line));
                if !grouped(p, kind) && !has_gap {
                    rows.push(blank_row());
                }
            }
            match block {
                Block::User(t) => push_text(
                    &mut rows,
                    "❯ ",
                    t,
                    theme::user_fg().add_modifier(Modifier::BOLD),
                    width,
                ),
                Block::Assistant(t) => push_text(&mut rows, "● ", t, theme::assistant_fg(), width),
                // A run of consecutive calls is one line: the transcript should read
                // as what happened, not as a log of every invocation.
                Block::Tool { .. } => {
                    // A running call is shown on its own, with its command. Finished
                    // calls collapse into one summary line per run.
                    let running = matches!(
                        self.blocks.get(index),
                        Some(Block::Tool { pending: true, .. })
                    );
                    let mut calls: Vec<ToolCall> = Vec::new();
                    let mut look = index;
                    while let Some(Block::Tool {
                        name,
                        ok,
                        excerpt,
                        pending,
                        detail,
                        ..
                    }) = self.blocks.get(look)
                    {
                        calls.push(ToolCall {
                            name: name.clone(),
                            ok: *ok,
                            pending: *pending,
                            excerpt: clean(excerpt),
                            detail: detail.clone(),
                        });
                        look += 1;
                        if running {
                            break;
                        }
                    }
                    let elapsed = match self.blocks.get(index) {
                        Some(Block::Tool {
                            started,
                            pending,
                            elapsed,
                            ..
                        }) if *pending => started.map(|s| s.elapsed()),
                        Some(Block::Tool { elapsed, .. }) => *elapsed,
                        _ => None,
                    };
                    push_calls(&mut rows, &calls, elapsed, self.tick, width);
                    if self.show_tool_details {
                        for call in &calls {
                            push_text(
                                &mut rows,
                                "    ",
                                &format!("{} {}", call.name, call.detail),
                                theme::accent_fg(),
                                width,
                            );
                            // `detail` is the tool's path argument for file tools, so
                            // a read of `lib.rs` highlights as Rust.
                            push_tool_excerpt(
                                &mut rows,
                                "      ",
                                &call.excerpt,
                                theme::system_fg(),
                                width,
                                crate::code::Lang::from_path(&call.detail),
                            );
                        }
                    }
                    index = look - 1;
                }
                Block::System(t) => push_text(&mut rows, "· ", t, theme::system_fg(), width),
                Block::Member(t) => push_text(&mut rows, "· ", t, theme::system_fg(), width),
                Block::Banner => {
                    for line in crate::banner::lines(self.tick, width as u16) {
                        rows.push(TranscriptRow {
                            line: line.clone(),
                            raw: line.to_string(),
                            prefix_cells: 0,
                            soft: false,
                        });
                    }
                }
                Block::Rich(lines) => {
                    for line in lines {
                        rows.push(TranscriptRow {
                            line: line.clone(),
                            raw: line.to_string(),
                            prefix_cells: 0,
                            soft: false,
                        });
                    }
                }
            }
            prev = Some(kind);
            index += 1;
        }
        if let Some(s) = &self.streaming {
            if !rows.is_empty() && !rows.last().is_some_and(|r| empty_line(&r.line)) {
                rows.push(blank_row());
            }
            push_text(&mut rows, "● ", s, theme::assistant_fg(), width);
            if let Some(last) = rows.last_mut() {
                if !self.animations || (self.tick / 6) & 1 == 0 {
                    last.line.spans.push(Span::styled("▊", theme::accent_fg()));
                }
            }
        }
        rows
    }
    pub fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.transcript_rows(width)
            .into_iter()
            .map(|r| r.line)
            .collect()
    }
}
/// Convert terminal-cell columns to UTF-8 source offsets. Widen a boundary within a wide glyph.
fn raw_byte(raw: &str, cell: usize, end: bool) -> usize {
    let mut column = 0;
    for (i, c) in raw.char_indices() {
        let w = cells(c, column);
        if column >= cell {
            return i;
        }
        if column + w > cell {
            return if end { i + c.len_utf8() } else { i };
        }
        column += w;
    }
    raw.len()
}
pub fn copy(rows: &[TranscriptRow], sel: crate::SelRange) -> String {
    let mut a = (sel.start_line, sel.start_col);
    let mut b = (sel.end_line, sel.end_col);
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    let mut out = String::new();
    let mut wrote = false;
    for (i, row) in rows
        .iter()
        .enumerate()
        .take(b.0.saturating_add(1))
        .skip(a.0)
    {
        let first = if i == a.0 {
            a.1.saturating_sub(row.prefix_cells)
        } else {
            0
        };
        let last = if i == b.0 {
            b.1.saturating_sub(row.prefix_cells)
        } else {
            usize::MAX
        };
        let start = raw_byte(&row.raw, first, false);
        let end = raw_byte(&row.raw, last, true);
        if end < start {
            continue;
        }
        if wrote && !row.soft {
            out.push('\n');
        }
        out.push_str(&row.raw[start..end]);
        wrote = true;
    }
    out
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    fn calls(items: &[(&'static str, bool, bool)]) -> Vec<ToolCall> {
        items
            .iter()
            .map(|(name, ok, pending)| ToolCall {
                name: (*name).to_string(),
                ok: *ok,
                pending: *pending,
                excerpt: String::new(),
                detail: String::new(),
            })
            .collect()
    }

    #[test]
    fn a_run_of_calls_becomes_one_phrase() {
        let run = calls(&[
            ("bash", true, false),
            ("bash", true, false),
            ("read", true, false),
        ]);
        assert_eq!(call_summary(&run), "ran 2 shell commands · read 1 file");
    }

    #[test]
    fn failures_and_mcp_calls_are_called_out() {
        let run = calls(&[
            ("bash", true, false),
            ("bash", false, false),
            ("mcp__github__search", true, false),
        ]);
        assert_eq!(
            call_summary(&run),
            "ran 2 shell commands · called 1 MCP tool · 1 failed"
        );
    }

    #[test]
    fn a_running_call_shows_its_command_and_blinks() {
        let mut app = crate::App::new(
            true,
            crate::StatusLine {
                model: "mock".into(),
                ..Default::default()
            },
        );
        app.tool_started("bash", "{\"command\":\"cargo test\"}", "$ cargo test");
        app.tick = 0;
        let first: String = app
            .transcript_lines(80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(first.contains("$ cargo test"), "{first}");
        assert!(first.contains("running…"), "{first}");
        assert!(
            first.contains(crate::motion::spinner(0)),
            "spinner drawn while pending: {first}"
        );

        // Half a second later the dot has blinked to the hollow frame.
        app.tick = 6;
        let later: String = app
            .transcript_lines(80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            later.contains(crate::motion::spinner(6)),
            "spinner advances: {later}"
        );
        assert_ne!(crate::motion::spinner(0), crate::motion::spinner(6));
    }

    #[test]
    fn a_single_call_keeps_its_name() {
        let run = calls(&[("bash", true, false)]);
        assert_eq!(call_summary(&run), "bash · (no output)");
        let pending = calls(&[("bash", true, true)]);
        assert_eq!(call_summary(&pending), "running bash");
    }
}

#[cfg(test)]
mod diff_rendering_tests {
    use super::*;

    fn app_with_tool(excerpt: &str) -> crate::App {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        // Excerpts are shown when tool details are expanded (Ctrl+O).
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: "edit".into(),
            ok: true,
            excerpt: excerpt.into(),
            pending: false,
            detail: "edited a.rs".into(),
            started: None,
            elapsed: None,
        });
        app
    }

    /// The style applied to the row that contains `needle`.
    fn style_of(rows: &[TranscriptRow], needle: &str) -> Option<Style> {
        rows.iter().find_map(|r| {
            let text: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
            if text.contains(needle) {
                r.line.spans.last().map(|s| s.style)
            } else {
                None
            }
        })
    }

    #[test]
    fn diff_lines_are_coloured_by_their_prefix() {
        let excerpt = "edited a.rs\n--- a.rs\n+++ a.rs\n@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;";
        let app = app_with_tool(excerpt);
        let rows = app.transcript_rows(80);
        assert_eq!(
            style_of(&rows, "-let a = 1;"),
            Some(theme::tool_err_fg()),
            "removals are red"
        );
        assert_eq!(
            style_of(&rows, "+let a = 2;"),
            Some(theme::tool_ok_fg()),
            "additions are green"
        );
        assert_eq!(
            style_of(&rows, "@@ -1,2"),
            Some(theme::member_fg()),
            "hunk headers stand out"
        );
    }

    #[test]
    fn an_edit_reports_its_size_in_the_summary_line() {
        // Collapsed tool runs show one phrase per call; for an edit that phrase is the
        // diff size, so the user sees the scale without expanding anything.
        let excerpt = "edited a.rs\n--- a.rs\n+++ a.rs\n@@ -1,3 +1,3 @@\n-a\n-b\n+c\n";
        assert_eq!(diff_stat(excerpt), Some("+1 −2".into()));
        assert_eq!(diff_stat("read a.rs"), None, "non-diffs have no stat");
    }

    /// `diff_stat` decides whether a tool result is a diff at all, so a false positive
    /// mislabels ordinary output: a bullet list or `git log` would show a bogus
    /// "+0 −3" as the summary of a read.
    #[test]
    fn non_diff_output_is_not_mistaken_for_a_diff() {
        // Markdown bullets from a read of a document.
        assert_eq!(diff_stat("- one\n- two\n- three"), None);
        // git log --oneline style output.
        assert_eq!(diff_stat("+-- a commit subject\n-3 days ago"), None);
        // A unified-diff header alone is not a change.
        assert_eq!(diff_stat("--- a.rs\n+++ a.rs"), None);
        // A directory listing with permissions-looking dashes.
        assert_eq!(diff_stat("read a.rs\n- rw-r--r-- 1k"), None);
    }

    /// ...while a real diff (which always carries a hunk header) still counts.
    #[test]
    fn a_real_diff_is_still_counted() {
        assert_eq!(
            diff_stat("--- a.rs\n+++ a.rs\n@@ -1,1 +1,2 @@\n-a\n+b\n+c"),
            Some("+2 −1".into())
        );
        // A headerless `-`/`+` block is not a diff: there is nothing to anchor it, and
        // the real tools always emit the `@@` line.
        assert_eq!(diff_stat("-a\n+b\n+c"), None);
    }

    #[test]
    fn diff_stats_ignore_file_headers() {
        // `---`/`+++` are headers, not a removal and an addition.
        let excerpt = "--- a.rs\n+++ a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(diff_stat(excerpt), Some("+1 −1".into()));
    }

    #[test]
    fn a_pasted_diff_inside_an_answer_is_coloured() {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.push_block(crate::Block::Assistant(
            "Here is the change:\n--- a.rs\n+++ a.rs\n-gone\n+kept".into(),
        ));
        let rows = app.transcript_rows(80);
        assert_eq!(
            style_of(&rows, "+kept"),
            Some(theme::tool_ok_fg()),
            "an added line in prose is green too"
        );
        assert_eq!(style_of(&rows, "-gone"), Some(theme::tool_err_fg()));
    }
}

/// The tool result is re-joined with `⏎` for storage before the transcript renders it.
/// A diff larger than that join's line budget would be cut silently, contradicting the
/// documented behaviour that a large diff is clipped *with a marker*.
#[test]
fn a_large_diff_reaches_the_transcript_intact() {
    // Build a 112-line diff: under the diff module's own 120-line budget, and over
    // the old 40-line excerpt cap that used to cut every large edit in half.
    let mut excerpt = String::from("edited a.rs\n--- a.rs\n+++ a.rs\n@@ -1,56 +1,56 @@\n");
    for i in 0..56 {
        excerpt.push_str(&format!("-old line {i}\n"));
    }
    for i in 0..56 {
        excerpt.push_str(&format!("+new line {i}\n"));
    }
    let mut app = crate::tests::app_for_test();
    app.blocks.clear();
    app.show_tool_details = true;
    // Feed it the way the agent does: a call starts (pending block), then finishes
    // with its result. The finish path is where the excerpt is stored.
    app.tool_started("edit", "{\"path\":\"a.rs\"}", "edit a.rs");
    app.tool_finished("edit", true, &excerpt);
    let rows = app.transcript_rows(100);
    let rendered: String = rows
        .iter()
        .map(|r| {
            r.line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("+new line 55"),
        "the last added line must be visible, not silently dropped; got {} rows ending at: {:?}",
        rows.len(),
        rendered.lines().last()
    );
    assert!(
        rendered.contains("-old line 0"),
        "the first removed line must be visible"
    );
    // Each diff line must be its own row so it can carry its own colour.
    let diff_rows = rows
        .iter()
        .filter(|r| {
            let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
            t.trim_start().starts_with("+new line") || t.trim_start().starts_with("-old line")
        })
        .count();
    assert!(
        diff_rows >= 112,
        "each diff line needs its own row, got {diff_rows} of 112"
    );
}

#[cfg(test)]
mod code_highlight_tests {
    use super::*;

    fn styles(rows: &[TranscriptRow], needle: &str) -> Vec<Style> {
        rows.iter()
            .filter_map(|r| {
                let text: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                if !text.contains(needle) {
                    return None;
                }
                Some(
                    r.line
                        .spans
                        .iter()
                        .filter(|s| !s.content.trim().is_empty())
                        .map(|s| s.style)
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect()
    }

    #[test]
    fn a_rust_fence_is_highlighted_by_language() {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.push_block(crate::Block::Assistant(
            "Here:\n```rust\nfn main() {\n    let s = \"hi\"; // note\n}\n```\nDone.".into(),
        ));
        let rows = app.transcript_rows(80);
        let code = styles(&rows, "fn main");
        assert!(
            code.contains(&crate::code::keyword_style()),
            "`fn` must be styled as a keyword, got {code:?}"
        );
        let line = styles(&rows, "let s =");
        assert!(
            line.contains(&crate::code::string_style()),
            "the string literal must be styled, got {line:?}"
        );
        assert!(
            line.contains(&crate::code::comment_style()),
            "the comment must be styled, got {line:?}"
        );
    }

    #[test]
    fn prose_outside_the_fence_is_not_highlighted_as_code() {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.push_block(crate::Block::Assistant(
            "```rust\nlet x = 1;\n```\nfn is just a word here".into(),
        ));
        let rows = app.transcript_rows(80);
        let prose = styles(&rows, "fn is just");
        assert!(
            !prose.contains(&crate::code::keyword_style()),
            "code styling must stop at the closing fence, got {prose:?}"
        );
    }

    #[test]
    fn an_unknown_fence_tag_still_renders_without_losing_text() {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.push_block(crate::Block::Assistant(
            "```brainfuck\n+++[>+<-]\n```".into(),
        ));
        let rows = app.transcript_rows(80);
        let text: String = rows
            .iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("+++[>+<-]"), "code text must survive: {text}");
    }
}

#[cfg(test)]
mod source_highlight_tests {
    use super::*;

    fn styles_for(rows: &[TranscriptRow], needle: &str) -> Vec<Style> {
        rows.iter()
            .filter(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains(needle)
            })
            .flat_map(|r| {
                r.line
                    .spans
                    .iter()
                    .filter(|s| !s.content.trim().is_empty())
                    .map(|s| s.style)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn read_block(path: &str, excerpt: &str) -> crate::App {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: excerpt.into(),
            pending: false,
            detail: path.into(),
            started: None,
            elapsed: None,
        });
        app
    }

    #[test]
    fn reading_a_rust_file_highlights_it_as_rust() {
        let app = read_block(
            "src/lib.rs",
            "   1\tpub fn answer() -> u32 {\n   2\t    // forty two\n   3\t    42\n   4\t}",
        );
        let rows = app.transcript_rows(90);
        assert!(
            styles_for(&rows, "pub fn").contains(&crate::code::keyword_style()),
            "a Rust keyword must be styled in a .rs read"
        );
        assert!(
            styles_for(&rows, "forty two").contains(&crate::code::comment_style()),
            "the comment must be styled in a .rs read"
        );
    }

    #[test]
    fn reading_a_plain_text_file_is_not_highlighted() {
        // No language for .txt: keywords must not be invented.
        let app = read_block("notes.txt", "pub fn answer() is just prose here");
        let rows = app.transcript_rows(90);
        assert!(
            !styles_for(&rows, "pub fn").contains(&crate::code::keyword_style()),
            "plain text must not be highlighted as code"
        );
    }

    #[test]
    fn a_diff_keeps_its_colours_instead_of_being_tokenized() {
        // An edit of a .rs file shows a diff: red/green must win over keyword styling.
        let app = read_block(
            "src/lib.rs",
            "edited src/lib.rs\n--- lib.rs\n+++ lib.rs\n@@ -1,1 +1,1 @@\n-let a = 1;\n+let a = 2;",
        );
        let rows = app.transcript_rows(90);
        assert!(
            styles_for(&rows, "-let a = 1;").contains(&theme::tool_err_fg()),
            "removals stay red"
        );
        assert!(
            styles_for(&rows, "+let a = 2;").contains(&theme::tool_ok_fg()),
            "additions stay green"
        );
    }
}

#[cfg(test)]
mod wrapped_code_tests {
    use super::*;

    /// Wrapping splits a logical line into visual rows, and each row is tokenized on its
    /// own. A long comment therefore lost its colour on every continuation row: the tail
    /// of `// a very long note ...` was re-lexed as code and its words could light up as
    /// keywords. The whole logical line must be lexed once, then split.
    #[test]
    fn a_comment_that_wraps_stays_a_comment_on_every_row() {
        let long = "// this comment is deliberately long so that it wraps across rows and the continuation mentioning fn must stay a comment";
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: long.into(),
            pending: false,
            detail: "src/lib.rs".into(),
            started: None,
            elapsed: None,
        });
        let rows = app.transcript_rows(40);
        // The excerpt rows are the ones carrying the six-space indent; the collapsed
        // summary line above them is a different render and not what this test is about.
        let comment_rows: Vec<&TranscriptRow> = rows
            .iter()
            .filter(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                t.starts_with("      ") && t.trim_start().starts_with("//")
                    || (t.starts_with("      ")
                        && !t.trim_start().is_empty()
                        && !t.starts_with("      read"))
            })
            .filter(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                !t.contains("read src/lib.rs")
            })
            .collect();
        assert!(
            comment_rows.len() >= 2,
            "the comment must actually wrap into excerpt rows, got {comment_rows:?}"
        );
        // `system_fg()` and `comment_style()` are both DarkGray, so colour alone cannot
        // tell a preserved comment from a lost one. Use a keyword-bearing comment: if a
        // continuation row is re-lexed as code, `fn` inside it turns magenta.
        for row in comment_rows {
            let text: String = row.line.spans.iter().map(|s| s.content.as_ref()).collect();
            let body: Vec<&ratatui::text::Span> = row
                .line
                .spans
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .collect();
            assert!(
                !body.iter().any(|s| s.style == crate::code::keyword_style()),
                "a wrapped comment must not be re-lexed as code, row: {text:?} styles: {:?}",
                body.iter().map(|s| s.style).collect::<Vec<_>>()
            );
        }
    }
}

#[cfg(test)]
mod gutter_tests {

    /// `read` prefixes each line with a right-aligned line number. Highlighting the
    /// whole line styles those digits as numeric literals, so a file of prose looks
    /// like it is full of yellow numbers — the gutter is metadata, not source.
    #[test]
    fn line_number_gutters_are_not_highlighted_as_numbers() {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: "     1\tpub fn answer() -> u32 {\n     2\t    42\n     3\t}".into(),
            pending: false,
            detail: "src/lib.rs".into(),
            started: None,
            elapsed: None,
        });
        let rows = app.transcript_rows(90);
        let gutter_row = rows
            .iter()
            .find(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains("pub fn")
            })
            .expect("the code line must render");
        let gutter_text: String = gutter_row
            .line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        // The leading line number must not be yellow.
        let gutter_number_styled = gutter_row
            .line
            .spans
            .iter()
            .take_while(|s| !s.content.contains("pub"))
            .any(|s| s.style == crate::code::number_style());
        assert!(
            !gutter_number_styled,
            "the line-number gutter must not be styled as a numeric literal: {gutter_text:?}"
        );
        // ...but the real literal on the next line still is.
        let literal_row = rows
            .iter()
            .find(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains("42")
            })
            .expect("the literal line must render");
        assert!(
            literal_row
                .line
                .spans
                .iter()
                .any(|s| s.style == crate::code::number_style()),
            "a real numeric literal keeps its styling"
        );
    }
}

#[cfg(test)]
mod gutter_false_positive_tests {

    fn excerpt_app(tool: &str, detail: &str, excerpt: &str) -> crate::App {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: tool.into(),
            ok: true,
            excerpt: excerpt.into(),
            pending: false,
            detail: detail.into(),
            started: None,
            elapsed: None,
        });
        app
    }

    fn row_for(app: &crate::App, needle: &str) -> Option<String> {
        app.transcript_rows(90).iter().find_map(|r| {
            let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
            t.contains(needle).then_some(t)
        })
    }

    /// `split_gutter` sniffs "spaces, digits, spaces" from the front of a line. Only the
    /// `read` family actually emits a line-number gutter, so other tools' output that
    /// happens to start with a number must keep every character.
    #[test]
    fn non_read_tool_output_starting_with_a_number_keeps_its_leading_digits() {
        // bash output of a numbered list: no gutter exists here.
        let app = excerpt_app("bash", "$ cat list.txt", "  1  first item");
        let row = row_for(&app, "first item").expect("row must render");
        assert!(
            row.contains("1"),
            "the leading number is content, not a gutter, and must survive: {row:?}"
        );
    }

    /// A wrapped continuation row must never be mistaken for a gutter. The split runs on
    /// the logical line before wrapping, so a continuation that begins with a number is
    /// untouched.
    #[test]
    fn a_wrapped_continuation_starting_with_a_number_is_not_a_gutter() {
        let long = format!("{}  2  continued", "x".repeat(40));
        let app = excerpt_app("read", "src/lib.rs", &long);
        let rows = app.transcript_rows(40);
        let text: String = rows
            .iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("2") && text.contains("continued"),
            "a continuation row keeps its number and text: {text:?}"
        );
    }

    /// `grep` output is `path:line: text`, and the `detail` is the pattern, not a path
    /// with an extension — so it never gains a language and never reaches split_gutter
    /// as code. Even if it did, the prefix is `path:`, not spaces and digits.
    #[test]
    fn grep_output_with_a_numeric_match_is_not_mistaken_for_a_gutter() {
        let app = excerpt_app("grep", "TODO in src", "src/a.rs:7:     3 apples");
        let row = row_for(&app, "apples").expect("row must render");
        assert!(
            row.contains("src/a.rs:7:") && row.contains("3 apples"),
            "grep's own prefix and the matched text both survive: {row:?}"
        );
    }

    /// A read of a file that itself contains a numbered list: the tool gutter is the
    /// 6-wide field, and the list number inside the content is content.
    #[test]
    fn a_numbered_list_inside_a_read_file_keeps_its_own_number() {
        // A .rs path so highlighting (and therefore split_gutter) actually runs; with a
        // plain-text path the excerpt is not tokenized at all and this asserts nothing.
        let app = excerpt_app("read", "src/list.rs", "     5\t  1  first item");
        let row = row_for(&app, "first item").expect("row must render");
        assert!(
            row.contains("5"),
            "the tool gutter number stays visible: {row:?}"
        );
        assert!(
            row.contains("1"),
            "the list number in the file's own content stays visible: {row:?}"
        );
        // ...and the content's own number keeps numeric styling. Only the expanded
        // excerpt row is tokenized; the collapsed summary line above it is not, so
        // search for the indented row rather than the first row mentioning the text.
        let excerpt_row = app
            .transcript_rows(90)
            .into_iter()
            .find(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains("first item") && t.starts_with("      ")
            })
            .expect("the expanded excerpt row must render");
        assert!(
            excerpt_row
                .line
                .spans
                .iter()
                .any(|s| s.style == crate::code::number_style()),
            "the list number is code, so it keeps numeric styling: {:?}",
            excerpt_row.line.spans
        );
    }
}

#[cfg(test)]
mod wide_char_wrap_tests {

    /// `wrap` cuts rows by display *cells*; the piece distributor counted *chars*.
    /// With wide (2-cell) characters the two disagree: the styling drifts right of the
    /// text it belongs to, the row loses its tail, or the assertion about which span a
    /// character sits in fails outright.
    #[test]
    fn wide_characters_do_not_desync_highlighting_from_wrapping() {
        // `日` is 2 cells wide; the keyword after it must be found in a keyword-styled
        // span even when the line wraps right before it.
        let filler = "日".repeat(18); // 36 cells
        let line = format!("{filler} let x = 1;");
        let app = {
            let mut app = crate::tests::app_for_test();
            app.blocks.clear();
            app.show_tool_details = true;
            app.push_block(crate::Block::Tool {
                name: "read".into(),
                ok: true,
                excerpt: line.clone(),
                pending: false,
                detail: "src/lib.rs".into(),
                started: None,
                elapsed: None,
            });
            app
        };
        let rows = app.transcript_rows(44);
        // Every character of the logical line must still be on screen exactly once.
        let text: String = rows
            .iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let flown: String = text
            .lines()
            .map(|l| {
                l.trim_start_matches(' ')
                    .trim_start_matches("⎿")
                    .trim_start()
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            flown.contains(&line),
            "wrapped text must be complete and in order: {text:?}"
        );
        // And `let` must live in a keyword-styled span.
        // Only the indented excerpt row is tokenized; the collapsed summary line
        // above it is plain, so target the excerpt row explicitly.
        // The transcript renders a collapsed summary *and* the expanded excerpt, and
        // only the excerpt is tokenized. Pick the row that actually carries styling.
        let keyword_row = rows
            .iter()
            .filter(|r| {
                let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains("let x")
            })
            .max_by_key(|r| r.line.spans.len())
            .expect("the row containing the keyword must render");
        let keyword_ok = keyword_row
            .line
            .spans
            .iter()
            .any(|s| s.content.contains("let") && s.style == crate::code::keyword_style());
        assert!(
            keyword_ok,
            "`let` must be keyword-styled after wide characters: {:?}",
            keyword_row.line.spans
        );
    }
}

#[cfg(test)]
mod tab_wrap_tests {

    /// `wrap` slices the *raw* logical line, which may contain tabs, and each row is
    /// then expanded by `display`. Pieces come from `display(para)` — already expanded.
    /// A tab therefore makes the per-row cell count disagree with the pieces, and the
    /// styling drifts. Highlighting must be computed against the same text the rows
    /// are cut from.
    #[test]
    fn a_tab_near_a_wrap_boundary_does_not_desync_styling() {
        let line = format!("{}\tlet x = 1;", "a".repeat(20));
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: line,
            pending: false,
            detail: "src/lib.rs".into(),
            started: None,
            elapsed: None,
        });
        let rows = app.transcript_rows(30);
        let keyword =
            rows.iter()
                .filter(|r| {
                    let t: String = r.line.spans.iter().map(|s| s.content.as_ref()).collect();
                    t.contains("let x")
                })
                .max_by_key(|r| r.line.spans.len())
                .map(|r| {
                    r.line.spans.iter().any(|s| {
                        s.content.contains("let") && s.style == crate::code::keyword_style()
                    })
                });
        assert_eq!(
            keyword,
            Some(true),
            "`let` must stay keyword-styled when a tab precedes a wrap"
        );
    }
}

#[cfg(test)]
mod narrow_width_tests {
    /// At a narrow width a wide glyph can fill a whole row, leaving `want` smaller than
    /// the next piece's first character. Skipping that piece would drop its text from
    /// the screen entirely — a silent data loss, not just a colour change.
    #[test]
    fn a_wide_glyph_at_a_narrow_width_is_never_dropped() {
        let line = "日let x";
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        app.show_tool_details = true;
        app.push_block(crate::Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: line.into(),
            pending: false,
            detail: "src/lib.rs".into(),
            started: None,
            elapsed: None,
        });
        // Strip every row's prefix so only the wrapped payload remains, then the
        // concatenation must equal the original line exactly.
        // The transcript renders the collapsed summary and the expanded excerpt, so the
        // same payload can appear more than once. The property that matters is that no
        // character is lost: the concatenated payloads must contain the line intact.
        let payloads: Vec<String> = app
            .transcript_rows(8)
            .iter()
            .filter(|r| r.line.spans.len() > 1)
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .skip(1)
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let joined: String = payloads.join("").replace(' ', "");
        assert!(
            joined.contains("日letx"),
            "every character must survive narrow wrapping, got {payloads:?}"
        );
    }
}

#[cfg(test)]
mod copy_alignment_tests {
    use super::*;

    fn row(raw: &str, line: Line<'static>, prefix: usize) -> TranscriptRow {
        TranscriptRow {
            line,
            raw: raw.into(),
            prefix_cells: prefix,
            soft: false,
        }
    }

    /// Copying a selection must return exactly the characters painted in those cells.
    /// A tab expands to several spaces on screen; if the copy mapped cells to the raw
    /// text naively it would return the tab's neighbours instead.
    #[test]
    fn copying_a_selection_over_a_tab_returns_the_painted_text() {
        let raw = "a\tbcd";
        let painted = format!("a{}bcd", " ".repeat(3)); // tab at column 1 → 3 spaces
        let r = row(raw, Line::styled(painted.clone(), theme::system_fg()), 0);
        // Select the cells covering "b" and "c" (after the expanded tab).
        let sel = crate::SelRange {
            start_line: 0,
            start_col: 4,
            end_line: 0,
            end_col: 6,
        };
        let copied = copy(&[r], sel);
        assert_eq!(
            copied, "bc",
            "copying cells 4..6 of {painted:?} must yield the painted characters"
        );
    }

    /// A tab followed by a wide glyph is the worst case for cell math: the tab's
    /// width depends on the running column, and the wide glyph occupies two cells
    /// for one char. Copying must still return the painted characters exactly.
    #[test]
    fn copying_a_selection_over_a_tab_and_a_wide_glyph_is_exact() {
        // "a" + tab(at col 1 → 3 spaces) + "日" (2 cells) + "z"
        let raw = "a\t日z";
        let painted = format!("a{}日z", " ".repeat(3));
        let r = row(raw, Line::styled(painted.clone(), theme::system_fg()), 0);
        // Cells: a=0, spaces=1..3, 日=4..5, z=6
        let sel = crate::SelRange {
            start_line: 0,
            start_col: 4,
            end_line: 0,
            end_col: 7,
        };
        let copied = copy(&[r], sel);
        assert_eq!(
            copied, "日z",
            "copying cells 4..7 of {painted:?} must yield the painted characters"
        );
    }

    /// A prefix (the `❯ ` marker or an indent) is not part of the selectable text.
    #[test]
    fn the_row_prefix_is_excluded_from_the_copy() {
        let r = row(
            "hello",
            Line::styled("❯ hello".to_string(), theme::system_fg()),
            2,
        );
        let sel = crate::SelRange {
            start_line: 0,
            start_col: 2,
            end_line: 0,
            end_col: 7,
        };
        assert_eq!(copy(&[r], sel), "hello");
    }
}

#[cfg(test)]
mod copy_wide_tests {
    use super::*;

    /// A selection edge landing *inside* a 2-cell glyph: the copy must not emit half a
    /// character (invalid UTF-8) nor drop the glyph from a selection that visually
    /// covers it.
    #[test]
    fn a_selection_edge_inside_a_wide_glyph_is_handled() {
        let raw = "a日b";
        let r = TranscriptRow {
            line: Line::styled(raw.to_string(), theme::system_fg()),
            raw: raw.into(),
            prefix_cells: 0,
            soft: false,
        };
        // Column 2 is the middle of `日` (it occupies cells 1..3).
        let sel = crate::SelRange {
            start_line: 0,
            start_col: 2,
            end_line: 0,
            end_col: 4,
        };
        let copied = copy(&[r], sel);
        assert!(
            copied.chars().all(|c| c.is_ascii() || c == '日'),
            "the copy must not contain a broken character: {copied:?}"
        );
        // Starting mid-glyph must not silently drop the glyph entirely.
        assert!(
            copied.contains('日'),
            "a selection starting inside a wide glyph still includes it: {copied:?}"
        );
    }
}

#[cfg(test)]
mod escape_sequence_tests {
    use super::clean;

    /// Tool output is arbitrary bytes. `clean` strips CSI and OSC, but not the
    /// string sequences (DCS `ESC P`, SOS `ESC X`, PM `ESC ^`, APC `ESC _`), whose
    /// payload is data — a sixel image or a terminal query — not text. Leaking it
    /// painted `q#0;0;0;0` into the transcript as if the command had printed it.
    #[test]
    fn string_sequence_payloads_do_not_reach_the_transcript() {
        for (input, leaked) in [
            ("\x1bPq#0;0;0;0\x1b\\sixel", "q#0;0;0;0"),
            ("\x1bP1;2;3q data \x1b\\", "q data"),
            ("\x1bXsos payload\x1b\\", "sos payload"),
            ("\x1b^pm payload\x1b\\", "pm payload"),
            ("\x1b_apc payload\x1b\\", "apc payload"),
        ] {
            let out = clean(input);
            assert!(
                !out.contains(leaked),
                "the sequence payload must be dropped, got {out:?} from {input:?}"
            );
        }
    }

    /// The text around a sequence is still the command's real output.
    #[test]
    fn text_around_a_string_sequence_survives() {
        assert_eq!(clean("a\x1bPq payload\x1b\\b"), "ab");
        assert_eq!(clean("\x1b]0;title\x07hello"), "hello");
        assert_eq!(clean("\x1b[31mred\x1b[0m"), "red");
    }
}

/// A mouse selection is anchored to the row list frozen at click time, while
/// `transcript_point` (drag/up) and `selected_text` (copy) both look rows up again.
/// They stay consistent because `transcript_rows` returns `selection_snapshot`
/// first, ignoring the requested width — so a re-render between Down and Up (streaming
/// output, resize, a new block) cannot make the point and the copied rows disagree.
/// This pins that contract: an out-of-range or reversed selection degrades to a
/// shorter string instead of panicking on an index.
#[cfg(test)]
mod selection_snapshot_tests {
    use super::*;

    fn app_with_lines(n: usize) -> crate::App {
        let mut app = crate::tests::app_for_test();
        app.blocks.clear();
        for i in 0..n {
            app.push_block(crate::Block::Assistant(format!("line {i}")));
        }
        app
    }

    #[test]
    fn the_frozen_rows_win_over_width_and_later_output() {
        let mut app = app_with_lines(5);
        let frozen = app.transcript_rows(80);
        app.selection_snapshot = Some(frozen.clone());
        app.selection = Some(crate::SelRange {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 3,
        });
        // Output streams in and the terminal is resized between Down and Drag/Up.
        for i in 0..10 {
            app.push_block(crate::Block::Assistant(format!("new {i}")));
        }
        assert_eq!(
            app.transcript_rows(40).len(),
            frozen.len(),
            "the anchored snapshot must be returned, not re-rendered rows"
        );
        assert_eq!(
            app.transcript_lines(40).len(),
            frozen.len(),
            "transcript_point reads through the same snapshot"
        );
    }

    #[test]
    fn an_out_of_range_or_reversed_selection_never_panics() {
        let app = app_with_lines(4);
        let rows = app.transcript_rows(80);
        // Dragged far past the end: clipped, not an index panic.
        let far = crate::SelRange {
            start_line: 0,
            start_col: 0,
            end_line: 999,
            end_col: 999,
        };
        let text = copy(&rows, far);
        assert!(
            text.contains("line 0"),
            "still copies what exists: {text:?}"
        );
        // Dragged upwards: the endpoints are swapped, so the same span comes back.
        let fwd = crate::SelRange {
            start_line: 0,
            start_col: 0,
            end_line: 3,
            end_col: 99,
        };
        let rev = crate::SelRange {
            start_line: 3,
            start_col: 99,
            end_line: 0,
            end_col: 0,
        };
        assert_eq!(
            copy(&rows, fwd),
            copy(&rows, rev),
            "a selection dragged upwards must copy the same text"
        );
    }
}
