//! Layout and drawing.
//!
//! Zones: animated status line, transcript (with text-selection highlighting),
//! (optional) slash-command popup above the input, input line, permission-mode footer.
//! The transparent invariant holds everywhere: every style comes from the theme, so
//! every background is `Color::Reset` and reverse video is never used; all animation is
//! foreground glyph cycling driven by `App::tick`, and selection highlight is
//! underline — never reverse video.

use crate::{theme, App};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph};
use ratatui::Frame;

const POPUP_MAX_ROWS: usize = 6;

/// Spinner verbs, cycled slowly while a turn runs — one word per ~4 s at 20 fps.
const VERBS: [&str; 5] = ["Thinking", "Working", "Forging", "Verifying", "Polishing"];

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let completions = app.visible_completions();
    let popup_rows = completions.len().min(POPUP_MAX_ROWS) as u16;
    let [status, transcript, popup, input, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(if popup_rows > 0 { popup_rows + 1 } else { 0 }), // +1 border row
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
    app.popup_rect = None;
    app.input_rect = Some((input.x + 1, input.y + 1));
    app.transcript_rect = Some((
        transcript.x,
        transcript.y,
        transcript.width,
        transcript.height,
    ));

    let theme = theme::Theme::from_mode(if app.transparent {
        loon_config::AppearanceMode::Transparent
    } else {
        loon_config::AppearanceMode::Opaque
    });

    // ── status line: animated while busy, quiet when idle ───────────────────────
    // idle: nothing (the banner already said who we are); busy: spinner only
    let status_spans: Vec<Span> = if app.status.busy {
        let elapsed = app.elapsed_secs();
        let verb = VERBS[(app.tick as usize / 80) % VERBS.len()];
        vec![Span::styled(
            format!("{} {}… {}s", app.spinner_glyph(), verb, elapsed),
            theme.accent_bold(),
        )]
    } else {
        vec![]
    };
    f.render_widget(
        Paragraph::new(Line::from(status_spans)).style(theme.base()),
        status,
    );

    // ── transcript, with selection highlight ────────────────────────────────────
    let lines = app.transcript_lines(transcript.width);
    let total = lines.len() as u16;
    let visible = transcript.height;
    let (start, end) = match app.scroll_from_bottom {
        None => (total.saturating_sub(visible), total),
        Some(off) => {
            let end = total.saturating_sub(off);
            (end.saturating_sub(visible), end)
        }
    };
    let sel = app.selection;
    let shown: Vec<Line> = lines[start as usize..end as usize]
        .iter()
        .enumerate()
        .map(|(i, line)| match sel {
            Some(sel) => {
                let abs = (start as usize + i) as u64;
                apply_selection(line, abs, &sel)
            }
            None => line.clone(),
        })
        .collect();
    f.render_widget(Paragraph::new(shown), transcript);

    // ── slash-command popup, directly above the input ───────────────────────────
    if popup_rows > 0 {
        app.popup_rect = Some((popup.x, popup.y, popup.height));
        let selected = app.completion_selected.min(completions.len() - 1);
        // window the list so the selection stays visible when there are more rows
        // than fit
        let start = selected
            .saturating_sub(POPUP_MAX_ROWS - 1)
            .min(completions.len().saturating_sub(popup_rows as usize));
        app.popup_window_start = start;
        let mut rows: Vec<Line> = Vec::new();
        for (i, c) in completions
            .iter()
            .skip(start)
            .take(popup_rows as usize)
            .enumerate()
        {
            let idx = start + i;
            let display = c.display.clone();
            let desc = c.description.clone();
            let row = if idx == selected {
                Line::from(vec![
                    Span::styled("› ", theme.menu_selected()),
                    Span::styled(format!("/{display}"), theme.menu_selected()),
                    Span::styled(format!("  {desc}"), theme.menu_selected()),
                ])
            } else {
                Line::from(vec![
                    Span::styled("  ", theme.dim()),
                    Span::styled(format!("/{display}"), theme.base()),
                    Span::styled(format!("  {desc}"), theme.dim()),
                ])
            };
            rows.push(row.style(theme.base()));
        }
        let block = RtBlock::default()
            .borders(Borders::TOP)
            .border_style(theme.border());
        f.render_widget(Paragraph::new(rows).block(block).style(theme.base()), popup);
    }

    // ── input: animated glyph while busy, accent caret when idle ────────────────
    let prompt = if app.status.busy {
        app.spinner_glyph().to_string()
    } else {
        "›".to_string()
    };
    let block = RtBlock::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(theme.border());
    let input_line = Line::from(vec![
        Span::styled(prompt.clone(), theme.accent_bold()),
        Span::styled(format!(" {}", app.input), theme.base()),
    ]);
    let para = Paragraph::new(input_line).block(block).style(theme.base());
    f.render_widget(para, input);

    // ── permission-mode footer ──────────────────────────────────────────────────
    let footer_text = permission_footer(&app.status.mode);
    let footer_style = if app.status.mode == "bypass" {
        theme.dim()
    } else {
        theme.ok()
    };
    f.render_widget(
        Paragraph::new(Line::styled(
            truncate_str(&footer_text, footer.width as usize),
            footer_style,
        )),
        footer,
    );

    // ── model picker overlay (bare /model) ──────────────────────────────────────
    if let Some((refs, selected)) = &app.model_picker {
        let rows_n = refs.len().min(10) as u16;
        let w = 48u16.min(area.width.saturating_sub(4));
        let h = (rows_n + 2).min(area.height.saturating_sub(4));
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let rect = ratatui::layout::Rect {
            x,
            y,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, rect);
        let mut rows: Vec<Line> = vec![Line::styled(
            " switch model — arrows · enter · esc ".to_string(),
            theme.accent_bold(),
        )];
        for (i, r) in refs.iter().take(rows_n as usize).enumerate() {
            let current = *r == app.status.model;
            if i == *selected {
                rows.push(Line::from(Span::styled(
                    format!("› {r}{}", if current { "  (current)" } else { "" }),
                    theme.menu_selected(),
                )));
            } else {
                rows.push(Line::from(Span::styled(
                    format!("  {r}"),
                    if current { theme.ok() } else { theme.base() },
                )));
            }
        }
        let block = RtBlock::default()
            .borders(Borders::ALL)
            .border_style(theme.border())
            .style(theme.base());
        f.render_widget(Paragraph::new(rows).block(block).style(theme.base()), rect);
    }

    // ── effort picker overlay (bare /effort) ────────────────────────────────────
    if let Some(selected) = app.effort_picker {
        let w = 36u16.min(area.width.saturating_sub(4));
        let h = 6u16.min(area.height.saturating_sub(4));
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let rect = ratatui::layout::Rect {
            x,
            y,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, rect);
        let mut rows: Vec<Line> = vec![Line::styled(
            " effort — arrows · enter · esc ".to_string(),
            theme.accent_bold(),
        )];
        for (i, level) in ["low — 2k output tokens", "medium — 8k", "high — 32k"]
            .iter()
            .enumerate()
        {
            if i == selected {
                rows.push(Line::from(Span::styled(
                    format!("› {level}"),
                    theme.menu_selected(),
                )));
            } else {
                rows.push(Line::from(Span::styled(format!("  {level}"), theme.base())));
            }
        }
        let block = RtBlock::default()
            .borders(Borders::ALL)
            .border_style(theme.border())
            .style(theme.base());
        f.render_widget(Paragraph::new(rows).block(block).style(theme.base()), rect);
    }

    // ── settings menu overlay (bare /config) ────────────────────────────────────
    if let Some(selected) = app.settings {
        let w = 56u16.min(area.width.saturating_sub(4));
        let h = 15u16.min(area.height.saturating_sub(4));
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let rect = ratatui::layout::Rect {
            x,
            y,
            width: w,
            height: h,
        };
        f.render_widget(ratatui::widgets::Clear, rect);
        let items: [String; 8] = [
            format!("permission mode: {}   (enter cycles)", app.status.mode),
            format!(
                "autocompact: {}   (enter toggles)",
                if app.settings_autocompact {
                    "on"
                } else {
                    "off"
                }
            ),
            format!(
                "effort: {}   (enter cycles low/medium/high)",
                app.settings_effort
            ),
            format!("max steps: {}   (enter cycles)", app.settings_max_steps),
            format!(
                "tool output cap: {}   (enter cycles)",
                if app.settings_tool_cap >= 262_144 {
                    "256k".to_string()
                } else if app.settings_tool_cap >= 65_536 {
                    "64k".to_string()
                } else {
                    format!("{}b", app.settings_tool_cap)
                }
            ),
            "theme: enter to cycle transparent/opaque".to_string(),
            "model…".to_string(),
            "providers list".to_string(),
        ];
        let mut rows: Vec<Line> = vec![Line::styled(" Settings ".to_string(), theme.accent_bold())];
        for (i, item) in items.iter().enumerate() {
            if i == selected {
                let pad = w
                    .saturating_sub(4)
                    .saturating_sub(item.chars().count() as u16);
                rows.push(Line::from(vec![
                    Span::styled(" › ", theme.menu_selected()),
                    Span::styled(item.clone(), theme.menu_selected()),
                    Span::styled(" ".repeat(pad as usize), theme.menu_selected()),
                ]));
            } else {
                rows.push(Line::from(vec![
                    Span::styled("   ", theme.dim()),
                    Span::styled(item.clone(), theme.base()),
                ]));
            }
        }
        let block = RtBlock::default()
            .borders(Borders::ALL)
            .border_style(theme.border())
            .style(theme.base());
        f.render_widget(Paragraph::new(rows).block(block).style(theme.base()), rect);
    }

    // text cursor at the recorded cursor position (hidden while busy streaming to
    // avoid flicker)
    if !app.status.busy {
        let x = (prompt.chars().count() + 1 + app.cursor)
            .min(input.width.saturating_sub(2) as usize) as u16;
        f.set_cursor_position((
            input.x + 1 + x.min(input.width.saturating_sub(2)),
            input.y + 1,
        ));
    }
}

