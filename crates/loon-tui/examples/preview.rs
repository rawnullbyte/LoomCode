//! Render the actual terminal widgets into SVG for review without an interactive terminal.
use loon_core::catalog::{Catalog, ComboSummary, MenuItem, ProviderSummary};
use loon_tui::{
    manager::{Navigator, View},
    App, Block, StatusLine,
};
use ratatui::{backend::TestBackend, style::Color, Terminal};
fn color(c: Color, bg: bool) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => "#17171c".into(),
        Color::White => "#f8fafc".into(),
        Color::DarkGray => "#7b8798".into(),
        Color::Cyan | Color::LightCyan => "#67d5ef".into(),
        Color::Green | Color::LightGreen => "#80d19c".into(),
        Color::Red | Color::LightRed => "#ef8790".into(),
        Color::Yellow | Color::LightYellow => "#e8cb85".into(),
        Color::Blue | Color::LightBlue => "#7facf8".into(),
        _ => if bg { "#18181c" } else { "#dcdce0" }.into(),
    }
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
#[tokio::main]
async fn main() {
    let pages = configuration_pages().await;
    let destination = std::env::args().nth(1).unwrap_or("docs/previews".into());
    std::fs::create_dir_all(&destination).unwrap();
    let models = [
        "local/code-large",
        "local/code-fast",
        "cloud/reasoner",
        "cloud/code-pro",
        "cloud/code-mini",
    ]
    .map(str::to_string)
    .to_vec();
    let catalog = Catalog {
        models: models.clone(),
        recent: models.clone(),
        providers: vec![
            ProviderSummary {
                name: "local".into(),
                url: "http://localhost:11434/v1".into(),
                api: "chat".into(),
                count: 2,
            },
            ProviderSummary {
                name: "cloud".into(),
                url: "https://api.example.org/v1".into(),
                api: "responses".into(),
                count: 3,
            },
        ],
        combos: vec![ComboSummary {
            name: "daily".into(),
            mode: "fallback".into(),
            members: vec![models[0].clone(), models[2].clone()],
        }],
        ..Default::default()
    };
    for view in [
        "workspace",
        "models",
        "providers",
        "settings",
        "configuration",
        "context7",
        "thinking",
        "appearance",
        "modes",
        "keybindings",
        "palette",
    ] {
        let mut app = App::new(
            false,
            StatusLine {
                model: "cloud/code-pro".into(),
                mode: "auto".into(),
                context_est: 8640,
                cwd: "/home/user/Projects/LoomCode".into(),
                ..Default::default()
            },
        );
        app.palette = "ocean".into();
        app.settings_effort = "high".into();
        app.push_welcome_banner();
        app.blocks.push(Block::User(
            "Fix the parser and add regression coverage.".into(),
        ));
        app.blocks.push(Block::Assistant("I found that the parser skips empty fields. I’m updating the parsing loop and checking the affected imports.".into()));
        app.blocks.push(Block::Tool {
            name: "read".into(),
            ok: true,
            excerpt: "src/parser.rs · lines 40–130 · sha256 recorded".into(),
            pending: false,
            detail: String::new(),
            started: None,
            elapsed: None,
        });
        app.blocks.push(Block::Tool{name:"update_plan".into(),ok:true,excerpt:"1. [completed] Reproduce the failure\n2. [in_progress] Update parsing and regression coverage\n3. [pending] Run tests and review the diff".into(),pending:false,detail:String::new(),started:None,elapsed:None});
        app.blocks.push(Block::Tool {
            name: "bash".into(),
            ok: true,
            excerpt: "cargo test --test parser\n12 passed · 0 failed".into(),
            pending: false,
            detail: String::new(),
            started: None,
            elapsed: None,
        });
        app.blocks.push(Block::Assistant("The parser now preserves empty fields. The focused test suite passes, including the new regression cases. I’m checking the final diff.".into()));
        app.work_state.plan = vec![loon_core::worklog::PlanStep {
            step: "Review the final diff".into(),
            status: "in_progress".into(),
        }];
        if view == "thinking" {
            app.status.busy = true;
            app.activity = "Thinking".into();
            app.request_rate
                .start_at(std::time::Instant::now() - std::time::Duration::from_secs(12));
            app.request_rate.add_chars(12400 + 1600);
            app.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(12));
        }
        if view == "palette" {
            // The real palette, built from the real command table, with a query typed in.
            app.open_palette();
            if let Some(palette) = &mut app.command_palette {
                for c in "key".chars() {
                    palette.push(c);
                }
            }
        }
        app.navigator = match view {
            "modes" => Some(Navigator::new(
                Default::default(),
                View::Menu(
                    "Execution mode".into(),
                    loon_core::mode::Mode::ALL
                        .iter()
                        .map(|m| MenuItem {
                            label: format!(
                                "{}{}",
                                if *m == loon_core::mode::Mode::Ultrawork {
                                    "● "
                                } else {
                                    "  "
                                },
                                m.id()
                            ),
                            detail: m.description().into(),
                            command: format!("/mode {}", m.id()),
                        })
                        .collect(),
                ),
            )),
            "appearance" => Some(Navigator::new(
                Default::default(),
                View::Menu(
                    "Appearance".into(),
                    [
                        "terminal", "dark", "light", "ocean", "violet", "ember", "graphite",
                    ]
                    .iter()
                    .map(|name| MenuItem {
                        label: (*name).into(),
                        detail: format!("Live preview of {name}; Enter saves, Escape reverts"),
                        command: format!("/theme {name}"),
                    })
                    .collect(),
                ),
            )),
            "models" => Some(Navigator::new(catalog.clone(), View::Models)),
            "providers" => Some(Navigator::new(catalog.clone(), View::Providers)),
            "settings" | "configuration" | "context7" => {
                let (title, items) = pages[view].clone();
                Some(Navigator::new(Default::default(), View::Menu(title, items)))
            }
            _ => None,
        };
        if view == "keybindings" {
            // Set after the navigator assignment, so the editor is what the frame renders.
            app.keys = loon_config::KeyMap::new(
                loon_config::KeyProfile::Emacs,
                &std::collections::BTreeMap::from([(
                    "open_sessions".to_string(),
                    "ctrl+shift+s".to_string(),
                )]),
            );
            app.open_keybindings();
            if let Some(nav) = &mut app.navigator {
                nav.keys = app.keys.clone();
                // Page to the prompt bindings so the preview shows real chords, not the
                // alphabetical tail of the list.
                nav.selected = 3;
            }
        }
        let (w, h) = (110, 30);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| loon_tui::ui::draw(f, &mut app)).unwrap();
        let mut svg=format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><rect width=\"100%\" height=\"100%\" fill=\"#0e1c28\"/><g font-family=\"DejaVu Sans Mono,monospace\" font-size=\"14\">",w*9+32,h*20+32,w*9+32,h*20+32);
        for y in 0..h {
            for x in 0..w {
                let c = terminal.backend().buffer().cell((x, y)).unwrap();
                let px = x * 9 + 16;
                let py = y * 20 + 16;
                svg.push_str(&format!(
                    "<rect x=\"{px}\" y=\"{py}\" width=\"9\" height=\"20\" fill=\"{}\"/>",
                    color(c.bg, true)
                ));
                if c.symbol() != " " {
                    svg.push_str(&format!(
                        "<text x=\"{px}\" y=\"{}\" fill=\"{}\">{}</text>",
                        py + 15,
                        color(c.fg, false),
                        escape(c.symbol())
                    ));
                }
            }
        }
        svg.push_str("</g></svg>");
        std::fs::write(format!("{destination}/{view}.svg"), svg).unwrap();
    }
}

