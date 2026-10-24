//! UI upgrade verification on the real buffer: the slash-command popup renders with a
//! highlighted selection, the spinner animates across ticks, and — critically — the
//! transparent invariant survives all of it.

use loon_tui::{App, Block, StatusLine};
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
    // layout: 6 borderless rows above the 3-row input + 1 footer row
    let rows: Vec<String> = (0..6)
        .map(|i| row_text(&t, 24 - 3 - 1 - 6 + i, 80))
        .collect();
    let joined = rows.join("\n");
    assert!(joined.contains("/help"), "{joined}");
    assert!(joined.contains("/undo"), "{joined}");
    assert!(joined.contains("/ultraloop"), "{joined}");
    // capped at 6 visible rows: /compact (7th) is windowed out, not rendered
    assert!(!joined.contains("/compact"), "cap must hold: {joined}");
    // the first row is the selected one: caret + bold
    assert!(
        rows[0].starts_with('❯'),
        "selected row [CC]-style marker: {}",
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
    let rows: Vec<String> = (0..5)
        .map(|i| row_text(&t, 24 - 3 - 1 - 5 + i, 80))
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

/// Argument values are not slash commands. The popup used to prefix every row with
/// `/`, so `/autocompact` offered `/90%` and Tab looked like it had replaced the
/// command with a value.
#[test]
fn argument_values_are_shown_without_a_slash() {
    let mut a = app();
    a.input = "/autocompact".into();
    let t = draw(&mut a);
    let screen = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("90%"),
        "the value must be offered: {screen}"
    );
    assert!(
        !screen.contains("/90%"),
        "an argument value must never render as a slash command: {screen}"
    );
    // Tab keeps the command and appends the value.
    let idx = a
        .visible_completions()
        .iter()
        .position(|c| c.insert == "90%")
        .expect("90% is offered for /autocompact");
    a.completion_selected = idx;
    assert!(a.accept_completion());
    assert_eq!(a.input, "/autocompact 90% ");
}

/// A queued prompt is shown as a dim row above the prompt bar, so what is waiting is
/// visible rather than only counted in the status bar.
#[test]
fn a_queued_prompt_is_previewed_above_the_input() {
    let mut a = app();
    a.queued.push("summarize the diff".into());
    let t = draw(&mut a);
    let screen = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("summarize the diff"),
        "the queued prompt must be visible: {screen}"
    );
    // It sits above the prompt bar, not inside the transcript.
    let (_, input_y) = a.input_rect.expect("the input row is recorded");
    let queue_row = (0..input_y)
        .find(|y| row_text(&t, *y, 80).contains("summarize the diff"))
        .expect("a row above the input holds the queued prompt");
    assert!(
        queue_row < input_y,
        "queue preview must sit above the input"
    );
    // Dim, not the normal prompt colour.
    let buf = t.backend().buffer();
    let painted: Vec<ratatui::style::Color> = (0..80)
        .map(|x| buf.cell((x, queue_row)).unwrap().fg)
        .filter(|c| *c != ratatui::style::Color::Reset)
        .collect();
    assert!(
        !painted.is_empty(),
        "the queue row must be painted in the dim colour"
    );
}