/// Overlay the selection for one transcript line: the selected character range is
/// re-rendered underlined (fg-only; reverse video stays banned).
fn apply_selection(line: &Line<'static>, abs_line: u64, sel: &crate::SelRange) -> Line<'static> {
    let (sel_first_line, sel_first_col, sel_last_line, sel_last_col) =
        if sel.start_line <= sel.end_line {
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
    if abs_line < sel_first_line || abs_line > sel_last_line {
        return line.clone();
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let begin = if abs_line == sel_first_line {
        sel_first_col
    } else {
        0
    };
    let stop = if abs_line == sel_last_line {
        sel_last_col
    } else {
        text.chars().count()
    };
    if stop <= begin {
        return line.clone();
    }
    let chars: Vec<char> = text.chars().collect();
    let byte = |i: usize| -> usize {
        chars[..i.min(chars.len())]
            .iter()
            .map(|c| c.len_utf8())
            .sum()
    };
    let (b0, b1, b2) = (byte(begin), byte(stop), byte(chars.len()));
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

/// The footer glyph and hint live here once; labels are plain data per mode. Change
/// the symbol or the key hint by editing exactly one constant.
const MODE_GLYPH: &str = "⏵⏵";
const MODE_HINT: &str = "(shift+tab to cycle)";

fn permission_footer(mode: &str) -> String {
    let label = match mode {
        "bypass" => "bypass permissions on",
        "auto" => "auto review on — a reviewer agent checks calls, doubtful ones ask you",
        "ask" => "manual mode on — mutating calls ask first",
        "allowlist" => "allowlist mode on — only allowlisted tools run",
        other => return format!("{MODE_GLYPH} {other} {MODE_HINT}"),
    };
    format!("{MODE_GLYPH} {label} {MODE_HINT}")
}

fn truncate_str(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w.saturating_sub(1)).collect::<String>() + "…"
    }
}
