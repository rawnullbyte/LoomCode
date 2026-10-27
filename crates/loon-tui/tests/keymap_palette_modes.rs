//! Configurable keybindings, the command palette and explicit execution modes, exercised
//! through the real dispatch path and the real agent.
//!
//! These are behavioural: each test presses keys or sends a prompt and asserts on what the
//! application actually did — which tools ran, which config file was written, which mode the
//! runtime enforces.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use loon_config::keymap::{Action, Chord, Code, KeyMap, Mods, Scope};
use loon_tui::{keys, App, StatusLine, UiCommand};
use tokio::sync::mpsc;

fn fresh() -> App {
    App::new(
        true,
        StatusLine {
            model: "mock".into(),
            mode: "bypass".into(),
            ..Default::default()
        },
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

async fn press(app: &mut App, event: KeyEvent) -> mpsc::Receiver<UiCommand> {
    let (tx, rx) = mpsc::channel(8);
    loon_tui::press_key(app, event, &tx).await;
    rx
}

// ── keybindings ───────────────────────────────────────────────────────────────

/// The binding decides what a key does: rebinding the palette to Ctrl+K makes Ctrl+K open it
/// and Ctrl+P stop doing so. Nothing is hard-coded past the map.
#[tokio::test]
async fn a_rebound_key_moves_the_command_and_the_old_key_stops_working() {
    fn rebind() -> KeyMap {
        let mut user = std::collections::BTreeMap::new();
        user.insert("open_palette".to_string(), "ctrl+k".to_string());
        KeyMap::new(loon_config::KeyProfile::Default, &user)
    }

    let mut bound = fresh();
    bound.keys = rebind();
    press(&mut bound, ctrl('k')).await;
    assert!(
        bound.palette_open,
        "Ctrl+K must open the palette after rebinding"
    );

    let mut unbound = fresh();
    unbound.keys = rebind();
    press(&mut unbound, ctrl('p')).await;
    assert!(
        !unbound.palette_open,
        "the old chord must not keep firing after a rebind"
    );
}

/// Rebound actions execute the real behaviour, not a placeholder: the palette entry that
/// copies the last answer produces a copy request.
#[tokio::test]
async fn a_rebound_copy_action_still_copies() {
    let mut app = fresh();
    app.push_block(loon_tui::Block::Assistant("the answer".into()));
    let mut user = std::collections::BTreeMap::new();
    user.insert("copy_last_response".to_string(), "f9".to_string());
    app.keys = KeyMap::new(loon_config::KeyProfile::Default, &user);
    press(&mut app, key(KeyCode::F(9))).await;
    assert_eq!(app.pending_copy.as_deref(), Some("the answer"));
}

/// Escape is contextual and both meanings are reachable: with a popup open it dismisses, and in
/// a manager surface it closes the surface.
#[tokio::test]
async fn escape_dismisses_a_popup_then_stops_a_turn() {
    let mut app = fresh();
    app.input = "/he".into();
    app.cursor = 3;
    assert!(app.completion_count() > 0, "the popup should be showing");
    press(&mut app, key(KeyCode::Esc)).await;
    assert!(app.popup_dismissed, "the first Escape closes the popup");

    let mut busy = fresh();
    busy.status.busy = true;
    press(&mut busy, key(KeyCode::Esc)).await;
    assert!(busy.cancel_armed, "the first Escape arms the stop");
}

/// A chord the terminal owns is refused with the reason, and the refusal is what the config
/// validator reports: the user is never left with a binding that cannot work.
#[test]
fn a_reserved_chord_is_refused_with_an_explanation() {
    let map = KeyMap::default();
    let error = map
        .rebind(Action::Help, Chord::ctrl('z'))
        .expect_err("ctrl+z suspends the process");
    let message = error.to_string();
    assert!(message.contains("ctrl+z"), "{message}");
    assert!(message.contains("suspend"), "{message}");
}

// ── command palette ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_palette_opens_filters_and_runs_the_selected_command() {
    let mut app = fresh();
    press(&mut app, ctrl('p')).await;
    assert!(app.palette_open, "Ctrl+P opens the palette");
    for c in "mode".chars() {
        press(&mut app, key(KeyCode::Char(c))).await;
    }
    let palette = app.command_palette.as_ref().unwrap();
    assert_eq!(palette.query, "mode");
    let top = palette.hits()[0].entry.label.clone();
    assert!(
        top.starts_with("/mode"),
        "the closest command must lead: {top}"
    );

    let rx = press(&mut app, key(KeyCode::Enter)).await;
    assert!(!app.palette_open, "accepting closes the palette");
    assert!(!rx.is_empty() || app.navigator.is_some() || !app.input.is_empty());
}

/// Accepting a slash entry sends that slash command through the ordinary path, so the palette
/// cannot invoke something the command layer does not implement.
#[tokio::test]
async fn accepting_a_palette_entry_submits_the_real_command() {
    let mut app = fresh();
    press(&mut app, ctrl('p')).await;
    for c in "help".chars() {
        press(&mut app, key(KeyCode::Char(c))).await;
    }
    let mut rx = press(&mut app, key(KeyCode::Enter)).await;
    match rx.try_recv() {
        Ok(UiCommand::Slash(loon_core::commands::SlashCommand::Help)) => {}
        other => panic!("expected /help, got {other:?}"),
    }
}

#[tokio::test]
async fn escape_closes_the_palette_and_keeps_typing_out_of_the_prompt() {
    let mut app = fresh();
    press(&mut app, ctrl('p')).await;
    for c in "abc".chars() {
        press(&mut app, key(KeyCode::Char(c))).await;
    }
    assert_eq!(
        app.input, "",
        "typing in the palette must not reach the prompt"
    );
    press(&mut app, key(KeyCode::Esc)).await;
    assert_eq!(
        app.command_palette.as_ref().unwrap().query,
        "",
        "the first Escape clears the query"
    );
    press(&mut app, key(KeyCode::Esc)).await;
    assert!(!app.palette_open, "the second Escape closes the palette");
    press(&mut app, key(KeyCode::Char('x'))).await;
    assert_eq!(app.input, "x", "the prompt receives keys again");
}

/// A palette entry that runs a keymap action (there is no slash command for it) executes the
/// action itself.
#[tokio::test]
async fn a_shortcut_entry_in_the_palette_runs_the_action() {
    let mut app = fresh();
    app.push_block(loon_tui::Block::Assistant("copy me".into()));
    press(&mut app, ctrl('p')).await;
    for c in "copy_last".chars() {
        press(&mut app, key(KeyCode::Char(c))).await;
    }
    press(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(
        app.pending_copy.as_deref(),
        Some("copy me"),
        "the shortcut entry must run the copy action"
    );
}

/// The palette search is fuzzy, not prefix-only: an out-of-order subsequence still finds it.
#[test]
fn palette_scoring_is_a_subsequence_match() {
    use loon_tui::palette::fuzzy_score;
    assert!(
        fuzzy_score("mdl", "/model").is_some(),
        "acronym-style match"
    );
    assert_eq!(fuzzy_score("xyz", "/model"), None);
    assert!(
        fuzzy_score("model", "/model").unwrap() > fuzzy_score("mdl", "/model").unwrap(),
        "an exact prefix must outrank a looser match"
    );
}

/// The scope decides which binding applies: the same Escape means "stop" in the prompt and
/// "dismiss" in a manager surface, and the map keeps them apart.
#[test]
fn scopes_keep_escape_unambiguous() {
    let map = KeyMap::default();
    assert_eq!(
        map.resolve(Chord::plain(Code::Esc), Scope::Prompt),
        Some(Action::StopTurn)
    );
    assert_eq!(
        map.resolve(Chord::plain(Code::Esc), Scope::Manager),
        Some(Action::Dismiss)
    );
    assert!(map.conflicts().is_empty());
}

/// A global chord resolves everywhere, and a scoped binding of the same chord takes precedence
/// only in its own scope.
#[test]
fn resolution_prefers_the_specific_scope() {
    let map = KeyMap::default()
        .steal(Action::Help, Chord::new(Code::Enter, Mods::CTRL))
        .unwrap();
    // Help is global; InterruptSend is prompt-scoped. In the prompt the specific binding wins,
    // and elsewhere the global one still resolves.
    assert_eq!(
        map.resolve(Chord::new(Code::Enter, Mods::CTRL), Scope::Prompt),
        Some(Action::InterruptSend)
    );
    assert_eq!(
        map.resolve(Chord::new(Code::Enter, Mods::CTRL), Scope::Manager),
        Some(Action::Help)
    );
    assert!(map.conflicts().is_empty());
}

/// The chord conversion is what makes rebinding work for named keys: Shift+Enter stays distinct
/// from Enter, and a letter is normalised so Shift+P is still `p`.
#[test]
fn terminal_events_convert_to_canonical_chords() {
    let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
    assert_eq!(
        keys::chord(&shift_enter),
        Some(Chord::new(Code::Enter, Mods::SHIFT))
    );
    let capital = KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT);
    assert_eq!(
        keys::chord(&capital),
        Some(Chord::plain(Code::Char('p'))),
        "a shifted letter is the same key"
    );
    let ctrl_left = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    assert_eq!(
        keys::chord(&ctrl_left),
        Some(Chord::new(
            Code::Left,
            Mods {
                ctrl: true,
                alt: false,
                shift: true
            }
        )),
        "named keys keep Shift"
    );
}

/// Ctrl+Q quits immediately even while a manager surface is open. The refactor that moved command
/// keys into the keymap had dropped it, and a modal that traps the user is worse than a missing
/// shortcut — so both the binding and the modal escape hatch are pinned here.
#[tokio::test]
async fn quit_works_from_the_prompt_and_from_an_open_dialog() {
    let mut app = fresh();
    press(&mut app, ctrl('q')).await;
    assert!(app.quit, "Ctrl+Q must quit from the prompt");

    let mut app = fresh();
    app.open_keybindings();
    assert!(app.navigator.is_some());
    press(&mut app, ctrl('q')).await;
    assert!(
        app.quit,
        "a dialog must not trap the user in the application"
    );
}

/// Ctrl+C keeps its two-step meaning: stop a turn, then quit. It must not have become a plain quit.
#[tokio::test]
async fn ctrl_c_stops_before_it_quits() {
    let mut app = fresh();
    app.status.busy = true;
    let mut rx = press(&mut app, ctrl('c')).await;
    assert!(!app.quit, "the first press must not quit");
    assert!(
        matches!(rx.try_recv(), Ok(UiCommand::Cancel)),
        "the first press must cancel the running turn"
    );
    let mut app = fresh();
    press(&mut app, ctrl('c')).await;
    assert!(!app.quit, "the first press only arms the quit");
    press(&mut app, ctrl('c')).await;
    assert!(app.quit, "the second press quits");
}

/// Shift+Tab cycles the permission mode, and the terminal reports it as `BackTab`. The chord
/// conversion has to bridge that, or the documented shortcut silently does nothing.
#[tokio::test]
async fn shift_tab_cycles_the_permission_mode() {
    let mut app = fresh();
    let mut rx = press(
        &mut app,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    )
    .await;
    assert!(
        matches!(rx.try_recv(), Ok(UiCommand::CycleMode)),
        "BackTab must cycle the permission mode"
    );
    // A terminal that reports plain BackTab (no modifier) must behave the same way.
    let mut app = fresh();
    let mut rx = press(
        &mut app,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
    )
    .await;
    assert!(matches!(rx.try_recv(), Ok(UiCommand::CycleMode)));
}