// Obtain configuration previews from the actual backend, including masked state.
async fn configuration_pages() -> std::collections::BTreeMap<String, (String, Vec<MenuItem>)> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = loon_config::LoonPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        runtime_dir: root.join("runtime"),
    };
    paths.ensure().unwrap();
    let mut cfg = loon_config::Config::builtin();
    cfg.context7.api_key = Some("preview-key-must-stay-hidden".into());
    cfg.appearance.palette = "ocean".into();
    cfg.session.effort = Some("high".into());
    cfg.save(&paths).unwrap();
    let registry = std::sync::Arc::new(loon_providers::ProviderRegistry::with_mock(
        std::sync::Arc::new(loon_providers::MockProvider::new()),
    ));
    let mut agent = loon_core::Agent::new_session(
        &paths,
        loon_core::AgentConfig::from(&cfg.session),
        registry,
        loon_tools::ToolRegistry::standard(),
        root.to_path_buf(),
        true,
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    agent.set_event_sink(tx);
    let mut pages = std::collections::BTreeMap::new();
    for (name, args) in [
        ("settings", vec![]),
        ("configuration", vec!["ui".into(), "open".into()]),
        (
            "context7",
            vec![
                "ui".into(),
                "open".into(),
                loon_core::config_ui::encode("/context7"),
            ],
        ),
    ] {
        agent
            .run_slash(loon_core::commands::SlashCommand::Config { args })
            .await
            .unwrap();
        while let Ok(event) = rx.try_recv() {
            if let loon_core::AgentEvent::OpenMenu { title, items } = event {
                pages.insert(name.into(), (title, items));
            }
        }
    }
    pages
}
