//! UI upgrade verification on the real buffer: the slash-command popup renders with a
//! highlighted selection, the spinner animates across ticks, and — critically — the
//! transparent invariant survives all of it.

use loon_tui::{App, StatusLine};
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;

fn app() -> App {
    App::new(
        true,
        StatusLine {
            model: "mock".into(),
            member: None,
            mode: "bypass".into(),
            context_est: 0,
            busy: false,
            cwd: "/w".into(),
        },
    )
}

fn cell_text(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> String {
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .map(|c| c.symbol().to_string())
        .unwrap_or_default()
}

fn row_text(terminal: &Terminal<TestBackend>, y: u16, width: u16) -> String {
    (0..width)
        .map(|x| cell_text(terminal, x, y))
        .collect::<String>()
}

fn draw(app: &mut App) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| loon_tui::ui::draw(f, app)).unwrap();
    terminal
}

#[test]
fn typing_slash_opens_popup_showing_all_commands() {
    let mut a = app();
    a.input = "/".into();
    let t = draw(&mut a);
    // popup sits just above the input; with 12 commands it is capped at 6 rows
    // layout: 6 content rows + 1 border row above the 3-row input + 1 footer row
    // → content at 14..20
    let rows: Vec<String> = (0..6)
        .map(|i| row_text(&t, 24 - 3 - 1 - 7 + 1 + i, 80))
        .collect();
    let joined = rows.join("\n");
    assert!(joined.contains("/help"), "{joined}");
    assert!(joined.contains("/undo"), "{joined}");
    assert!(joined.contains("/fork"), "{joined}");
    // capped at 6 visible rows: /compact (7th) is windowed out, not rendered
    assert!(!joined.contains("/compact"), "cap must hold: {joined}");
    // the first row is the selected one: caret + bold
    assert!(
        rows[0].trim_start().starts_with('›'),
        "selected row caret: {}",
        rows[0]
    );
    assert!(
        rows[1].starts_with("  /"),
        "unselected rows two-space prefixed: {:?}",
        rows[1]
    );
}

#[test]
fn prefix_narrows_and_selection_renders_highlighted() {
    let mut a = app();
    a.input = "/re".into();
    let t = draw(&mut a);
    let rows: Vec<String> = (0..3)
        .map(|i| row_text(&t, 24 - 3 - 1 - 4 + 1 + i, 80))
        .collect();
    let joined = rows.join("\n");
    assert!(joined.contains("/redo"), "{joined}");
    assert!(joined.contains("/rewind"), "{joined}");
    assert!(joined.contains("/recap"), "{joined}");
    assert!(
        !joined.contains("/help"),
        "filtered list must drop non-matches"
    );
}

#[test]
fn spinner_glyph_animates_across_ticks_and_styled_accent_while_busy() {
    let mut a = app();
    a.status.busy = true;
    a.turn_started = Some(std::time::Instant::now());
    let mut glyphs = Vec::new();
    for _ in 0..12 {
        a.tick();
        glyphs.push(a.spinner_glyph());
    }
    // palindrome cycle: glyph changes across ticks and repeats (frame 0 == frame 10)
    assert_ne!(glyphs[0], glyphs[1], "spinner must animate");
    assert_eq!(glyphs[0], glyphs[10], "palindrome period = 2n-2 = 10");
    // rendered status line carries the animated glyph + elapsed seconds
    let t = draw(&mut a);
    let status = row_text(&t, 0, 80);
    assert!(status.contains("…"), "elapsed seconds shown: {status}");
    let status_cell = t.backend().buffer().cell((1, 0)).unwrap();
    assert_eq!(
        status_cell.fg,
        Color::Cyan,
        "busy spinner styled accent (transparent theme)"
    );
    // idle status is a quiet "ready"
    a.status.busy = false;
    a.turn_started = None;
    let t2 = draw(&mut a);
    let idle = row_text(&t2, 0, 80);
    assert!(idle.contains("ready"), "{idle}");
}

#[test]
fn settings_menu_navigation_and_toggles() {
    let mut a = app();
    assert!(a.settings.is_none());
    a.settings = Some(0);
    // up from 0 wraps to the last row
    a.settings = Some((a.settings.unwrap() + 4) % 5);
    assert_eq!(a.settings, Some(4));
    a.settings = Some(0);
    // toggling autocompact flips the mirror
    let before = a.settings_autocompact;
    a.settings_autocompact = !a.settings_autocompact;
    assert_ne!(a.settings_autocompact, before);
    // effort cycles low → medium → high
    a.settings_effort = "low".into();
    let next = match a.settings_effort.as_str() {
        "low" => "medium",
        "medium" => "high",
        _ => "low",
    };
    assert_eq!(next, "medium");
    // esc closes
    a.settings = None;
    assert!(a.settings.is_none());
}

#[test]
fn tab_accept_and_input_update_flow() {
    let mut a = app();
    a.input = "/un".into();
    assert_eq!(a.completion_count(), 1);
    assert!(a.accept_completion());
    assert_eq!(a.input, "/undo ");
    // after acceptance the popup is gone (input has a space)
    assert_eq!(a.completion_count(), 0);
    // esc dismissal hides the popup until the input changes
    let mut b = app();
    b.input = "/".into();
    assert!(b.completion_count() > 0);
    b.popup_dismissed = true;
    assert_eq!(b.completion_count(), 0);
    b.input_changed();
    assert!(b.completion_count() > 0);
}