#[test]
fn only_the_newest_queued_prompts_fit_and_older_ones_scroll_off() {
    let mut a = app();
    for n in 1..=5 {
        a.queued.push(format!("queued {n}"));
    }
    let t = draw(&mut a);
    let screen = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    // The newest rows stay adjacent to the prompt; the oldest is dropped.
    assert!(screen.contains("queued 5"), "{screen}");
    assert!(screen.contains("queued 4"), "{screen}");
    assert!(
        !screen.contains("queued 1"),
        "oldest must be windowed out: {screen}"
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
    // The status bar (bottom row) carries elapsed seconds and throughput; the header
    // above it is the wordmark only.
    let t = draw(&mut a);
    let status = row_text(&t, 23, 80);
    assert!(status.contains("0s"), "elapsed seconds shown: {status}");
    assert!(status.contains("t/s"), "throughput shown: {status}");

    // idle: the status bar drops the timing readouts and shows readiness
    a.status.busy = false;
    a.turn_started = None;
    let t2 = draw(&mut a);
    let idle = row_text(&t2, 23, 80);
    assert!(idle.contains("Ready"), "idle status: {idle:?}");
    assert!(!idle.contains("tok/s"), "no throughput when idle: {idle:?}");
}

#[test]
fn settings_menu_navigation_and_toggles() {
    use loon_core::catalog::MenuItem;
    use loon_tui::manager::{Navigator, View};
    let mut a = app();
    a.navigator = Some(Navigator::new(
        Default::default(),
        View::Menu(
            "Settings".into(),
            vec![MenuItem {
                label: "effort".into(),
                detail: "coding effort".into(),
                command: "/effort".into(),
            }],
        ),
    ));
    let t = draw(&mut a);
    let screen = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("Settings"));
    assert!(screen.contains("coding effort"));
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

#[test]
fn argument_completion_keeps_the_command() {
    // "/config ti" + Tab must yield "/config tips ", never "/tips ".
    let mut a = app();
    a.input = "/config ti".into();
    assert!(a.accept_completion());
    assert_eq!(a.input, "/config tips ");
    // and the value suggestions then appear for that key
    let values = a.visible_completions();
    let names: Vec<&str> = values.iter().map(|c| c.insert.as_str()).collect();
    assert_eq!(names, vec!["true", "false"], "{names:?}");
    // accepting a value replaces only the value word
    a.input = "/config permission_mode aut".into();
    assert!(a.accept_completion());
    assert_eq!(a.input, "/config permission_mode auto ");
}

#[test]
fn banner_is_a_transcript_block_that_scrolls_away() {
    let mut a = app();
    a.push_welcome_banner();
    let t = draw(&mut a);
    let fresh = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        fresh.contains("██████╗") || fresh.contains("L O O M"),
        "wordmark drawn: {fresh}"
    );
    assert!(
        fresh.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "{fresh}"
    );
    assert!(
        !fresh.contains("/model · /providers"),
        "the banner carries no command list: {fresh}"
    );

    // Enough chat pushes the banner off the top of the screen.
    for n in 0..40 {
        a.blocks.push(Block::Assistant(format!("message {n}")));
    }
    let t = draw(&mut a);
    let scrolled = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !scrolled.contains("██████╗") && !scrolled.contains("L O O M"),
        "banner scrolled away: {scrolled}"
    );
    assert!(scrolled.contains("message 39"), "{scrolled}");
}

#[test]
fn the_banner_animates_between_frames() {
    let mut a = app();
    a.push_welcome_banner();
    let colours = |app: &App| -> Vec<Option<ratatui::style::Color>> {
        app.transcript_lines(80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.style.fg))
            .collect()
    };
    let first = colours(&a);
    for _ in 0..12 {
        a.tick();
    }
    let later = colours(&a);
    assert_eq!(
        first.len(),
        later.len(),
        "the wordmark itself does not change shape"
    );
    assert_ne!(first, later, "the gradient sweeps as the clock advances");
}

#[test]
fn only_the_selected_ultra_row_shimmers_like_the_logo() {
    use loon_core::catalog::MenuItem;
    use loon_tui::manager::{Navigator, View};

    let items: Vec<MenuItem> = loon_config::Effort::LEVELS
        .into_iter()
        .map(|n| MenuItem {
            label: n.into(),
            detail: "effort".into(),
            command: format!("/effort {n}"),
        })
        .collect();
    let ultra_index = items.iter().position(|i| i.label == "ultra").unwrap();

    let row_of = |a: &mut App, needle: &str| -> Vec<ratatui::buffer::Cell> {
        let t = draw(a);
        let y = (0..24)
            .find(|y| row_text(&t, *y, 80).contains(needle))
            .unwrap_or_else(|| panic!("row {needle} drawn"));
        let buf = t.backend().buffer();
        (0..80)
            .map(|x| buf.cell((x, y)).cloned().unwrap())
            .collect()
    };
    let word = |cells: &[ratatui::buffer::Cell], n: usize| -> Vec<ratatui::style::Color> {
        let chars: Vec<String> = cells.iter().map(|c| c.symbol().to_string()).collect();
        let start = chars
            .windows(n)
            .position(|w| {
                w.iter()
                    .all(|c| c.chars().all(|ch| ch.is_ascii_alphabetic()))
            })
            .unwrap_or(0);
        (start..start + n).map(|i| cells[i].fg).collect()
    };

    // Cursor on ultra: its glyphs carry several distinct hues, and the band travels.
    let mut a = app();
    a.animations = true;
    let mut nav = Navigator::new(
        Default::default(),
        View::Menu("Coding effort".into(), items.clone()),
    );
    nav.selected = ultra_index;
    a.navigator = Some(nav);
    a.tick = 0;
    let first = word(&row_of(&mut a, "ultra"), 5);
    a.tick = 40;
    let later = word(&row_of(&mut a, "ultra"), 5);
    assert!(
        first.windows(2).any(|w| w[0] != w[1]),
        "the wordmark gradient varies across the glyphs: {first:?}"
    );
    assert_ne!(first, later, "the gradient travels over time");

    // A different row (cursor on `low`) is plain: no shimmer anywhere else.
    let mut b = app();
    b.animations = true;
    let mut nav = Navigator::new(
        Default::default(),
        View::Menu("Coding effort".into(), items.clone()),
    );
    nav.selected = 0;
    b.navigator = Some(nav);
    b.tick = 0;
    let low = word(&row_of(&mut b, "low"), 3);
    b.tick = 40;
    let low_later = word(&row_of(&mut b, "low"), 3);
    assert_eq!(low, low_later, "only the selected ultra row animates");
    assert_eq!(low[0], low[2], "an unselected row keeps one flat colour");

    // Animations off: ultra is static too.
    a.animations = false;
    a.tick = 0;
    let still = word(&row_of(&mut a, "ultra"), 5);
    a.tick = 40;
    let still_later = word(&row_of(&mut a, "ultra"), 5);
    assert_eq!(still, still_later, "no motion when animations are disabled");

    // No popup background is painted in transparent mode (the ripple is gone).
    let t = draw(&mut a);
    let buf = t.backend().buffer();
    for (x, y) in (0..24).flat_map(|y| (0..80).map(move |x| (x, y))) {
        assert_eq!(
            buf.cell((x, y)).unwrap().bg,
            ratatui::style::Color::Reset,
            "transparent background at ({x},{y})"
        );
    }
}

