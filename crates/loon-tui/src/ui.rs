//! Layout and drawing.
//!
//! Zones: transcript (which begins with the animated loom banner and scrolls it away),
//! (optional) slash-command popup above the input, input line, one-row status bar. The transparent invariant holds everywhere: every
//! style comes from the theme, so every background is `Color::Reset` and reverse video
//! is never used; animation is foreground glyph/colour cycling driven by `App::tick`.

use crate::{theme, App};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

/// Most input rows shown before the editor scrolls its own view.
const MAX_INPUT_LINES: usize = 8;
/// Most queued prompts previewed above the input; the status bar carries the count.
const MAX_QUEUE_ROWS: usize = 3;

pub fn draw(f: &mut Frame, app: &mut App) {
    draw_inner(f, app);
    apply_terminal_preferences(f, app);
}
fn draw_inner(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if app.viewport_area.is_some() && app.viewport_area != Some(area) {
        app.resized();
    }
    app.viewport_area = Some(area);
    crate::manager::preview_theme(app);
    let completions = app.visible_completions();
    let desired_popup = completions.len().min(app.popup_rows.clamp(3, 20)) as u16;
    // The banner lives in the transcript, so there is no fixed header: the transcript
    // owns everything between the top of the screen and the input.
    let footer_h = if app.footer && area.height >= 2 { 1 } else { 0 };
    // The input grows with its lines (Shift+Enter), up to eight visible rows.
    let lines = app.input_lines().len().min(MAX_INPUT_LINES);
    let room = area.height.saturating_sub(footer_h);
    let input_h = if area.height >= 6 {
        ((lines as u16) + 2).min(room.saturating_sub(3)).max(3)
    } else if area.height > footer_h {
        1
    } else {
        0
    };
    let spare = area.height.saturating_sub(footer_h + input_h);
    let transcript_min = spare.min(3);
    // Queued prompts sit as dim rows directly above the prompt, so what is waiting is
    // visible instead of hidden behind a counter in the status bar. The budget also covers
    // the separator rule and, when prompts are clipped, the "+N earlier" line.
    let queued_visible = (app.queued.len() as u16).min(MAX_QUEUE_ROWS as u16);
    let queued_overflow = app.queued.len() as u16 > queued_visible;
    let queue_rows = if queued_visible == 0 {
        0
    } else {
        (queued_visible + 1 + u16::from(queued_overflow)).min(spare.saturating_sub(transcript_min))
    };
    let popup_rows = desired_popup.min(spare.saturating_sub(transcript_min + queue_rows));
    let [transcript, queue, popup, input, footer] = Layout::vertical([
        Constraint::Min(transcript_min),
        Constraint::Length(queue_rows),
        Constraint::Length(popup_rows), // borderless [CC]-style list
        Constraint::Length(input_h),
        Constraint::Length(footer_h),
    ])
    .areas(area);
    app.popup_rect = None;
    // The input block has only top/bottom borders, so its content starts at input.x
    // (no left border column to offset).
    app.input_rect = if input.height >= 3 {
        Some((input.x, input.y + 1))
    } else if input.height > 0 {
        Some((input.x, input.y))
    } else {
        None
    };
    app.transcript_rect = Some((
        transcript.x,
        transcript.y,
        transcript.width,
        transcript.height,
    ));

    let theme = theme::Theme::from_palette(&app.palette, app.transparent);
    f.render_widget(Paragraph::new("").style(theme.base()), area);

    // ── transcript, with selection highlight ────────────────────────────────────
    let lines = app.transcript_lines(transcript.width);
    let total = lines.len();
    let visible = transcript.height as usize;
    if let Some(off) = app.scroll_from_bottom {
        app.scroll_from_bottom = Some(off.min(total.saturating_sub(visible)));
    }
    let (start, end) = match app.scroll_from_bottom {
        None => (total.saturating_sub(visible), total),
        Some(off) => {
            let end = total.saturating_sub(off);
            (end.saturating_sub(visible), end)
        }
    };
    // Normalise first, then overlay selection: normalisation paints every span with
    // the theme background, so doing it afterwards would erase the selection bar.
    let sel = app.selection;
    let shown: Vec<Line> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let line = normalize_line(line.clone(), &theme);
            match sel {
                Some(sel) => apply_selection(&line, (start + i) as u64, &sel),
                None => line,
            }
        })
        .collect();
    f.render_widget(Paragraph::new(shown).style(theme.base()), transcript);

    // ── queued prompts, dim, directly above the prompt bar ──────────────────────
    // Oldest at the top, newest nearest the input: Up on an empty input pulls back
    // exactly the row sitting against the prompt. The first row is a dim rule so the
    // waiting prompts do not read as part of the assistant's answer above them.
    if queue_rows > 0 && queue.height > 0 {
        let visible = app.queued.len().min(MAX_QUEUE_ROWS);
        let skip = app.queued.len().saturating_sub(visible);
        let mut rows: Vec<Line> = Vec::new();
        if queue.height > visible as u16 {
            rows.push(Line::from(Span::styled(
                "─".repeat(queue.width as usize),
                theme.border(),
            )));
        }
        // Tell the user when older prompts are not shown, rather than silently dropping
        // them: the count in the status bar is the total, this is what fits.
        let overflow = if skip > 0 {
            format!("↳ +{skip} earlier")
        } else {
            String::new()
        };
        if !overflow.is_empty() && rows.len() + visible < queue.height as usize {
            rows.push(Line::from(Span::styled(
                clip_cells(&overflow, queue.width as usize),
                theme.dim(),
            )));
        }
        for text in app.queued.iter().skip(skip) {
            let first = text.lines().next().unwrap_or("");
            let more = if text.lines().count() > 1 { " …" } else { "" };
            // A preview, not content: clip to the frame instead of letting the terminal
            // cut the text at the edge with no ellipsis.
            let label = format!("↳ {first}{more}");
            rows.push(Line::from(Span::styled(
                clip_cells(&label, queue.width as usize),
                theme.dim(),
            )));
        }
        f.render_widget(Paragraph::new(rows).style(theme.base()), queue);
    }

    // ── slash-command popup, directly above the input ───────────────────────────
    // [CC] style: borderless list, "❯ " marker on the hovered/selected row, name
    // column padded to a common width, description dimmed beside it. Hovering
    // highlights only; Enter/click picks.
    if popup_rows > 0 && popup.height > 0 {
        app.popup_rect = Some((popup.x, popup.y, popup.height));
        let selected = app.completion_selected.min(completions.len() - 1);
        // window the list so the selection stays visible when there are more rows
        // than fit
        let start = selected
            .saturating_sub(popup_rows.saturating_sub(1) as usize)
            .min(completions.len().saturating_sub(popup_rows as usize));
        app.popup_window_start = start;
        let visible: Vec<_> = completions
            .iter()
            .skip(start)
            .take(popup_rows as usize)
            .collect();
        // Shared name-column width, capped so descriptions still have room.
        let name_w = visible
            .iter()
            .map(|c| c.display.chars().count() + 1) // leading '/' (or indent)
            .max()
            .unwrap_or(0)
            .min((popup.width as usize / 2).max(8));
        let mut rows: Vec<Line> = Vec::new();
        for (i, c) in visible.iter().enumerate() {
            let idx = start + i;
            // Only command rows are slash commands: an argument value is shown as the
            // value itself, indented under the command column, never as "/90%".
            let full_name = match c.kind {
                crate::autocomplete::Kind::Command => format!("/{}", c.display),
                crate::autocomplete::Kind::Argument => format!(" {}", c.display),
            };
            let name = if full_name.chars().count() > name_w {
                full_name
                    .chars()
                    .take(name_w.saturating_sub(1))
                    .collect::<String>()
                    + "…"
            } else {
                full_name
            };
            let pad = name_w.saturating_sub(name.chars().count());
            let marker = if idx == selected { "❯ " } else { "  " };
            let name_style = if idx == selected {
                theme.accent_bold()
            } else {
                theme.base()
            };
            let desc_style = theme.dim();
            rows.push(Line::from(vec![
                Span::styled(marker, theme.accent_bold()),
                Span::styled(name, name_style),
                Span::styled(" ".repeat(pad), theme.base()),
                Span::styled("  ", theme.base()),
                Span::styled(c.description.clone(), desc_style),
            ]));
        }
        f.render_widget(Paragraph::new(rows).style(theme.base()), popup);
    }

    // ── input: a steady caret; motion belongs to the header, not the prompt ─────
    let prompt = "›".to_string();
    let block = RtBlock::default()
        .borders(if input.height >= 3 {
            Borders::TOP | Borders::BOTTOM
        } else {
            Borders::NONE
        })
        .border_style(if app.status.busy && app.animations {
            theme.accent().fg(crate::motion::pulse(app.tick))
        } else {
            theme.border()
        });
    let input_chars: Vec<char> = app.input.chars().collect();
    app.cursor = app.cursor.min(input_chars.len());
    let text_lines: Vec<String> = app.input_lines().into_iter().map(str::to_string).collect();
    let (cursor_line, cursor_col) = app.cursor_line_col();
    // Horizontal scroll follows the cursor line only.
    let available = input.width.saturating_sub(3).max(1) as usize;
    let current: Vec<char> = text_lines
        .get(cursor_line)
        .map(|l| l.chars().collect())
        .unwrap_or_default();
    app.input_view_start = app.input_view_start.min(cursor_col);
    while current[app.input_view_start..cursor_col]
        .iter()
        .map(|c| c.width().unwrap_or(0))
        .sum::<usize>()
        >= available
        && app.input_view_start < cursor_col
    {
        app.input_view_start += 1;
    }
    let shown: Vec<Line> = if app.input.is_empty() && app.tips {
        vec![Line::from(vec![
            Span::styled(prompt.clone(), theme.accent_bold()),
            Span::styled(
                format!(
                    " {}",
                    if app.tip_text.is_empty() {
                        placeholder_tip(app.tick)
                    } else {
                        &app.tip_text
                    }
                ),
                theme.dim(),
            ),
        ])]
    } else {
        let selection = app.input_selection();
        text_lines
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let marker = if i == 0 { prompt.clone() } else { " ".into() };
                let view = if i == cursor_line {
                    app.input_view_start
                } else {
                    0
                };
                // Char offsets of this line's start, so the selection can be mapped
                // from absolute input offsets onto this line's spans.
                let line_start: usize = text_lines
                    .iter()
                    .take(i)
                    .map(|l| l.chars().count() + 1)
                    .sum();
                let line_len = text.chars().count();
                if let Some((lo, hi)) = selection {
                    // Intersect the selection with this line, in line-local chars.
                    let sel_lo = lo.saturating_sub(line_start).min(line_len);
                    let sel_hi = hi.saturating_sub(line_start).min(line_len);
                    if sel_lo < sel_hi {
                        let before: String = text
                            .chars()
                            .skip(view)
                            .take(sel_lo.saturating_sub(view))
                            .collect();
                        let selected: String = text
                            .chars()
                            .skip(sel_lo.max(view))
                            .take(sel_hi.saturating_sub(sel_lo.max(view)))
                            .collect();
                        let after: String = text.chars().skip(sel_hi.max(view)).collect();
                        return Line::from(vec![
                            Span::styled(marker, theme.accent_bold()),
                            Span::styled(format!(" {before}"), theme.base()),
                            Span::styled(selected, crate::theme::selection_fg()),
                            Span::styled(after, theme.base()),
                        ]);
                    }
                }
                let body: String = text.chars().skip(view).collect();
                Line::from(vec![
                    Span::styled(marker, theme.accent_bold()),
                    Span::styled(format!(" {body}"), theme.base()),
                ])
            })
            .collect()
    };
    let content_height = if input.height >= 3 {
        input.height - 2
    } else {
        input.height
    };
    let input_start = cursor_line.saturating_sub(content_height.saturating_sub(1) as usize);
    // Recorded for mouse hit-testing: a click maps to a (line, column) inside the
    // whole input, not just the first line.
    app.input_scroll = input_start;
    app.input_rows = content_height;
    let shown = shown.into_iter().skip(input_start).collect::<Vec<_>>();
    let para = Paragraph::new(shown).block(block).style(theme.base());
    f.render_widget(para, input);

    // Use the whole row: identity, activity, and resource usage have distinct zones.
    if footer.height > 0 {
        draw_status(f, app, &theme, footer);
    }

    if app.navigator.is_some() || app.editor.is_some() {
        crate::manager::draw(f, app, &theme);
        return;
    }

    // The caret stays visible while a turn runs: prompts typed during a turn are
    // still edited (and queued), so hiding the cursor made arrows look dead.
    if input.height > 0 && input.width > 0 {
        let cursor_cells = current[app.input_view_start.min(cursor_col)..cursor_col]
            .iter()
            .map(|c| c.width().unwrap_or(0))
            .sum::<usize>();
        let x = (prompt.chars().count() + 1 + cursor_cells)
            .min(input.width.saturating_sub(1) as usize) as u16;
        let visible = content_height as usize;
        let row = cursor_line
            .saturating_sub(input_start)
            .min(visible.saturating_sub(1)) as u16;
        f.set_cursor_position((input.x + x, input.y + u16::from(input.height >= 3) + row));
    }
}

