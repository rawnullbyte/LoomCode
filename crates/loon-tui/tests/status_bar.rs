//! Status bar: the elapsed clock must coarsen past minutes, and the context figure
//! must track a running turn instead of freezing at the size the turn started with.

use loon_tui::{App, StatusLine};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn app(busy: bool) -> App {
    App::new(
        false,
        StatusLine {
            model: "mock".into(),
            member: None,
            mode: "ask".into(),
            context_est: 0,
            busy,
            cwd: "/tmp/proj".into(),
        },
    )
}

/// Render one frame and return every row as text.
fn frame(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| loon_tui::ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect()
}

fn footer(app: &mut App) -> String {
    footer_at(app, 120)
}

fn footer_at(app: &mut App, width: u16) -> String {
    let rows = frame(app, width, 20);
    rows.last().cloned().unwrap_or_default()
}

#[test]
fn the_clock_shows_hours_and_days_not_endless_minutes() {
    // 45s, 3m07s, 2h05m, 3d04h — the bar coarsens as the session grows.
    for (secs, want) in [
        (0u64, "0s"),
        (45, "45s"),
        (59, "59s"),
        (60, "1m00s"),
        (187, "3m07s"),
        (3599, "59m59s"),
        (3600, "1h00m"),
        (7500, "2h05m"),
        (86_399, "23h59m"),
        (86_400, "1d00h"),
        (273_600, "3d04h"),
    ] {
        let mut a = app(true);
        a.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(secs));
        let bar = footer(&mut a);
        assert!(
            bar.contains(want),
            "elapsed {secs}s must render as {want:?} in the status bar, got: {bar:?}"
        );
    }
}

#[test]
fn the_context_figure_moves_while_a_turn_is_running() {
    // The defect: the figure was only refreshed when the turn ended, so a long turn
    // showed the size it started with. Here the context grows mid-turn and the
    // rendered bar must follow.
    let mut a = app(true);
    a.turn_started = Some(std::time::Instant::now());
    a.context_limit = 200_000;
    a.status.context_est = 12_000;
    let before = footer(&mut a);
    assert!(
        before.contains("12k") || before.contains("12,000"),
        "sanity: the first frame shows the starting size, got: {before:?}"
    );

    // Mid-turn growth, as delivered by AgentEvent::ContextUpdated.
    a.status.context_est = 41_000;
    let after = footer(&mut a);
    assert!(
        after.contains("41k") || after.contains("41,000"),
        "a running turn must show the new context size, got: {after:?}"
    );
    assert!(
        !after.contains("12k") && !after.contains("12,000"),
        "the stale size must be gone, got: {after:?}"
    );
}

#[test]
fn the_busy_bar_shows_context_time_and_queue_together() {
    let mut a = app(true);
    a.context_limit = 200_000;
    a.status.context_est = 41_000;
    a.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(7500));
    a.queued.push("next".into());
    // Wide enough that the whole right-hand cluster fits instead of being clipped.
    let bar = footer_at(&mut a, 200);
    assert!(bar.contains("2h05m"), "time: {bar:?}");
    assert!(
        bar.contains("41k") || bar.contains("41,000"),
        "context: {bar:?}"
    );
    assert!(bar.contains("1 next request"), "queue: {bar:?}");
}

#[test]
fn an_idle_bar_still_reports_the_context_size() {
    let mut a = app(false);
    a.context_limit = 200_000;
    a.status.context_est = 41_000;
    let bar = footer(&mut a);
    assert!(bar.contains("41k") || bar.contains("41,000"), "{bar:?}");
}

mod subagent_transcript {
    //! A background subagent's progress and report must land in the transcript, so the
    //! user sees what their agents did without asking the model to list them.

    use loon_tui::{App, Block, StatusLine};

    fn app() -> App {
        App::new(
            false,
            StatusLine {
                model: "mock".into(),
                member: None,
                mode: "ask".into(),
                context_est: 0,
                busy: true,
                cwd: "/tmp/proj".into(),
            },
        )
    }

