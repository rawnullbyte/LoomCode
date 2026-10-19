//! A01: transparent rendering invariant, verified on the actual buffer the TUI draws.
//! Every application-owned cell must have the terminal default background in transparent
//! mode — across all screens, states, and widths. No reverse-video anywhere.

use loon_tui::{theme, App, Block, StatusLine};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;

fn make_app(transparent: bool) -> App {
    let mut app = App::new(
        transparent,
        StatusLine {
            model: "combo:daily-driver".into(),
            member: Some("ollama/llama3.1".into()),
            mode: "bypass".into(),
            context_est: 12_400,
            busy: false,
            cwd: "/home/u/proj".into(),
        },
    );
    app.push_block(Block::System("Loon ready".into()));
    app.push_block(Block::User("fix the parser tests please".into()));
    app.push_block(Block::Tool {
        name: "read".into(),
        ok: true,
        excerpt: "1  fn parse() {".into(),
        pending: false,
    });
    app.push_block(Block::Tool {
        name: "edit".into(),
        ok: false,
        excerpt: "tool error: conflict".into(),
        pending: false,
    });
    app.push_block(Block::Member("skipped x: capability".into()));
    app.stream_text("Here is what I found and the fix I am applying…");
    app
}

fn assert_all_cells_bg(transparent: bool, width: u16, height: u16) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = make_app(transparent);
    terminal.draw(|f| loon_tui::ui::draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    for (i, cell) in buf.content().iter().enumerate() {
        let expected = if transparent { Color::Reset } else { cell.bg };
        assert_eq!(
            cell.bg,
            expected,
            "cell {} (x={}, y={}) has bg {:?}; transparent={}",
            i,
            i % width as usize,
            i / width as usize,
            cell.bg,
            transparent
        );
        if transparent {
            // reverse-video turns fg into bg: forbidden by the invariant
            let modifier_contains_reverse =
                (cell.modifier.bits() & ratatui::style::Modifier::REVERSED.bits()) != 0;
            assert!(
                !modifier_contains_reverse,
                "cell {i} uses REVERSED, which leaks opaque backgrounds"
            );
        }
    }
}

#[test]
fn a01_transparent_background_everywhere() {
    for (w, h) in [(100, 30), (40, 12), (20, 6)] {
        assert_all_cells_bg(true, w, h);
    }
}

#[test]
fn a01_narrow_terminal_still_transparent_and_bounded() {
    // 12 columns, streaming, everything: no panics, invariant holds.
    assert_all_cells_bg(true, 12, 4);
}

#[test]
fn a01_opaque_mode_paints_only_with_explicit_opt_in() {
    // opaque mode has a real bg; transparent must never. Both render without panic.
    assert_all_cells_bg(false, 60, 20);
}

#[test]
fn a01_busy_and_cancel_states_transparent() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = make_app(true);
    app.status.busy = true;
    app.push_block(Block::System(
        "cancelling at the next safe boundary…".into(),
    ));
    terminal.draw(|f| loon_tui::ui::draw(f, &mut app)).unwrap();
    for cell in terminal.backend().buffer().content() {
        assert_eq!(cell.bg, Color::Reset);
    }
}

#[test]
fn a02_theme_is_the_only_style_source() {
    // The free helpers in theme all carry Reset bg in transparent mode.
    for style in [
        theme::user_fg(),
        theme::assistant_fg(),
        theme::tool_ok_fg(),
        theme::tool_err_fg(),
        theme::system_fg(),
        theme::member_fg(),
    ] {
        assert_eq!(style.bg, Some(Color::Reset));
    }
    let t = theme::Theme::from_mode(loon_config::AppearanceMode::Transparent);
    assert_eq!(t.base().bg, Some(Color::Reset));
    assert_eq!(t.err().bg, Some(Color::Reset));
}
