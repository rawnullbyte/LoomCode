//! Layout and drawing.
//!
//! Four zones: animated status line, transcript, (optional) slash-command popup above
//! the input, input line. The transparent invariant holds everywhere: every style comes
//! from the theme, so every background is `Color::Reset` and reverse video is never
//! used; all animation is foreground glyph cycling driven by `App::tick`.

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
    let [status, transcript, popup, input] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(if popup_rows > 0 { popup_rows + 1 } else { 0 }), // +1 border row
        Constraint::Length(3),
    ])
    .areas(area);
    app.popup_rect = None;
    app.input_rect = Some((input.x + 1, input.y + 1));

    let theme = theme::Theme::from_mode(if app.transparent {
        loon_config::AppearanceMode::Transparent
    } else {
        loon_config::AppearanceMode::Opaque
    });

    // ── status line: animated while busy, quiet when idle ───────────────────────
    let left = if app.status.busy {
        let elapsed = app.elapsed_secs();
        let verb = VERBS[(app.tick as usize / 80) % VERBS.len()];
        Span::styled(
            format!("{} {}… {}s", app.spinner_glyph(), verb, elapsed),
            theme.accent_bold(),
        )
    } else {
        Span::styled(" ready".to_string(), theme.ok())
    };
    let mut status_spans: Vec<Span> = vec![left];
    let info = format!(
        " · {}{}{} · {}",
        app.status.model,
        app.status
            .member
            .as_ref()
            .map(|m| format!(" (via {m})"))
            .unwrap_or_default(),
        if app.status.context_est > 0 {
            format!(" · ~{}k ctx", app.status.context_est / 1000)
        } else {
            String::new()
        },
        app.status.mode,
    );
    status_spans.push(Span::styled(
        truncate_str(&info, status.width as usize),
        theme.dim(),
    ));
    f.render_widget(
        Paragraph::new(Line::from(status_spans)).style(theme.base()),
        status,
    );

    // ── transcript ──────────────────────────────────────────────────────────────
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
    let shown = &lines[start as usize..end as usize];
    f.render_widget(Paragraph::new(shown.to_vec()), transcript);

    // ── slash-command popup, directly above the input ───────────────────────────
    if popup_rows > 0 {
        let selected = app.completion_selected.min(completions.len() - 1);
        // window the list so the selection stays visible when there are more rows
        // than fit
        let start = selected
            .saturating_sub(POPUP_MAX_ROWS - 1)
            .min(completions.len().saturating_sub(popup_rows as usize));
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
                    Span::styled("› ", theme.accent_bold()),
                    Span::styled(format!("/{display}"), theme.accent_bold()),
                    Span::styled(format!("  {desc}"), theme.dim()),
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
    let input_text = format!("{prompt} {}", app.input);
    let block = RtBlock::default()
        .borders(Borders::TOP)
        .border_style(theme.border());
    let mut input_line = Line::from(vec![
        Span::styled(prompt.clone(), theme.accent_bold()),
        Span::styled(format!(" {}", app.input), theme.base()),
    ]);
    input_line.style = theme.base();
    let para = Paragraph::new(input_line).block(block).style(theme.base());
    let _ = &input_text; // kept for cursor math documentation
    f.render_widget(para, input);

    // cursor position (hide while busy streaming to avoid flicker)
    if !app.status.busy {
        let x = (prompt.chars().count() + 1 + app.input.chars().count())
            .min(input.width.saturating_sub(2) as usize) as u16;
        f.set_cursor_position((
            input.x + 1 + x.min(input.width.saturating_sub(2)),
            input.y + 1,
        ));
    }
}

fn truncate_str(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w.saturating_sub(1)).collect::<String>() + "…"
    }
}