/// Normalise a transcript line onto the theme: every span gets the theme background
/// (Reset in transparent mode), never reverse video.
fn normalize_line(mut line: Line<'static>, theme: &theme::Theme) -> Line<'static> {
    let base = theme.base();
    line.style = base;
    for span in &mut line.spans {
        span.style.fg = match span.style.fg {
            Some(Color::Reset) => base.fg,
            Some(Color::Cyan | Color::Magenta) => theme.accent().fg,
            Some(Color::Green) => theme.ok().fg,
            Some(Color::Red) => theme.err().fg,
            Some(Color::Yellow) => theme.warn().fg,
            Some(Color::DarkGray) => theme.dim().fg,
            color => color,
        };
        span.style.bg = base.bg;
    }
    line
}

/// Overlay the selection for one transcript line: the selected character range gets
/// the theme's grey selection background (the one deliberate exception to bg=Reset).
fn apply_selection(line: &Line<'static>, abs_line: u64, sel: &crate::SelRange) -> Line<'static> {
    let (first_line, first_col, last_line, last_col) =
        if (sel.start_line, sel.start_col) <= (sel.end_line, sel.end_col) {
            (
                sel.start_line as u64,
                sel.start_col,
                sel.end_line as u64,
                sel.end_col,
            )
        } else {
            (
                sel.end_line as u64,
                sel.end_col,
                sel.start_line as u64,
                sel.start_col,
            )
        };
    if abs_line < first_line || abs_line > last_line {
        return line.clone();
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let begin = if abs_line == first_line { first_col } else { 0 };
    let stop = if abs_line == last_line {
        last_col
    } else {
        text.chars().count()
    };
    if stop <= begin {
        return line.clone();
    }
    use unicode_width::UnicodeWidthChar;
    let cell_byte = |column: usize| -> usize {
        let mut n = 0;
        for (i, c) in text.char_indices() {
            if n >= column {
                return i;
            }
            n += c.width().unwrap_or(0);
        }
        text.len()
    };
    let (b0, b1, b2) = (cell_byte(begin), cell_byte(stop), text.len());
    let mut out = Line::default();
    if b0 > 0 {
        out.spans
            .push(Span::styled(text[..b0].to_string(), line.style));
    }
    out.spans.push(Span::styled(
        text[b0..b1].to_string(),
        theme::selection_fg(),
    ));
    if b2 > b1 {
        out.spans
            .push(Span::styled(text[b1..b2].to_string(), line.style));
    }
    out.style = line.style;
    out
}

fn draw_status(f: &mut Frame, app: &App, theme: &theme::Theme, area: ratatui::layout::Rect) {
    use ratatui::layout::Alignment;
    if area.width < 55 {
        // Narrow terminals still have to answer the two questions worth a glance: is it
        // working, and is anything waiting on me. Dropping the queue count here left a
        // user with three queued prompts staring at "● Ready" and no sign of them.
        let marker = if app.status.busy {
            crate::motion::spinner(app.tick).to_string()
        } else {
            "●".to_string()
        };
        let state = if app.request_rate.active() {
            format!("{} · {}", app.request_rate.label(), app.activity)
        } else {
            app.activity.clone()
        };
        let queue = match app.queued.len() {
            0 => String::new(),
            n if app.queue_paused => format!(" · {n} paused"),
            n => format!(" · {n} queued"),
        };
        // Keep the queue when everything does not fit: the activity word truncates with an
        // ellipsis, the count never does.
        let tail = format!("{state}{queue}");
        let budget = (area.width as usize).saturating_sub(marker.chars().count() + 1);
        let shown = if tail.chars().count() <= budget {
            tail
        } else if queue.is_empty() {
            clip_cells(&tail, budget)
        } else {
            let room = budget.saturating_sub(queue.chars().count());
            format!("{}{queue}", clip_cells(&state, room))
        };
        f.render_widget(
            Paragraph::new(format!("{marker} {shown}")).style(theme.accent()),
            area,
        );
        return;
    }
    let [identity, activity, resources] = Layout::horizontal([
        Constraint::Percentage(26),
        Constraint::Percentage(42),
        Constraint::Percentage(32),
    ])
    .areas(area);
    let model = app
        .status
        .model
        .rsplit('/')
        .next()
        .unwrap_or(&app.status.model);
    let left = vec![
        (
            format!(" {} ", app.status.mode),
            theme.mode_style(&app.status.mode),
        ),
        (format!("│ {model}"), theme.accent_bold()),
    ];
    f.render_widget(
        Paragraph::new(Line::from(clip_spans(left, identity.width))),
        identity,
    );
    let activity_text = if app.status.busy {
        format!(
            "{} {}",
            crate::motion::spinner(if app.animations { app.tick } else { 0 }),
            if app.spinner_verbs.is_empty() {
                app.activity.as_str()
            } else {
                app.spinner_verbs[(app.tick as usize / 30) % app.spinner_verbs.len()].as_str()
            }
        )
    } else {
        app.activity.clone()
    };
    let middle = vec![
        (
            activity_text,
            if app.status.busy {
                theme.warn()
            } else {
                theme.ok()
            },
        ),
        (
            if app.work_state.ultraloop {
                " · ULTRA"
            } else {
                ""
            }
            .into(),
            theme.accent(),
        ),
    ];
    f.render_widget(
        Paragraph::new(Line::from(clip_spans(middle, activity.width))).alignment(Alignment::Center),
        activity,
    );
    let context = if app.context_limit > 0 {
        // Compact units: a 1M window reads `1M`, not the old `1000k`.
        format!(
            "~{} / {}",
            loon_config::tokens::format_tokens(app.status.context_est),
            loon_config::tokens::format_tokens(app.context_limit)
        )
    } else {
        "compact off".into()
    };
    let queue = if app.queued.is_empty() {
        String::new()
    } else {
        if app.queue_paused {
            format!(" · {} paused (↑ edit, Enter resume)", app.queued.len())
        } else {
            format!(" · {} next request", app.queued.len())
        }
    };
    let text = if app.status.busy {
        format!(
            "{} · {} · {context}{queue}",
            app.request_rate.label(),
            format_elapsed(app.elapsed_secs())
        )
    } else {
        format!("{context} · {}{queue} ", app.settings_effort)
    };
    let right = vec![(text, theme.accent())];
    f.render_widget(
        Paragraph::new(Line::from(clip_spans(right, resources.width))).alignment(Alignment::Right),
        resources,
    );
}

/// Elapsed time in the status bar, coarsening as it grows so a long session still
/// reads at a glance: `45s`, `3m07s`, `2h05m`, `3d04h`. Seconds are dropped once
/// hours appear and minutes once days do — the bar is a glance, not a stopwatch.
fn format_elapsed(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if secs < MINUTE {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m{:02}s", secs / MINUTE, secs % MINUTE)
    } else if secs < DAY {
        format!("{}h{:02}m", secs / HOUR, (secs % HOUR) / MINUTE)
    } else {
        format!("{}d{:02}h", secs / DAY, (secs % DAY) / HOUR)
    }
}

/// Clip plain text to a cell width, ending with an ellipsis when it does not fit. Used for
/// one-row previews (queued prompts) where the terminal must not cut mid-word with no sign
/// that more text exists.
fn clip_cells(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            // Replace the last cell with the ellipsis so the row never overflows.
            while used + 1 > width {
                if out.pop().is_none() {
                    break;
                }
                used -= 1;
            }
            out.push('…');
            return out;
        }
        out.push(c);
        used += w;
    }
    out
}

/// Clip styled segments to the terminal width, ending with an ellipsis.
fn clip_spans(segments: Vec<(String, Style)>, width: u16) -> Vec<Span<'static>> {
    let width = width as usize;
    let mut out = Vec::new();
    let mut used = 0;
    for (text, style) in segments {
        if used >= width {
            break;
        }
        let mut shown = String::new();
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width {
                if used < width {
                    shown.push('…');
                    used += 1;
                }
                break;
            }
            shown.push(c);
            used += w;
        }
        if !shown.is_empty() {
            out.push(Span::styled(shown, style));
        }
        if used >= width {
            break;
        }
    }
    out
}