    fn system_texts(app: &App) -> Vec<String> {
        app.blocks
            .iter()
            .filter_map(|b| match b {
                Block::System(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_finished_report_is_rendered_with_status_and_work() {
        let mut a = app();
        a.push_block(Block::System(
            "background #3 scout completed\nthe parser has an off-by-one".to_string(),
        ));
        let texts = system_texts(&a);
        let report = texts.last().expect("a block must be pushed");
        assert!(report.contains("#3"), "the handle: {report}");
        assert!(report.contains("scout"), "the label: {report}");
        assert!(report.contains("completed"), "the status: {report}");
        assert!(
            report.contains("the parser has an off-by-one"),
            "the work it did: {report}"
        );
    }

    #[test]
    fn a_state_change_is_one_readable_line() {
        let mut a = app();
        a.push_block(Block::System(
            "background #1 scout: running — reading files".to_string(),
        ));
        let texts = system_texts(&a);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("running"));
        assert!(texts[0].contains("reading files"));
    }
}

/// A resumed transcript must keep the tool calls the live session showed.
#[test]
fn a_loaded_session_keeps_banner_user_tool_and_assistant_rows() {
    use loon_core::replay::VisibleBlock;
    use loon_tui::Block;
    let mut a = app(false);
    a.set_session_blocks(
        [
            VisibleBlock::User {
                text: "run a command".into(),
            },
            VisibleBlock::Tool {
                name: "bash".into(),
                detail: "$ echo hi".into(),
                ok: true,
                excerpt: "hi".into(),
            },
            VisibleBlock::Assistant {
                text: "done after the tool".into(),
            },
        ]
        .into_iter()
        .map(|b| match b {
            VisibleBlock::User { text } => Block::User(text),
            VisibleBlock::Assistant { text } => Block::Assistant(text),
            VisibleBlock::Tool {
                name,
                detail,
                ok,
                excerpt,
            } => Block::Tool {
                name,
                ok,
                excerpt,
                pending: false,
                detail,
                started: None,
                elapsed: None,
            },
        }),
    );
    // The banner stays pinned at the top, and nothing of the session is dropped.
    assert!(matches!(a.blocks.first(), Some(Block::Banner)));
    assert_eq!(a.blocks.len(), 4, "banner + 3 session rows: {:?}", a.blocks);
    assert!(matches!(
        &a.blocks[2],
        Block::Tool { name, detail, ok: true, pending: false, .. }
            if name == "bash" && detail == "$ echo hi"
    ));
}

/// A fresh chat (`/new`) shows the banner plus an honest opener — never phantoms.
#[test]
fn a_fresh_chat_shows_banner_and_opener_not_phantom_content() {
    use loon_tui::Block;
    let mut a = app(false);
    a.blocks.clear();
    a.set_session_blocks(std::iter::empty());
    assert!(matches!(a.blocks.first(), Some(Block::Banner)));
    assert!(
        a.blocks.iter().any(|b| matches!(
            b,
            Block::System(t) if t.contains("new session")
        )),
        "a fresh chat must say what it is: {:?}",
        a.blocks
    );
    // Tool and cancelled rows (added by the session-loaded work) never phantom.
    assert!(!a
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Tool { .. } | Block::Assistant(_))));
}

/// Below 55 columns the bar used to collapse to just the activity word, dropping the
/// queue count entirely: a user with three queued prompts saw "● Ready" and no sign that
/// anything was waiting. The queue is the one field that must survive a narrow terminal.
#[test]
fn a_narrow_status_bar_keeps_the_queue_count() {
    let mut a = app(false);
    a.activity = "Ready".into();
    a.queued = vec!["one".into(), "two".into(), "three".into()];
    let bar = footer_at(&mut a, 34);
    assert!(
        bar.contains("3 queued"),
        "the queue count must survive a narrow bar, got: {bar:?}"
    );
}

/// A paused queue says so, rather than looking like ordinary pending work.
#[test]
fn a_narrow_status_bar_distinguishes_a_paused_queue() {
    let mut a = app(false);
    a.activity = "Ready".into();
    a.queued = vec!["one".into(), "two".into()];
    a.queue_paused = true;
    let bar = footer_at(&mut a, 34);
    assert!(bar.contains("2 paused"), "got: {bar:?}");
}

/// With nothing queued the narrow bar still names the state, and never shows a phantom
/// count.
#[test]
fn a_narrow_status_bar_with_no_queue_shows_no_count() {
    let mut a = app(false);
    a.activity = "Ready".into();
    let bar = footer_at(&mut a, 34);
    assert!(bar.contains("Ready"), "{bar:?}");
    assert!(!bar.contains("queued"), "no queue means no count: {bar:?}");
    assert!(!bar.contains("paused"), "{bar:?}");
}

/// An activity string longer than the bar must not push the count off the edge: the word
/// truncates, the count stays.
#[test]
fn a_long_activity_does_not_displace_the_queue_count() {
    let mut a = app(true);
    a.activity = "Running a very long tool description that will not fit".into();
    a.queued = vec!["one".into()];
    let bar = footer_at(&mut a, 30);
    assert!(
        bar.contains("1 queued"),
        "the count must be kept when the activity word is too long, got: {bar:?}"
    );
}

/// Queued prompts render as dim rows above the prompt bar. Two defects: a long prompt was
/// cut by the frame edge with no ellipsis, and the rows sat directly under the assistant's
/// last line with no separator, so they read as part of the answer.
#[test]
fn queued_prompts_are_clipped_with_an_ellipsis() {
    let mut a = app(false);
    a.queued = vec!["a queued prompt whose text is much longer than the narrow frame".into()];
    let rows = frame(&mut a, 40, 20);
    let queued = rows
        .iter()
        .find(|r| r.contains('↳'))
        .expect("the queued prompt must be rendered");
    assert!(
        queued.contains('…'),
        "an over-long preview must be clipped with an ellipsis: {queued:?}"
    );
    assert!(
        queued.chars().count() <= 40,
        "the row must not overflow the frame: {queued:?}"
    );
}

/// A dim rule separates the queue from the transcript, so waiting prompts do not read as
/// part of the assistant's answer above them.
#[test]
fn a_rule_separates_the_queue_from_the_transcript() {
    let mut a = app(false);
    a.blocks
        .push(loon_tui::Block::Assistant("an answer".into()));
    a.queued = vec!["pending".into()];
    let rows = frame(&mut a, 40, 20);
    let queued_at = rows
        .iter()
        .position(|r| r.contains('↳'))
        .expect("the queue row");
    let rule = rows[queued_at - 1].trim().to_string();
    assert!(
        !rule.is_empty() && rule.chars().all(|c| c == '─'),
        "the row above the queue must be a drawn separator rule, got: {rule:?}"
    );
}

/// When more prompts are waiting than fit, the hidden ones are counted rather than
/// silently dropped — the status bar shows the total, this says what is not on screen.
#[test]
fn hidden_queued_prompts_are_counted() {
    let mut a = app(false);
    a.queued = (0..6).map(|n| format!("prompt {n}")).collect();
    let rows = frame(&mut a, 40, 20);
    let joined = rows.join("\n");
    assert!(
        joined.contains("+3 earlier"),
        "the three prompts beyond the preview cap must be counted: {joined:?}"
    );
}
