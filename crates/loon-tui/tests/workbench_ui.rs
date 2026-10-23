use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use loon_core::catalog::{Catalog, ComboSummary, Editor, MenuItem, ProviderSummary};
use loon_tui::{
    manager::{Form, Navigator, View},
    App, Block, SelRange, StatusLine,
};
use ratatui::{backend::TestBackend, Terminal};
fn app() -> App {
    App::new(
        true,
        StatusLine {
            model: "p/model0".into(),
            mode: "bypass".into(),
            cwd: "/project".into(),
            ..Default::default()
        },
    )
}
fn catalog() -> Catalog {
    Catalog {
        models: (0..120)
            .map(|n| format!("p/model{n:03}"))
            .chain((0..5).map(|n| format!("other/model{n:03}")))
            .collect(),
        recent: (0..5).map(|n| format!("p/model{n:03}")).collect(),
        providers: vec![
            ProviderSummary {
                name: "p".into(),
                url: "http://localhost/v1".into(),
                api: "chat".into(),
                count: 120,
            },
            ProviderSummary {
                name: "other".into(),
                url: "http://localhost/v1".into(),
                api: "responses".into(),
                count: 5,
            },
        ],
        combos: vec![ComboSummary {
            name: "daily".into(),
            mode: "fallback".into(),
            members: vec!["p/model000".into()],
        }],
        ..Default::default()
    }
}
fn screen(a: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| loon_tui::ui::draw(f, a)).unwrap();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| term.backend().buffer().cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[tokio::test]
async fn model_home_searches_all_providers_and_scoped_browse_stays_scoped() {
    let mut a = app();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    a.navigator = Some(Navigator::new(catalog(), View::Models));
    let rows = a.navigator.as_ref().unwrap().rows();
    assert_eq!(rows.iter().filter(|r| r.detail == "Recent").count(), 5);
    assert!(rows.iter().any(|r| r.label == "combo:daily"));
    for c in "other model004".chars() {
        loon_tui::manager::key(
            &mut a,
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &tx,
        )
        .await;
    }
    let rows = a.navigator.as_ref().unwrap().rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "other/model004");
    a.navigator = Some(Navigator::new(catalog(), View::AllModels(Some("p".into()))));
    a.navigator.as_mut().unwrap().query = "other/model".into();
    assert!(a.navigator.as_ref().unwrap().rows().is_empty());
}
#[tokio::test]
async fn page_keys_and_mouse_scroll_keep_selection_visible() {
    let mut a = app();
    a.navigator = Some(Navigator::new(catalog(), View::AllModels(None)));
    let (tx, _) = tokio::sync::mpsc::channel(8);
    screen(&mut a, 100, 28);
    for _ in 0..6 {
        loon_tui::manager::key(
            &mut a,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &tx,
        )
        .await;
    }
    let output = screen(&mut a, 100, 28);
    let nav = a.navigator.as_ref().unwrap();
    let label = nav.rows()[nav.selected].label.clone();
    assert!(output.contains(&label), "{output}");
    let before = nav.selected;
    loon_tui::manager::mouse(
        &mut a,
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 45,
            row: 10,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(a.navigator.as_ref().unwrap().selected, before + 3);
}
#[test]
fn every_modal_renders_in_small_terminals_and_masks_secrets() {
    for (w, h) in [(1, 1), (12, 5), (40, 12), (100, 30)] {
        let mut a = app();
        a.navigator = Some(Navigator::new(catalog(), View::Models));
        screen(&mut a, w, h);
        a.editor = Some(Form::new(Editor {
            kind: "provider".into(),
            values: vec![
                "test".into(),
                "https://example.org/v1".into(),
                "SECRET_DO_NOT_DISPLAY".into(),
                "responses".into(),
                String::new(),
                String::new(),
            ],
            editing: false,
            fields: vec![],
        }));
        let text = screen(&mut a, w, h);
        assert!(!text.contains("SECRET_DO_NOT_DISPLAY"));
    }
}
#[test]
fn copying_preserves_code_indentation_unicode_and_original_newlines() {
    let mut a = app();
    let text="fn main() {\n    let 中文 = \"Hello world and a long line that wraps repeatedly\";\n\tprintln!(\"😀\");\n}";
    a.blocks.push(Block::Assistant(text.into()));
    let rows = a.transcript_rows(28);
    let copied = loon_tui::transcript::copy(
        &rows,
        SelRange {
            start_line: 0,
            start_col: 0,
            end_line: rows.len() - 1,
            end_col: 1000,
        },
    );
    assert_eq!(copied, text);
    let copied_backwards = loon_tui::transcript::copy(
        &rows,
        SelRange {
            start_line: rows.len() - 1,
            start_col: 1000,
            end_line: 0,
            end_col: 0,
        },
    );
    assert_eq!(copied_backwards, text);
}
#[test]
fn selected_cells_get_a_visible_background_that_survives_normalisation() {
    let mut a = app();
    a.blocks.push(Block::Assistant("selectable text".into()));
    let rows = a.transcript_rows(60);
    let width = 60;
    let p = rows[0].prefix_cells;
    a.selection = Some(SelRange {
        start_line: 0,
        start_col: p,
        end_line: 0,
        end_col: p + 9,
    });
    let mut term = Terminal::new(TestBackend::new(width, 12)).unwrap();
    term.draw(|f| loon_tui::ui::draw(f, &mut a)).unwrap();
    let buffer = term.backend().buffer();
    // Locate the assistant row rather than assuming a fixed offset.
    let y = (0..12u16)
        .find(|y| buffer.cell(((p + 1) as u16, *y)).unwrap().symbol() != " ")
        .expect("assistant row is on screen");
    let inside = buffer.cell(((p + 1) as u16, y)).unwrap();
    let outside = buffer.cell(((p + 20) as u16, y)).unwrap();
    assert_ne!(
        inside.bg,
        ratatui::style::Color::Reset,
        "selected cell must paint a background"
    );
    assert_eq!(
        outside.bg,
        ratatui::style::Color::Reset,
        "unselected cells stay on the terminal background"
    );
}

#[test]
fn same_line_reverse_selection_and_wide_glyphs_use_terminal_cells() {
    let mut a = app();
    a.blocks.push(Block::Assistant("a中b😀c".into()));
    let rows = a.transcript_rows(80);
    // Columns are terminal cells; the block prefix occupies `prefix_cells` of them.
    let p = rows[0].prefix_cells;
    let copied = loon_tui::transcript::copy(
        &rows,
        SelRange {
            start_line: 0,
            start_col: p + 6,
            end_line: 0,
            end_col: p + 1,
        },
    );
    assert_eq!(copied, "中b😀");
    assert_eq!(
        loon_tui::transcript::clean("\x1b[31mred\x1b[0m\r\x1b]0;title\x07!"),
        "red!"
    );
}
#[tokio::test]
async fn forms_edit_at_cursor_and_submit_without_showing_keys() {
    let mut a = app();
    a.editor = Some(Form::new(Editor {
        kind: "provider".into(),
        values: vec![
            "local".into(),
            "http://localhost/v1".into(),
            "secret".into(),
            "chat".into(),
            String::new(),
            String::new(),
        ],
        editing: false,
        fields: vec![],
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    loon_tui::manager::key(
        &mut a,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &tx,
    )
    .await;
    match rx.recv().await.unwrap() {
        loon_tui::UiCommand::Slash(loon_core::commands::SlashCommand::Provider { args }) => {
            assert_eq!(args[1], "local");
            assert_eq!(args[3], "secret");
        }
        other => panic!("{other:?}"),
    };
    assert!(a.editor.is_none());
}
#[test]
fn themes_keep_transparency_or_use_readable_opaque_surfaces() {
    for name in ["terminal", "dark", "light", "ocean"] {
        let mut a = app();
        a.palette = name.into();
        a.transparent = name == "terminal";
        a.navigator = Some(Navigator::new(catalog(), View::Models));
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| loon_tui::ui::draw(f, &mut a)).unwrap();
        if a.transparent {
            for cell in &term.backend().buffer().content {
                assert_eq!(cell.bg, ratatui::style::Color::Reset);
            }
        }
    }
}
#[test]
fn session_search_matches_title_and_workspace() {
    let mut n = Navigator::new(
        Default::default(),
        View::Menu(
            "Sessions".into(),
            vec![MenuItem {
                label: "Fix parser".into(),
                detail: "1234 · /work/loon".into(),
                command: "/resume 1234".into(),
            }],
        ),
    );
    n.query = "parser loon".into();
    assert_eq!(n.rows().len(), 1);
}

#[tokio::test]
async fn failed_save_preserves_editor_values_and_shows_the_error() {
    let mut a = app();
    a.editor = Some(Form::new(Editor {
        kind: "provider".into(),
        values: vec![
            "my provider".into(),
            "invalid-url".into(),
            "secret".into(),
            "chat".into(),
            String::new(),
            String::new(),
        ],
        editing: false,
        fields: vec![],
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    loon_tui::manager::key(
        &mut a,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &tx,
    )
    .await;
    a.slash_result("Error: endpoint must be http(s)".into());
    let form = a.editor.as_ref().unwrap();
    assert_eq!(form.editor.values[2], "secret");
    assert!(form.error.contains("http(s)"));
}

#[test]
fn mcp_servers_are_managed_from_a_popup() {
    use loon_core::catalog::McpSummary;
    let mut cat = catalog();
    cat.mcps = vec![McpSummary {
        name: "github".into(),
        command: "npx".into(),
        args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
        timeout_secs: 60,
        tools: 12,
        error: String::new(),
    }];
    cat.tools = vec!["mcp_github_search".into(), "read".into()];
    let nav = Navigator::new(cat, View::Mcps);
    let rows = nav.rows();
    assert!(rows.iter().any(|r| r.label == "+ Add MCP server"));
    assert!(rows.iter().any(|r| r.label == "Reconnect all"));
    let server = rows.iter().find(|r| r.label == "github").unwrap();
    assert!(server.detail.contains("12 tools"), "{}", server.detail);

    let detail = Navigator::new(
        Catalog {
            mcps: vec![McpSummary {
                name: "github".into(),
                command: "npx".into(),
                args: vec![],
                timeout_secs: 60,
                tools: 1,
                error: String::new(),
            }],
            tools: vec!["mcp_github_search".into(), "read".into()],
            ..Default::default()
        },
        View::Mcp("github".into()),
    );
    let rows = detail.rows();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"Reconnect"), "{labels:?}");
    assert!(labels.contains(&"Edit server"), "{labels:?}");
    assert!(labels.contains(&"Remove server"), "{labels:?}");
    assert!(labels.contains(&"mcp_github_search"), "{labels:?}");
    assert!(
        !labels.contains(&"read"),
        "only this server's tools are listed: {labels:?}"
    );
}

#[test]
fn collapsed_tool_group_keeps_each_failed_command_and_error_visible() {
    let mut a = app();
    for (command, ok, output) in [
        ("pwd", true, "/repo"),
        (
            "cargo test",
            false,
            "--- stderr ---\nerror: missing dependency\n[exit 1: completed]",
        ),
        (
            "npm test",
            false,
            "--- stderr ---\nCannot find module vite\n[exit 1: completed]",
        ),
    ] {
        a.blocks.push(Block::Tool {
            name: "bash".into(),
            ok,
            excerpt: output.into(),
            pending: false,
            detail: format!("$ {command}"),
            started: None,
            elapsed: None,
        });
    }
    let output = screen(&mut a, 100, 28);
    assert!(output.contains("missing dependency"), "{output}");
    assert!(output.contains("Cannot find module vite"));
    assert!(output.contains("cargo test"));
    assert!(output.contains("npm test"));
}
#[test]
fn reduced_motion_freezes_animation_without_hiding_activity() {
    let mut a = app();
    a.animations = false;
    a.status.busy = true;
    a.activity = "Running bash".into();
    let initial = a.tick;
    a.tick();
    assert_eq!(a.tick, initial);
    assert!(screen(&mut a, 100, 15).contains("Running bash"));
}
#[tokio::test]
async fn form_mouse_scroll_changes_focused_field() {
    let mut a = app();
    a.editor = Some(Form::new(Editor {
        kind: "provider".into(),
        values: vec![
            "local".into(),
            "http://localhost/v1".into(),
            String::new(),
            "chat".into(),
            String::new(),
            String::new(),
        ],
        editing: false,
        fields: vec![],
    }));
    screen(&mut a, 80, 16);
    let old = a.editor.as_ref().unwrap().focused;
    loon_tui::manager::mouse(
        &mut a,
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 8,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_ne!(a.editor.as_ref().unwrap().focused, old);
}

#[tokio::test]
async fn theme_picker_previews_keyboard_and_wheel_and_escape_restores() {
    let mut a = app();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let items = [
        "terminal", "dark", "light", "ocean", "violet", "ember", "graphite",
    ]
    .iter()
    .map(|name| MenuItem {
        label: (*name).into(),
        detail: format!("Preview {name}"),
        command: format!("/theme {name}"),
    })
    .collect();
    a.navigator = Some(Navigator::new(
        Default::default(),
        View::Menu("Appearance".into(), items),
    ));
    screen(&mut a, 100, 30);
    loon_tui::manager::key(
        &mut a,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &tx,
    )
    .await;
    assert_eq!(a.palette, "dark");
    assert!(!a.transparent);
    assert!(rx.try_recv().is_err(), "preview must not persist");
    screen(&mut a, 100, 30);
    loon_tui::manager::mouse(
        &mut a,
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 30,
            row: 12,
            modifiers: KeyModifiers::NONE,
        },
    );
    screen(&mut a, 100, 30);
    assert_ne!(a.palette, "dark");
    loon_tui::manager::key(&mut a, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx).await;
    assert_eq!(a.palette, "terminal");
    assert!(a.transparent);
    assert!(a.navigator.is_none());
    assert!(rx.try_recv().is_err());
}
#[tokio::test]
async fn settings_arrow_values_change_in_place_without_enter() {
    let mut a = app();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    a.navigator = Some(Navigator::new(
        Default::default(),
        View::Menu(
            "Settings".into(),
            vec![MenuItem {
                label: "effort = medium".into(),
                detail: "reasoning effort".into(),
                command: "/config edit effort".into(),
            }],
        ),
    ));
    for _ in 0..2 {
        loon_tui::manager::key(
            &mut a,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &tx,
        )
        .await;
    }
    assert_eq!(
        a.navigator.as_ref().unwrap().rows()[0].label,
        "effort = xhigh"
    );
    assert!(a.editor.is_none());
    let values: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|c| {
            if let loon_tui::UiCommand::Slash(loon_core::commands::SlashCommand::Config { args }) =
                c
            {
                Some(args)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(values[0], vec!["menu-set", "effort", "high"]);
    assert_eq!(values[1], vec!["menu-set", "effort", "xhigh"]);
}
#[test]
fn live_resizing_preserves_draft_and_modal_selection_without_stale_hitboxes() {
    let mut a = app();
    a.input = "draft with Unicode 🦀 and a long line that should reappear when expanded".into();
    a.cursor = a.input.chars().count();
    a.navigator = Some(Navigator::new(catalog(), View::AllModels(None)));
    a.navigator.as_mut().unwrap().selected = 60;
    let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
    let draft = a.input.clone();
    for (w, h) in [
        (120, 35),
        (28, 9),
        (1, 1),
        (5, 3),
        (70, 18),
        (160, 42),
        (80, 24),
    ] {
        terminal.backend_mut().resize(w, h);
        terminal.autoresize().unwrap();
        a.resized();
        terminal.clear().unwrap();
        terminal.draw(|f| loon_tui::ui::draw(f, &mut a)).unwrap();
        assert_eq!(a.input, draft);
        assert_eq!(a.navigator.as_ref().unwrap().selected, 60);
        if let Some(rect) = a.navigator.as_ref().unwrap().rect {
            assert!(rect.right() <= w && rect.bottom() <= h);
        }
        if let Some((x, y)) = a.input_rect {
            assert!(x < w && y < h);
        }
    }
}
#[test]
fn throughput_is_one_request_counter_for_thinking_and_writing() {
    let mut a = app();
    a.request_rate
        .start_at(std::time::Instant::now() - std::time::Duration::from_secs(2));
    a.request_rate.add_chars(800);
    a.stream_text(&"x".repeat(400));
    assert_eq!(a.request_rate.tokens(), 300);
    a.activity = "Thinking".into();
    a.status.busy = true;
    a.turn_started = Some(std::time::Instant::now());
    let thinking = screen(&mut a, 130, 30);
    assert_eq!(thinking.matches("t/s").count(), 1);
    assert!(!thinking.contains("out ~"));
    a.activity = "Writing".into();
    let writing = screen(&mut a, 130, 30);
    assert_eq!(writing.matches("t/s").count(), 1);
    assert_eq!(a.request_rate.tokens(), 300);
    a.request_rate.report(400);
    assert_eq!(a.request_rate.tokens(), 400);
    assert!(!a.request_rate.label().contains('~'));
}

#[test]
fn rewinding_clears_newer_chat_and_restores_original_draft() {
    let mut a = app();
    a.blocks = vec![
        Block::User("keep".into()),
        Block::Assistant("keep reply".into()),
        Block::User("remove".into()),
        Block::Assistant("remove reply".into()),
    ];
    a.rewind_conversation(
        vec![
            loon_core::replay::VisibleBlock::User {
                text: "keep".into(),
            },
            loon_core::replay::VisibleBlock::Assistant {
                text: "keep reply".into(),
            },
        ],
        "remove\nexact draft".into(),
    );
    assert_eq!(a.blocks.len(), 2);
    assert_eq!(a.input, "remove\nexact draft");
    assert_eq!(a.cursor, a.input.chars().count());
    let rendered = screen(&mut a, 100, 30);
    assert!(!rendered.contains("remove reply"));
    assert!(rendered.contains("exact draft"));
}

#[test]
fn rewind_keeps_earlier_tool_rows_in_the_live_transcript() {
    let mut a = app();
    a.blocks = vec![
        Block::Banner,
        Block::User("keep".into()),
        Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: "prior file output".into(),
            pending: false,
            detail: String::new(),
            started: None,
            elapsed: None,
        },
        Block::Assistant("keep answer".into()),
        Block::User("remove".into()),
        Block::Assistant("remove answer".into()),
    ];
    a.rewind_conversation(
        vec![
            loon_core::replay::VisibleBlock::User {
                text: "keep".into(),
            },
            loon_core::replay::VisibleBlock::Assistant {
                text: "keep answer".into(),
            },
        ],
        "remove".into(),
    );
    assert_eq!(a.blocks.len(), 4);
    assert!(matches!(&a.blocks[2],Block::Tool {excerpt,..} if excerpt=="prior file output"));
    assert_eq!(a.input, "remove");
}
#[tokio::test]
async fn configuration_pages_keep_history_selection_and_adjust_without_closing() {
    let mut a = app();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let row = MenuItem {
        label: "timeout_secs = 30".into(),
        detail: "Request timeout".into(),
        command: format!(
            "/config ui edit {}",
            loon_core::config_ui::encode("/context7/timeout_secs")
        ),
    };
    a.open_menu("Settings".into(), vec![]);
    a.open_menu("Settings / context7".into(), vec![row.clone()]);
    assert_eq!(a.navigator.as_ref().unwrap().history.len(), 1);
    loon_tui::manager::key(
        &mut a,
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        &tx,
    )
    .await;
    assert!(a.navigator.is_some());
    match rx.recv().await.unwrap() {
        loon_tui::UiCommand::Slash(loon_core::commands::SlashCommand::Config { args }) => {
            assert_eq!(
                args,
                vec![
                    "ui",
                    "step",
                    &loon_core::config_ui::encode("/context7/timeout_secs"),
                    "1"
                ]
            )
        }
        _ => panic!("wrong command"),
    }
    a.open_menu("Settings / context7".into(), vec![row]);
    assert_eq!(a.navigator.as_ref().unwrap().selected, 0);
    assert_eq!(a.navigator.as_ref().unwrap().history.len(), 1);
    a.slash_result("Error: timeout_secs must be 1–120".into());
    // Driver result and backend menu events can arrive through separate channels in either order.
    let items = a
        .navigator
        .as_ref()
        .unwrap()
        .rows()
        .into_iter()
        .map(|r| MenuItem {
            label: r.label,
            detail: r.detail,
            command: format!(
                "/config ui edit {}",
                loon_core::config_ui::encode("/context7/timeout_secs")
            ),
        })
        .collect();
    a.open_menu("Settings / context7".into(), items);
    assert!(screen(&mut a, 110, 30).contains("timeout_secs must be"));
    assert!(a.blocks.is_empty());
    loon_tui::manager::key(&mut a, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx).await;
    assert_eq!(a.navigator.as_ref().unwrap().title(), "Settings");
}
#[tokio::test]
async fn configuration_secret_form_masks_input_and_sends_a_clear_action() {
    let mut a = app();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    a.editor = Some(Form::new(Editor {
        kind: "config-secret".into(),
        values: vec![
            "/context7/api_key".into(),
            "fixture-typed-secret".into(),
            "replace".into(),
        ],
        editing: true,
        fields: vec![
            Default::default(),
            Default::default(),
            loon_core::catalog::FieldSpec::choice(["replace", "keep", "clear"]),
        ],
    }));
    let shown = screen(&mut a, 110, 30);
    assert!(!shown.contains("fixture-typed-secret"));
    assert!(shown.contains("•••"));
    loon_tui::manager::key(
        &mut a,
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &tx,
    )
    .await;
    for _ in 0..2 {
        loon_tui::manager::key(
            &mut a,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &tx,
        )
        .await;
    }
    loon_tui::manager::key(
        &mut a,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &tx,
    )
    .await;
    match rx.recv().await.unwrap() {
        loon_tui::UiCommand::Slash(loon_core::commands::SlashCommand::Config { args }) => {
            assert_eq!(&args[..2], ["ui", "set"]);
            assert_eq!(args[4], "clear");
        }
        _ => panic!("wrong command"),
    }
}
#[test]
fn terminal_preferences_affect_footer_placeholder_banner_and_color_depth() {
    let mut a = app();
    a.tip_text = "custom coding hint".into();
    a.footer = false;
    a.welcome_banner = false;
    a.push_welcome_banner();
    assert!(!a.blocks.contains(&Block::Banner));
    a.color_mode = "mono".into();
    a.ascii_only = true;
    let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
    terminal.draw(|f| loon_tui::ui::draw(f, &mut a)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("custom coding hint"));
    assert!(!text.contains("bypass"));
    for cell in &terminal.backend().buffer().content {
        assert_eq!(cell.fg, ratatui::style::Color::Reset);
        assert_eq!(cell.bg, ratatui::style::Color::Reset);
        assert!(!cell.symbol().contains('─'));
    }
    a.color_mode = "c16".into();
    terminal.draw(|f| loon_tui::ui::draw(f, &mut a)).unwrap();
    assert!(terminal
        .backend()
        .buffer()
        .content
        .iter()
        .all(|c| !matches!(
            c.fg,
            ratatui::style::Color::Rgb(..) | ratatui::style::Color::Indexed(_)
        )));
}
#[test]
fn configuration_scope_search_prefers_labels_and_back_returns_to_settings() {
    let mut a = app();
    a.open_menu("Settings".into(), vec![]);
    let rows = ["user", "project", "local"]
        .into_iter()
        .map(|scope| MenuItem {
            label: format!("Scope: {scope}"),
            detail: "User, project and local settings".into(),
            command: format!("/config ui scope {}", loon_core::config_ui::encode(scope)),
        })
        .collect::<Vec<_>>();
    a.open_menu("Settings / All configuration (user)".into(), rows.clone());
    assert_eq!(a.navigator.as_ref().unwrap().history.len(), 1);
    a.navigator.as_mut().unwrap().query = "Scope: project".into();
    assert_eq!(
        a.navigator.as_ref().unwrap().rows()[0].label,
        "Scope: project"
    );
    a.open_menu("Settings / All configuration (project)".into(), rows);
    let nav = a.navigator.as_mut().unwrap();
    assert_eq!(nav.history.len(), 1);
    nav.back();
    assert_eq!(nav.title(), "Settings");
}

#[test]
fn new_session_clears_selection_and_shows_wordmark_in_a_short_terminal() {
    let mut a = app();
    a.push_welcome_banner();
    a.blocks.push(Block::Assistant("old answer".repeat(200)));
    a.selection_snapshot = Some(a.transcript_rows(80));
    a.queued.push("old queued".into());
    a.status.busy = true;
    a.set_session_blocks(std::iter::empty());
    assert!(a.selection_snapshot.is_none());
    assert!(!a.status.busy && a.queued.is_empty());
    let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
    t.draw(|f| loon_tui::ui::draw(f, &mut a)).unwrap();
    let text = t
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(
        text.contains("██████╗"),
        "the new session banner should be visible: {text}"
    );
    assert!(!text.contains("old answer"));
}

#[test]
fn wrapped_fenced_comments_and_sql_keep_their_syntax_styles() {
    use ratatui::style::Color;
    let mut a = app();
    a.blocks.push(Block::Assistant("```rust\n// return fn let this comment should stay dim over several visual rows\n```\n~~~sql\nSELECT id FROM users WHERE active = 1\n~~~".into()));
    let rows = a.transcript_rows(25);
    for row in rows
        .iter()
        .filter(|r| r.raw.contains("return") || r.raw.contains("visual"))
    {
        for span in row.line.spans.iter().skip(1) {
            assert_eq!(span.style.fg, Some(Color::DarkGray), "{:?}", row.line);
        }
    }
    assert!(rows
        .iter()
        .flat_map(|r| &r.line.spans)
        .any(|s| s.content == "SELECT" && s.style.fg == Some(Color::Magenta)));
}
