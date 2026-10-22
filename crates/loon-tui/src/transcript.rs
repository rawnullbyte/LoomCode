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
    for para in text.split('\n') {
        let trimmed = para.trim_start();
        let fence = markdown && trimmed.starts_with("```");
        let body_style = if markdown && (fence || code) {
            theme::tool_ok_fg()
        } else if markdown && trimmed.starts_with('#') {
            theme::accent_fg().add_modifier(Modifier::BOLD)
        } else {
            style
        };
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
                    } else {
                        // Fenced code and diff lines inside prose keep their own
                        // colour: a pasted patch in an answer reads like a patch.
                        let line_style = diff_style(&text).unwrap_or(body_style);
                        spans.push(Span::styled(text, line_style));
                    }
                    spans
                }),
                raw,
                prefix_cells,
                soft: i > 0,
            });
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
    let prefix_cells = cell_width(marker).min(width.saturating_sub(1));
    let marker = take_cells(marker, prefix_cells);
    let indent = " ".repeat(prefix_cells);
    let avail = width.saturating_sub(prefix_cells).max(1);
    let lines = clean(excerpt);
    let mut first = true;
    let mut in_code = false;
    for para in lines.split('\n') {
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
        for (i, raw) in wrap(para, avail).into_iter().enumerate() {
            let lead = if first {
                marker.clone()
            } else {
                indent.clone()
            };
            first = false;
            rows.push(TranscriptRow {
                line: Line::from(vec![
                    Span::styled(lead, style),
                    Span::styled(display(&raw), body_style),
                ]),
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
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in excerpt.split('\n').flat_map(|l| l.split(" ⏎ ")) {
        let line = line.trim_start();
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
                            push_colored_excerpt(
                                &mut rows,
                                "      ",
                                &call.excerpt,
                                theme::system_fg(),
                                width,
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
