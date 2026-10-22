//! The prompt selection must be painted, not merely computed: a Shift selection that
//! only exists in `App` state would look like a dead key to the user.

use loon_tui::{App, StatusLine};
use ratatui::backend::TestBackend;
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

fn draw(app: &mut App) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|f| loon_tui::ui::draw(f, app)).unwrap();
    terminal
}

/// Background colour of each cell on a row, as the terminal would paint it.
fn row_bgs(t: &Terminal<TestBackend>, y: u16, width: u16) -> Vec<ratatui::style::Color> {
    (0..width)
        .map(|x| {
            t.backend()
                .buffer()
                .cell((x, y))
                .map(|c| c.bg)
                .unwrap_or(ratatui::style::Color::Reset)
        })
        .collect()
}

#[test]
fn a_shift_selection_paints_a_highlight_over_the_prompt_text() {
    let mut a = app();
    a.input = "alpha beta".into();
    a.cursor = 10;
    let plain = draw(&mut a);
    // The app records the input text row during the draw; using it keeps this test
    // honest about where the prompt actually is.
    let (_, input_y) = a.input_rect.expect("the input row is recorded");
    let before = row_bgs(&plain, input_y, 60);

    // Select "beta" (6..10) the way Shift+Ctrl+Left does.
    a.input_anchor = Some(6);
    let selected = draw(&mut a);
    let after = row_bgs(&selected, input_y, 60);

    let changed: Vec<usize> = (0..60).filter(|&x| before[x] != after[x]).collect();
    assert!(
        !changed.is_empty(),
        "the selection must change at least one cell's background"
    );
    // The highlight covers exactly the selected characters, not the whole row.
    assert_eq!(
        changed.len(),
        4,
        "only the four selected characters should be highlighted, got cells {changed:?}"
    );
    // "beta" starts after the prompt glyph, its space, and "alpha " — 8 cells in.
    assert_eq!(
        changed,
        vec![8, 9, 10, 11],
        "the highlight must sit over the selected text"
    );
}

#[test]
fn without_a_selection_the_prompt_is_unhighlighted() {
    let mut a = app();
    a.input = "alpha beta".into();
    a.cursor = 4;
    assert_eq!(a.input_selection(), None, "a caret alone selects nothing");
    let t = draw(&mut a);
    let (_, input_y) = a.input_rect.expect("the input row is recorded");
    // No cell may carry the transcript selection background.
    let sel = loon_tui::theme::selection_fg();
    let painted = row_bgs(&t, input_y, 60)
        .into_iter()
        .filter(|bg| sel.bg.is_some() && *bg == sel.bg.unwrap_or(ratatui::style::Color::Reset))
        .count();
    assert_eq!(painted, 0, "no selection, no highlight");
}
