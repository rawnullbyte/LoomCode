//! Terminal key dispatch driven by the configurable [`loon_config::keymap`].
//!
//! Every *command* key goes through one lookup: a crossterm event is converted to a
//! [`loon_config::Chord`], the active map resolves it in the current [`Scope`], and the
//! resulting [`loon_config::KeyAction`] is executed here. The prompt editor keeps its text
//! keys (characters, caret motion, selection, completion) — those are editing primitives,
//! not commands, and remapping them would surprise anyone who only wanted a different
//! shortcut for the palette.
//!
//! Precedence, in order:
//! 1. modal input that must not be interrupted (approval prompt, form editor, picker,
//!    reverse history search, command palette);
//! 2. configurable action bindings;
//! 3. fixed text editing.

use crate::{App, UiCommand};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use loon_config::keymap::{Action, Chord, Code, KeyMap, Mods, Scope};
use tokio::sync::mpsc;

/// Which scope is currently receiving keys. The navigator/form is a modal surface, so when
/// one is open the manager scope owns the keymap; otherwise the prompt does.
pub fn scope(app: &App) -> Scope {
    if app.palette_open || app.navigator.is_some() || app.editor.is_some() {
        Scope::Manager
    } else {
        Scope::Prompt
    }
}

/// Convert a terminal event into a keymap chord. `None` means the event cannot be a binding
/// (a modifier-only press, or a media key we do not name).
pub fn chord(event: &KeyEvent) -> Option<Chord> {
    let code = match event.code {
        KeyCode::Char(c) => Code::Char(c.to_ascii_lowercase()),
        KeyCode::Enter => Code::Enter,
        KeyCode::Esc => Code::Esc,
        KeyCode::Tab => Code::Tab,
        // Terminals report Shift+Tab as `BackTab` with or without a SHIFT modifier, so it must
        // map to Shift+Tab here or the binding could never fire.
        KeyCode::BackTab => {
            return Some(Chord::new(Code::Tab, Mods::SHIFT));
        }
        KeyCode::Backspace => Code::Backspace,
        KeyCode::Delete => Code::Delete,
        KeyCode::Insert => Code::Insert,
        KeyCode::Left => Code::Left,
        KeyCode::Right => Code::Right,
        KeyCode::Up => Code::Up,
        KeyCode::Down => Code::Down,
        KeyCode::Home => Code::Home,
        KeyCode::End => Code::End,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PageDown => Code::PageDown,
        KeyCode::F(n) => Code::F(n),
        _ => return None,
    };
    let mut mods = Mods::NONE;
    mods.ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    mods.alt = event.modifiers.contains(KeyModifiers::ALT);
    // Shift is meaningful for named keys (Shift+Enter) and for chords the user wrote with an
    // explicit shift (ctrl+shift+left). For a character it only produced the capital we
    // already lowercased, so carrying it would make Shift+p a different key from p.
    mods.shift = event.modifiers.contains(KeyModifiers::SHIFT) && !matches!(code, Code::Char(_));
    Some(Chord::new(code, mods))
}

/// A transcription of the key for error messages and the editor's capture prompt.
pub fn chord_label(event: &KeyEvent) -> String {
    chord(event)
        .map(|c| c.to_string())
        .unwrap_or_else(|| format!("{:?}", event.code))
}

/// Result of offering a key to the action bindings.
pub enum Handled {
    /// An action executed.
    Action(Action),
    /// No binding matched; the caller should fall through to text editing.
    Unbound,
}

/// Execute one action. `Ok` means it ran (or was legitimately a no-op because the action has
/// nothing to act on); `Err` carries a message for the transcript.
pub async fn execute(
    app: &mut App,
    action: Action,
    cmd_tx: &mpsc::Sender<UiCommand>,
) -> Result<(), String> {
    execute_with(app, action, false, cmd_tx).await
}