#[test]
fn other_popups_do_not_shimmer() {
    use loon_core::catalog::MenuItem;
    use loon_tui::manager::{Navigator, View};
    // A settings menu whose selected row happens to be labelled "ultra" must stay plain:
    // the shimmer belongs to the effort selector only.
    let mut a = app();
    a.animations = true;
    let mut nav = Navigator::new(
        Default::default(),
        View::Menu(
            "Settings".into(),
            vec![MenuItem {
                label: "ultra".into(),
                detail: "not the effort picker".into(),
                command: "/config ultra".into(),
            }],
        ),
    );
    nav.selected = 0;
    a.navigator = Some(nav);
    let colors = |a: &mut App| -> Vec<ratatui::style::Color> {
        let t = draw(a);
        let y = (0..24)
            .find(|y| row_text(&t, *y, 80).contains("ultra"))
            .unwrap();
        let buf = t.backend().buffer();
        (0..80).map(|x| buf.cell((x, y)).unwrap().fg).collect()
    };
    a.tick = 0;
    let first = colors(&mut a);
    a.tick = 40;
    assert_eq!(
        first,
        colors(&mut a),
        "no shimmer outside the effort picker"
    );
}

/// A long session must not get slower to interact with. Frame cost used to grow linearly
/// with the number of transcript blocks — 24 ms per frame at 2 000 blocks, on a 50 ms tick,
/// which is the "laggy after a few hours" report — because every frame re-lexed every block
/// while only ~30 rows are on screen. With the row cache the steady-state frame is flat
/// across session lengths, and a new message costs one rebuild.
#[test]
fn frame_cost_is_flat_across_session_lengths() {
    fn build(blocks: usize) -> App {
        let mut a = app();
        a.push_welcome_banner();
        for i in 0..blocks {
            a.blocks.push(Block::Assistant(format!(
                "message {i} with enough text to wrap at this width and exercise the renderer"
            )));
        }
        a
    }
    let cost = |a: &mut App| {
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| loon_tui::ui::draw(f, a)).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..30 {
            t.draw(|f| loon_tui::ui::draw(f, a)).unwrap();
        }
        start.elapsed() / 30
    };
    let short = cost(&mut build(50));
    let long = cost(&mut build(3_000));
    // 60× more content, but the cached frame must stay within a small constant factor —
    // generous because the machine load varies; the old code was ~40× slower here.
    assert!(
        long.as_nanos() < short.as_nanos() * 6 + 1_000_000,
        "a 3 000-block session must not render meaningfully slower than 50: short={short:?} long={long:?}"
    );

    // And the window actually moves: the newest message is on screen, the first is not.
    let mut a = build(3_000);
    let t = draw(&mut a);
    let scrolled = (0..24)
        .map(|y| row_text(&t, y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(scrolled.contains("message 2999"), "{scrolled}");
    assert!(!scrolled.contains("message 1 "), "{scrolled}");
}

/// A finished tool replaces its pending block *in place*, which keeps the block count
/// identical — so a length-only cache key would keep showing "running…" after the result
/// had arrived. The generation must catch it.
#[test]
fn a_finished_tool_replaces_its_pending_row() {
    let mut a = app();
    a.tool_started("bash", "{\"command\": \"cargo test\"}", "$ cargo test");
    let before = (0..24)
        .map(|y| row_text(&draw(&mut a), y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(before.contains("running…"), "pending first: {before}");

    a.tool_finished("bash", true, "12 passed · 0 failed");
    let after = (0..24)
        .map(|y| row_text(&draw(&mut a), y, 80))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        after.contains("12 passed · 0 failed"),
        "the result must appear: {after}"
    );
    assert!(
        !after.contains("running…"),
        "the pending row must not survive the finish: {after}"
    );
}