/// Rotating placeholder tips, one per ~6 s at 20 fps — Claude-Code's prompt feel.
fn placeholder_tip(tick: u64) -> &'static str {
    const TIPS: [&str; 6] = [
        "Try \"write a test for src/lib.rs\"",
        "Try \"explain the parser module\"",
        "Try \"fix the failing build\"",
        "/ for commands · shift+tab cycles permissions",
        "Try \"review my last change\"",
        "Try \"where is the config loaded?\"",
    ];
    TIPS[(tick as usize / 120) % TIPS.len()]
}

/// Apply terminal color-depth and UI-glyph preferences without changing transcript source text.
fn apply_terminal_preferences(f: &mut Frame, app: &App) {
    if !app.ascii_only && matches!(app.color_mode.as_str(), "auto" | "true") {
        return;
    }
    let area = f.area();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut f.buffer_mut()[(x, y)];
            if app.ascii_only {
                let symbol: String = cell
                    .symbol()
                    .chars()
                    .map(|c| match c {
                        '─' | '━' | '═' => '-',
                        '│' | '┃' | '║' => '|',
                        '\u{2500}'..='\u{257f}' => '+',
                        '›' | '❯' | '→' | '▶' => '>',
                        '←' | '◀' => '<',
                        '↑' => '^',
                        '↓' => 'v',
                        '•' | '●' | '○' | '◌' | '✦' | '✧' => '*',
                        '✓' | '✔' => '+',
                        '✗' | '✕' => 'x',
                        '…' => '.',
                        '█' | '▓' | '▒' | '░' => '#',
                        _ => c,
                    })
                    .collect();
                cell.set_symbol(&symbol);
            }
            cell.fg = terminal_color(cell.fg, &app.color_mode);
            cell.bg = terminal_color(cell.bg, &app.color_mode);
        }
    }
}
fn terminal_color(color: Color, mode: &str) -> Color {
    if mode == "mono" {
        return Color::Reset;
    }
    let rgb = match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(n) if mode == "c16" => {
            if n < 16 {
                return basic_color(n);
            }
            if n >= 232 {
                let v = 8 + (n - 232) * 10;
                Some((v, v, v))
            } else {
                let n = n - 16;
                let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
                Some((level(n / 36), level((n / 6) % 6), level(n % 6)))
            }
        }
        _ => None,
    };
    let Some((r, g, b)) = rgb else {
        return color;
    };
    if mode == "c256" {
        let level = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
        return Color::Indexed(16 + 36 * level(r) + 6 * level(g) + level(b));
    }
    if mode == "c16" {
        let colors = [
            (0, 0, 0),
            (170, 0, 0),
            (0, 170, 0),
            (170, 85, 0),
            (0, 0, 170),
            (170, 0, 170),
            (0, 170, 170),
            (170, 170, 170),
            (85, 85, 85),
            (255, 85, 85),
            (85, 255, 85),
            (255, 255, 85),
            (85, 85, 255),
            (255, 85, 255),
            (85, 255, 255),
            (255, 255, 255),
        ];
        let i = colors
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| {
                let d = |a: u8, b: i32| (a as i32 - b).pow(2);
                d(r, c.0) + d(g, c.1) + d(b, c.2)
            })
            .unwrap()
            .0;
        return basic_color(i as u8);
    }
    color
}
fn basic_color(n: u8) -> Color {
    [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ][(n % 16) as usize]
}