/// As [`execute`], with the Shift state that turns word motion into a selection. Shift is a
/// modifier on the same command (Ctrl+Shift+Left selects the word), not a separate action, so
/// it travels alongside the action rather than doubling the binding list.
pub async fn execute_with(
    app: &mut App,
    action: Action,
    extend: bool,
    cmd_tx: &mpsc::Sender<UiCommand>,
) -> Result<(), String> {
    match action {
        Action::SubmitPrompt => {
            // Only a plain Enter submits from the prompt; in a list, Enter is `Confirm`, so
            // the manager never reaches this arm.
            crate::submit_input(app, cmd_tx, false).await;
        }
        Action::NewLine => {
            let byte = app.cursor_byte_offset(app.cursor);
            app.input.insert(byte, '\n');
            app.cursor += 1;
            app.input_changed();
        }
        Action::InterruptSend => {
            let text = app.input.trim().to_string();
            if text.is_empty() && app.queued.is_empty() && !app.status.busy {
                return Ok(());
            }
            app.input.clear();
            app.cursor = 0;
            app.input_anchor = None;
            app.cancel_armed = false;
            if text.is_empty() {
                let _ = cmd_tx.send(UiCommand::SendNow(String::new())).await;
                app.push_block(crate::Block::System("stopped — running the queue".into()));
                return Ok(());
            }
            crate::submit_input(app, cmd_tx, true).await;
        }
        Action::StopTurn => {
            // Double Escape: the first press arms, the second stops. A plain Escape while a
            // popup is open dismisses the popup instead (handled by the caller), and when
            // idle it does nothing.
            if app.status.busy {
                if app.cancel_armed {
                    app.cancel_armed = false;
                    let _ = cmd_tx.send(UiCommand::Cancel).await;
                } else {
                    app.cancel_armed = true;
                    app.popup_dismissed = true;
                    app.activity = "Esc again to stop".into();
                }
            } else if app.completion_count() > 0 {
                app.popup_dismissed = true;
                app.cancel_armed = false;
            }
        }
        Action::DeleteWord => {
            // Word delete respects an active selection first: deleting the selection is
            // what typing and plain Backspace do, and jumping elsewhere instead would
            // silently discard the text the user had highlighted.
            if let Some((lo, hi)) = app.input_selection() {
                let from = app.cursor_byte_offset(lo);
                let to = app.cursor_byte_offset(hi);
                app.input.replace_range(from..to, "");
                app.cursor = lo;
            } else if app.cursor > 0 {
                let (start, end) = crate::kill_word_left(&app.input, app.cursor);
                if end > start {
                    let from = app.cursor_byte_offset(start);
                    let to = app.cursor_byte_offset(end);
                    app.input.replace_range(from..to, "");
                    app.cursor = start;
                }
            }
            app.input_changed();
        }
        Action::WordLeft | Action::WordRight => {
            let target = if action == Action::WordLeft {
                crate::prev_word_start(&app.input, app.cursor)
            } else {
                crate::word_right(&app.input, app.cursor)
            };
            app.move_cursor(target, extend);
        }
        Action::HistorySearch => {
            if !app.open_history_search() {
                app.push_block(crate::Block::System(
                    "no prompt history yet — send a prompt first".into(),
                ));
            }
        }
        Action::HistoryPrevious => {
            if app.completion_count() == 0 && app.input.is_empty() && !app.queued.is_empty() {
                let _ = cmd_tx.send(UiCommand::TakeLastQueued).await;
            } else if app.completion_count() == 0 && app.can_recall_previous() {
                app.recall_previous();
            } else if app.completion_count() == 0 && app.input.contains('\n') {
                let (line, col) = app.cursor_line_col();
                app.move_cursor(app.offset_at(line.saturating_sub(1), col), false);
            } else if app.completion_count() == 0 {
                app.move_cursor(0, false);
            } else if app.completion_selected == 0 {
                app.completion_selected = app.completion_count().saturating_sub(1);
            } else {
                app.completion_selected -= 1;
            }
        }
        Action::HistoryNext => {
            if app.completion_count() == 0 && app.can_recall_next() {
                app.recall_next();
            } else if app.completion_count() == 0 && app.input.contains('\n') {
                let (line, col) = app.cursor_line_col();
                let last = app.input_lines().len().saturating_sub(1);
                app.move_cursor(app.offset_at((line + 1).min(last), col), false);
            } else if app.completion_count() == 0 {
                let end = app.input.chars().count();
                app.move_cursor(end, false);
            } else if app.completion_count() > 0 {
                app.completion_selected = (app.completion_selected + 1) % app.completion_count();
            }
        }
        Action::OpenPalette => app.open_palette(),
        Action::Help => {
            let _ = cmd_tx
                .send(UiCommand::Slash(loon_core::commands::SlashCommand::Help))
                .await;
        }
        Action::ExpandToolOutput => {
            app.show_tool_details = !app.show_tool_details;
            app.selection = None;
            app.selection_snapshot = None;
        }
        Action::CopyLastResponse => {
            app.pending_copy = app.blocks.iter().rev().find_map(|b| match b {
                crate::Block::Assistant(s) => Some(s.clone()),
                _ => None,
            });
        }
        Action::ToggleDetails => app.details_open = !app.details_open,
        Action::ScrollUp => {
            app.scroll_from_bottom = Some(app.scroll_from_bottom.unwrap_or(0).saturating_add(10));
        }
        Action::ScrollDown => match app.scroll_from_bottom {
            Some(0) | None => app.scroll_from_bottom = None,
            Some(n) => app.scroll_from_bottom = Some(n.saturating_sub(10)),
        },
        Action::OpenSessions => {
            let _ = cmd_tx
                .send(UiCommand::Slash(
                    loon_core::commands::SlashCommand::Resume { target: None },
                ))
                .await;
        }
        Action::OpenThemes => {
            let _ = cmd_tx
                .send(UiCommand::Slash(
                    loon_core::commands::SlashCommand::ThemeSet {
                        name: "cycle".into(),
                    },
                ))
                .await;
        }
        Action::OpenModels => {
            let _ = cmd_tx
                .send(UiCommand::Slash(loon_core::commands::SlashCommand::Model {
                    reference: None,
                }))
                .await;
        }
        Action::OpenSettings => {
            let _ = cmd_tx
                .send(UiCommand::Slash(
                    loon_core::commands::SlashCommand::Config { args: vec![] },
                ))
                .await;
        }
        Action::OpenKeybindings => app.open_keybindings(),
        Action::CyclePermissionMode => {
            let _ = cmd_tx.send(UiCommand::CycleMode).await;
        }
        Action::CancelAndQuit => {
            if app.status.busy {
                let _ = cmd_tx.send(UiCommand::Cancel).await;
                if !app.cancel_hint_shown {
                    app.push_block(crate::Block::System("stopping…".into()));
                    app.cancel_hint_shown = true;
                }
            } else if app.cancel_hint_shown {
                app.quit = true;
            } else {
                app.cancel_hint_shown = true;
                app.push_block(crate::Block::System("press Ctrl+C again to quit".into()));
            }
        }
        Action::Quit => app.quit = true,
        // Manager-scope actions are routed by the navigator, which owns selection and
        // dismissal; reaching them here means no surface is open, so they are no-ops.
        Action::Dismiss | Action::Confirm | Action::NavigatePrevious | Action::NavigateNext => {}
    }
    Ok(())
}

