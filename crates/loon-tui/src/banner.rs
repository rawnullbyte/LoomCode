//! The opening banner: a large gradient ASCII wordmark, left-aligned in the transcript.
//! It is a normal transcript block, so it scrolls away as the conversation grows
//! instead of occupying a fixed row.
//!
//! The colour sweep is derived from `App::tick`, so a frame is a pure function of the
//! clock — no per-frame randomness, no jitter when the terminal redraws.

use crate::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Big wordmark (ANSI-shadow style). Every row is exactly the same width.
const WORDMARK: [&str; 6] = [
    "██╗      ██████╗  ██████╗ ███╗   ███╗",
    "██║     ██╔═══██╗██╔═══██╗████╗ ████║",
    "██║     ██║   ██║██║   ██║██╔████╔██║",
    "██║     ██║   ██║██║   ██║██║╚██╔╝██║",
    "███████╗╚██████╔╝╚██████╔╝██║ ╚═╝ ██║",
    "╚══════╝ ╚═════╝  ╚═════╝ ╚═╝     ╚═╝",
];

/// Left inset, in cells, before the wordmark.
const INSET: usize = 2;

/// The banner as transcript lines: art, then version and workspace.
pub fn lines(tick: u64, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    if w == 0 {
        return Vec::new();
    }
    let big_w = WORDMARK[0].chars().count();
    // Too narrow for the big art: one line keeps it readable.
    let art: Vec<&str> = if w >= big_w + INSET {
        WORDMARK.to_vec()
    } else {
        vec!["L O O M"]
    };
    let art_w = art[0].chars().count();
    let mut out: Vec<Line<'static>> = art
        .iter()
        .enumerate()
        .map(|(row, text)| wordmark_row(text, row, w, tick))
        .collect();
    // The version sits under the wordmark, centred against its width (not the screen).
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let pad = INSET + art_w.saturating_sub(version.chars().count()) / 2;
    out.push(Line::styled(
        format!("{}{}", " ".repeat(pad), version),
        theme::system_fg(),
    ));
    out.push(Line::default());
    out
}

/// One wordmark row: the glyphs with a slow colour sweep travelling across the columns.
fn wordmark_row(text: &str, row: usize, width: usize, tick: u64) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" ".repeat(INSET), theme::base_style())];
    for (col, ch) in text.chars().enumerate() {
        if INSET + col >= width {
            break;
        }
        if ch == ' ' {
            spans.push(Span::styled(" ", theme::base_style()));
            continue;
        }
        let hue = (0.47 + col as f32 * 0.0075 + (tick as f32) * 0.0009 + row as f32 * 0.01) % 1.0;
        let color = theme::hsv_to_rgb(hue, 0.55, 1.0);
        spans.push(Span::styled(
            ch.to_string(),
            Style::default()
                .fg(color)
                .bg(Color::Reset)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let mut line = Line::from(spans);
    line.style = theme::base_style();
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(tick: u64, width: u16) -> String {
        lines(tick, width)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_wordmark_row_has_the_same_width() {
        let width = WORDMARK[0].chars().count();
        for row in WORDMARK {
            assert_eq!(row.chars().count(), width, "ragged row: {row}");
        }
    }

    #[test]
    fn wide_terminals_get_the_big_wordmark() {
        let out = rendered(0, 80);
        assert!(out.contains("██████╗"), "{out}");
        assert!(
            out.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "{out}"
        );
    }

    #[test]
    fn narrow_terminals_fall_back_to_one_line() {
        let out = rendered(0, 20);
        assert!(out.contains("L O O M"), "{out}");
        assert!(!out.contains("██████╗"), "{out}");
    }

    #[test]
    fn the_version_is_centred_under_the_wordmark() {
        let lines = lines(0, 80);
        let art_w = WORDMARK[0].chars().count();
        let version = lines[WORDMARK.len()].to_string();
        let pad = version.len() - version.trim_start().len();
        let expected = INSET + art_w.saturating_sub(version.trim().chars().count()) / 2;
        assert_eq!(pad, expected, "version centred under the logo: {version:?}");
        assert!(version.trim().starts_with('v'), "{version:?}");
    }

    #[test]
    fn the_wordmark_starts_at_the_left_edge() {
        let first = lines(0, 80)[0].to_string();
        assert!(
            first.starts_with(&" ".repeat(INSET)) && first.trim_start().starts_with('█'),
            "left-aligned wordmark: {first:?}"
        );
    }

    #[test]
    fn the_gradient_moves_with_the_clock() {
        let first: String = lines(0, 80).iter().map(|l| l.to_string()).collect();
        let later: String = lines(40, 80).iter().map(|l| l.to_string()).collect();
        assert_eq!(first, later, "text is stable; only colour animates");
    }
}
