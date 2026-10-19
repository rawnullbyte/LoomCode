//! Layout and drawing. Compact three-zone layout: status line, transcript, input.

use crate::{theme, App};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block as RtBlock, Borders, Paragraph};
use ratatui::Frame;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let [status, transcript, input] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(area);

    // ── status line ─────────────────────────────────────────────────────────────
    let theme = theme::Theme::from_mode(if app.transparent {
        loon_config::AppearanceMode::Transparent
    } else {
        loon_config::AppearanceMode::Opaque
    });
    let _busy = if app.status.busy { " · working" } else { "" };
    let ctx = if app.status.context_est > 0 {
        format!(" · ~{}k ctx", app.status.context_est / 1000)
    } else {
        String::new()
    };
    let status_text = format!(
        " {} · {}{}{} · {}",
        app.status.cwd,
        app.status.model,
        app.status
            .member
            .as_ref()
            .map(|m| format!(" (via {m})"))
            .unwrap_or_default(),
        ctx,
        app.status.mode
    );
    let status_line =
        ratatui::text::Line::styled(truncate_to(status_text, status.width as usize), theme.dim());
    f.render_widget(Paragraph::new(status_line), status);

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

    // ── input ───────────────────────────────────────────────────────────────────
    let prompt = if app.status.busy { "…" } else { "›" };
    let input_text = format!("{prompt} {}", app.input);
    let input_style = theme.base();
    let block = RtBlock::default()
        .borders(Borders::TOP)
        .border_style(theme.border());
    let para = Paragraph::new(ratatui::text::Line::styled(
        truncate_to(input_text, input.width.max(4) as usize - 1),
        input_style,
    ))
    .block(block);
    f.render_widget(para, input);

    // cursor position (hide while busy streaming to avoid flicker)
    let cursor_visible = !app.status.busy;
    if cursor_visible {
        let x = (prompt.chars().count() + 1 + app.input.chars().count())
            .min(input.width.saturating_sub(2) as usize) as u16;
        f.set_cursor_position((
            input.x + 1 + x.min(input.width.saturating_sub(2)),
            input.y + 1,
        ));
    }
}

fn truncate_to(s: String, w: usize) -> String {
    if s.chars().count() <= w {
        s
    } else {
        s.chars().take(w.saturating_sub(1)).collect::<String>() + "…"
    }
}