/// Resolve and run a chord against the active map. Returns `Unbound` when nothing matched so
/// the caller can fall through to editing.
pub async fn dispatch(
    app: &mut App,
    key: &KeyEvent,
    cmd_tx: &mpsc::Sender<UiCommand>,
) -> Result<Handled, String> {
    let Some(chord) = chord(key) else {
        return Ok(Handled::Unbound);
    };
    let scope = scope(app);
    // Shift on a modified command key is a *selection* modifier (Ctrl+Shift+Left selects the
    // word Ctrl+Left jumps over), not a separate binding: named keys carry Shift in their
    // chord, so retry without it when a Control/Alt chord did not match outright.
    let unshifted = (chord.mods.shift && (chord.mods.ctrl || chord.mods.alt)).then_some(Chord {
        code: chord.code,
        mods: Mods {
            shift: false,
            ..chord.mods
        },
    });
    let action = match app
        .keys
        .resolve(chord, scope)
        .or_else(|| unshifted.and_then(|chord| app.keys.resolve(chord, scope)))
    {
        Some(action) => action,
        None if (chord.mods.ctrl || chord.mods.alt) && chord.mods.shift => {
            // Shift is a *selection* modifier on a command that already has a binding:
            // Ctrl+Shift+Left moves by a word and extends the selection, exactly as
            // Ctrl+Left moves by a word. Look the chord up without Shift, unless the user
            // bound the shifted form explicitly (checked first, above).
            let unshifted = Chord {
                code: chord.code,
                mods: Mods {
                    shift: false,
                    ..chord.mods
                },
            };
            match app.keys.resolve(unshifted, scope) {
                Some(action) => action,
                None => return Ok(Handled::Unbound),
            }
        }
        None => return Ok(Handled::Unbound),
    };
    // Escape closes an open popup before it can stop a turn: a visible menu swallowing the
    // dismissal key would leave the user stuck.
    if action == Action::StopTurn && app.completion_count() > 0 && !app.status.busy {
        app.popup_dismissed = true;
        app.cancel_armed = false;
        return Ok(Handled::Action(action));
    }
    // Escape also closes the palette; that is a dismissal, not a stop.
    if action == Action::StopTurn && app.palette_open {
        return Ok(Handled::Unbound);
    }
    let extend = key.modifiers.contains(KeyModifiers::SHIFT);
    execute_with(app, action, extend, cmd_tx).await?;
    Ok(Handled::Action(action))
}

/// The effective map, replacing it when configuration changes at runtime.
pub fn install(app: &mut App, keys: KeyMap) {
    app.keys = keys;
}
