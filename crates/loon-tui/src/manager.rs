//! Shared searchable, windowed navigator for every management surface.
use crate::{App, UiCommand};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use loon_core::{
    catalog::{Catalog, Editor, FieldKind, MenuItem},
    commands::{self, SlashCommand},
};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use tokio::sync::mpsc;
#[derive(Debug, Clone)]
pub enum View {
    Models,
    AllModels(Option<String>),
    Providers,
    Provider(String),
    Combos,
    Combo(String),
    Mcps,
    Mcp(String),
    Menu(String, Vec<MenuItem>),
    Confirm(String),
    /// `/keybindings`: one row per action, with chord capture.
    Keybindings,
}
#[derive(Debug, Clone)]
pub enum Action {
    Open(View),
    Command(String),
    Close,
}
impl Action {
    fn command_path(&self) -> Option<String> {
        if let Self::Command(c) = self {
            let args = c.strip_prefix("/config ui edit ")?;
            Some(args.to_string())
        } else {
            None
        }
    }
}
#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub detail: String,
    pub action: Action,
}
#[derive(Debug, Clone)]
pub struct Navigator {
    pub catalog: Catalog,
    pub view: View,
    pub query: String,
    pub selected: usize,
    pub history: Vec<(View, String, usize)>,
    pub rect: Option<Rect>,
    pub start: usize,
    pub page: usize,
    /// The effective keymap, carried on the navigator so `/keybindings` rows (and the search
    /// over them) are built from the same map the dispatcher uses.
    pub keys: loon_config::KeyMap,
}
impl Navigator {
    pub fn new(catalog: Catalog, view: View) -> Self {
        Self {
            catalog,
            view,
            query: String::new(),
            selected: 0,
            history: vec![],
            rect: None,
            start: 0,
            page: 8,
            keys: loon_config::KeyMap::default(),
        }
    }
    pub fn from_catalog(catalog: Catalog, view: &str) -> Self {
        let v = match view {
            "providers" => View::Providers,
            "combos" => View::Combos,
            "mcps" => View::Mcps,
            v if v.starts_with("models:") => View::AllModels(
                v.strip_prefix("models:")
                    .filter(|p| *p != "all")
                    .map(str::to_string),
            ),
            _ => View::Models,
        };
        Self::new(catalog, v)
    }
    pub fn title(&self) -> String {
        match &self.view {
            View::Models => "Models".into(),
            View::AllModels(p) => p
                .as_ref()
                .map(|p| format!("Models / {p}"))
                .unwrap_or("Models / All providers".into()),
            View::Providers => "Providers".into(),
            View::Provider(p) => format!("Provider / {p}"),
            View::Combos => "Combos".into(),
            View::Combo(n) => format!("Combo / {n}"),
            View::Mcps => "MCP servers".into(),
            View::Mcp(n) => format!("MCP / {n}"),
            View::Menu(t, _) => t.clone(),
            View::Confirm(_) => "Confirm removal".into(),
            View::Keybindings => "Keybindings".into(),
        }
    }
    fn command(label: impl Into<String>, detail: impl Into<String>, cmd: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            detail: detail.into(),
            action: Action::Command(cmd.into()),
        }
    }
    fn open_row(label: impl Into<String>, detail: impl Into<String>, v: View) -> Row {
        Row {
            label: label.into(),
            detail: detail.into(),
            action: Action::Open(v),
        }
    }
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let model =
            |r: &String, detail: &str| Self::command(r.clone(), detail, format!("/model {r}"));
        match &self.view {
            View::Models => {
                if self.query.is_empty() {
                    for r in self.catalog.recent.iter().take(5) {
                        rows.push(model(r, "Recent"));
                    }
                }
                rows.push(Self::open_row(
                    "All models",
                    format!(
                        "{} models · type to search every provider",
                        self.catalog.models.len()
                    ),
                    View::AllModels(None),
                ));
                for p in &self.catalog.providers {
                    rows.push(Self::open_row(
                        p.name.clone(),
                        format!("Provider · {} models · {}", p.count, p.api),
                        View::AllModels(Some(p.name.clone())),
                    ));
                }
                for c in &self.catalog.combos {
                    rows.push(Self::command(
                        format!("combo:{}", c.name),
                        format!("Combo · {} · {} members", c.mode, c.members.len()),
                        format!("/model combo:{}", c.name),
                    ));
                }
                if !self.query.is_empty() {
                    for r in &self.catalog.models {
                        rows.push(model(r, "Model"));
                    }
                }
                rows.push(Self::command(
                    "Refresh model catalog",
                    "Fetch /models from all providers",
                    "/providers refresh all",
                ));
                rows.push(Self::command(
                    "Manage providers",
                    "Add, edit, test or remove",
                    "/providers",
                ));
                rows.push(Self::command(
                    "Manage combos",
                    "Fallback and round-robin",
                    "/combos",
                ));
            }
            View::AllModels(provider) => {
                for r in &self.catalog.models {
                    if provider
                        .as_ref()
                        .map(|p| r.starts_with(&format!("{p}/")))
                        .unwrap_or(true)
                    {
                        rows.push(model(r, "Enter to select"));
                    }
                }
                rows.push(Self::command(
                    "Refresh models",
                    "Discover this provider's available models",
                    format!(
                        "/providers refresh {}",
                        provider.as_deref().unwrap_or("all")
                    ),
                ));
            }
            View::Providers => {
                rows.push(Self::command(
                    "+ Add provider",
                    "Endpoint, API family, credentials and reasoning support",
                    "/providers add",
                ));
                rows.push(Self::command(
                    "Refresh all",
                    "Fetch models from every configured endpoint",
                    "/providers refresh all",
                ));
                for p in &self.catalog.providers {
                    rows.push(Self::open_row(
                        p.name.clone(),
                        format!("{} · {} models · {}", p.api, p.count, p.url),
                        View::Provider(p.name.clone()),
                    ));
                }
            }
            View::Provider(name) => {
                let p = self.catalog.providers.iter().find(|p| p.name == *name);
                rows.push(Self::open_row(
                    "Browse models",
                    p.map(|p| p.url.clone()).unwrap_or_default(),
                    View::AllModels(Some(name.clone())),
                ));
                rows.push(Self::command(
                    "Refresh / test connection",
                    "Fetch advertised models; show errors",
                    format!("/providers refresh {name}"),
                ));
                rows.push(Self::command(
                    "Edit provider",
                    "Blank key keeps the saved credential",
                    format!("/providers edit {name}"),
                ));
                rows.push(Self::open_row(
                    "Remove provider",
                    "Refused while selected or used by a combo",
                    View::Confirm(format!("/providers remove {name}")),
                ));
            }
            View::Combos => {
                rows.push(Self::command(
                    "+ Add combo",
                    "Ordered model members and routing mode",
                    "/combos add",
                ));
                for c in &self.catalog.combos {
                    rows.push(Self::open_row(
                        c.name.clone(),
                        format!("{} · {}", c.mode, c.members.join(" → ")),
                        View::Combo(c.name.clone()),
                    ));
                }
            }
            View::Combo(name) => {
                if let Some(c) = self.catalog.combos.iter().find(|c| c.name == *name) {
                    rows.push(Self::command(
                        "Use combo",
                        format!("{} · {}", c.mode, c.members.join(" → ")),
                        format!("/model combo:{name}"),
                    ));
                    rows.push(Self::command(
                        "Edit mode and members",
                        "Member order sets fallback priority / rotation",
                        format!("/combos edit {name}"),
                    ));
                    rows.push(Self::open_row(
                        "Remove combo",
                        "Current combo must be switched first",
                        View::Confirm(format!("/combos remove {name}")),
                    ));
                }
            }
            View::Mcps => {
                rows.push(Self::command(
                    "+ Add MCP server",
                    "Command, arguments, environment and timeout",
                    "/mcp add",
                ));
                rows.push(Self::command(
                    "Reconnect all",
                    "Restart every server and refresh its tools",
                    "/mcp reload",
                ));
                for m in &self.catalog.mcps {
                    rows.push(Self::open_row(
                        m.name.clone(),
                        if m.error.is_empty() {
                            format!("{} tools · {} {}", m.tools, m.command, m.args.join(" "))
                        } else {
                            format!("error: {}", m.error)
                        },
                        View::Mcp(m.name.clone()),
                    ));
                }
            }
            View::Mcp(name) => {
                if let Some(m) = self.catalog.mcps.iter().find(|m| m.name == *name) {
                    rows.push(Self::command(
                        "Reconnect",
                        format!("{} tools currently available", m.tools),
                        "/mcp reload",
                    ));
                    rows.push(Self::command(
                        "Edit server",
                        format!("{} {}", m.command, m.args.join(" ")),
                        format!("/mcp edit {name}"),
                    ));
                    rows.push(Self::open_row(
                        "Remove server",
                        "Its tools disappear from the next request",
                        View::Confirm(format!("/mcp remove {name}")),
                    ));
                    for spec in &self.catalog.tools {
                        if spec.starts_with(&format!("mcp_{name}_")) {
                            rows.push(Row {
                                label: spec.clone(),
                                detail: "tool".into(),
                                action: Action::Close,
                            });
                        }
                    }
                }
            }
            View::Keybindings => {
                // Rows come from the live map, so a capture/custom marker is always truthful.
                for (action, chord, description, custom) in self.keys.rows() {
                    let marker = if custom { "✎ " } else { "" };
                    rows.push(Row {
                        label: format!("{marker}{action} = {chord}"),
                        detail: description,
                        action: Action::Open(View::Keybindings),
                    });
                }
            }
            View::Menu(_, items) => rows.extend(
                items
                    .iter()
                    .map(|i| Self::command(i.label.clone(), i.detail.clone(), i.command.clone())),
            ),
            View::Confirm(cmd) => {
                rows.push(Row {
                    label: "Cancel".into(),
                    detail: "Keep this item".into(),
                    action: Action::Close,
                });
                rows.push(Self::command("Remove", cmd.clone(), cmd.clone()));
            }
        }
        let words = self
            .query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut rows: Vec<_> = rows
            .into_iter()
            .filter(|r| {
                let t = format!("{} {}", r.label, r.detail).to_lowercase();
                words.iter().all(|w| t.contains(w))
            })
            .collect();
        // Prefer matches in the visible label over incidental matches in help text.
        if !words.is_empty() {
            rows.sort_by_key(|r| {
                let label = r.label.to_lowercase();
                std::cmp::Reverse(words.iter().filter(|w| label.contains(w.as_str())).count())
            });
        }
        rows
    }
    pub fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        self.selected = if len == 0 {
            0
        } else {
            (self.selected as isize + delta).clamp(0, len as isize - 1) as usize
        };
    }
    pub fn open(&mut self, v: View) {
        self.history
            .push((self.view.clone(), self.query.clone(), self.selected));
        self.view = v;
        self.query.clear();
        self.selected = 0;
    }
    pub fn back(&mut self) -> bool {
        if let Some((v, q, s)) = self.history.pop() {
            self.view = v;
            self.query = q;
            self.selected = s;
            true
        } else {
            false
        }
    }
}
/// Multi-select overlay for an editor field of kind [`FieldKind::Multi`].
#[derive(Debug, Clone, Default)]
pub struct Picker {
    pub query: String,
    pub selected: usize,
    pub start: usize,
    pub page: usize,
    pub rect: Option<Rect>,
}
#[derive(Debug, Clone)]
pub struct Form {
    pub editor: Editor,
    pub focused: usize,
    pub cursor: usize,
    pub error: String,
    pub picker: Option<Picker>,
    pub rect: Option<Rect>,
    pub first: usize,
}
impl Form {
    pub fn new(editor: Editor) -> Self {
        let focused = if editor.editing { 1 } else { 0 };
        let cursor = editor
            .values
            .get(focused)
            .map(|v| v.chars().count())
            .unwrap_or(0);
        Self {
            editor,
            focused,
            cursor,
            error: String::new(),
            picker: None,
            rect: None,
            first: 0,
        }
    }
    pub fn labels(&self) -> Vec<&'static str> {
        match self.editor.kind.as_str() {
            "provider" => vec![
                "Name",
                "API base URL",
                "API key (optional)",
                "API family (←/→)",
                "Key environment variable",
                "Reasoning levels (CSV)",
            ],
            "combo" => vec!["Name", "Mode (←/→)", "Members (Enter to pick)"],
            "mcp_http" => vec!["Name", "Port or URL (e.g. 3000)", "Timeout seconds"],
            "mcp" => vec![
                "Name",
                "Port / URL / command",
                "Arguments (space separated)",
                "Environment (K=V, space separated)",
                "Timeout seconds",
            ],
            "config-value" => vec!["Setting", "Value (empty = default for optional fields)"],
            "config-secret" => vec![
                "Credential setting",
                "New value (blank keeps saved value)",
                "Action (←/→)",
            ],
            "config-entry" => vec!["Collection", "New entry name"],
            "todo" => vec!["Todo", "Text"],
            "note" => vec!["Note", "Text"],
            _ => vec!["Setting", "Value"],
        }
    }
    fn editable(&self, i: usize) -> bool {
        !(i == 0 && self.editor.editing)
    }
    fn kind(&self, i: usize) -> FieldKind {
        self.editor
            .fields
            .get(i)
            .map(|f| f.kind)
            .unwrap_or_default()
    }
    fn focused_kind(&self) -> FieldKind {
        self.kind(self.focused)
    }
    fn choices(&self) -> &[String] {
        self.editor
            .fields
            .get(self.focused)
            .map(|f| f.choices.as_slice())
            .unwrap_or(&[])
    }
    pub fn focus(&mut self, delta: isize) {
        let n = self.editor.values.len();
        loop {
            self.focused = (self.focused as isize + delta).rem_euclid(n as isize) as usize;
            if self.editable(self.focused) {
                break;
            }
        }
        self.cursor = self.editor.values[self.focused].chars().count();
    }
    /// ←/→ on a choice field: wrap through its values.
    fn cycle(&mut self, delta: isize) {
        let next = {
            let choices = self.choices();
            if choices.is_empty() {
                return;
            }
            let current = self.editor.values[self.focused].as_str();
            let idx = choices.iter().position(|c| c == current).unwrap_or(0);
            let n = choices.len() as isize;
            choices[(idx as isize + delta).rem_euclid(n) as usize].clone()
        };
        self.editor.values[self.focused] = next;
        self.cursor = self.editor.values[self.focused].chars().count();
    }
    fn members(&self) -> Vec<String> {
        self.editor.values[self.focused]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
    fn toggle_member(&mut self, item: &str) {
        let mut values = self.members();
        match values.iter().position(|v| v == item) {
            Some(pos) => {
                values.remove(pos);
            }
            None => values.push(item.to_string()),
        }
        self.editor.values[self.focused] = values.join(",");
        self.cursor = self.editor.values[self.focused].chars().count();
    }
    fn open_picker(&mut self) {
        if self.focused_kind() == FieldKind::Multi {
            self.picker = Some(Picker::default());
        }
    }
    fn picker_rows(&self) -> Vec<String> {
        let query = self
            .picker
            .as_ref()
            .map(|p| p.query.to_lowercase())
            .unwrap_or_default();
        self.choices()
            .iter()
            .filter(|c| c.to_lowercase().contains(&query))
            .cloned()
            .collect()
    }
    fn picker_move(&mut self, delta: isize) {
        let len = self.picker_rows().len();
        if let Some(p) = self.picker.as_mut() {
            p.selected = if len == 0 {
                0
            } else {
                (p.selected as isize + delta).clamp(0, len as isize - 1) as usize
            };
        }
    }
    fn picker_toggle(&mut self) {
        let rows = self.picker_rows();
        let idx = self.picker.as_ref().map(|p| p.selected).unwrap_or(0);
        if let Some(item) = rows.get(idx).cloned() {
            self.toggle_member(&item);
        }
    }
    fn picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => self.picker = None,
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Down => self.picker_move(1),
            KeyCode::PageUp => {
                let page = self.picker.as_ref().map(|p| p.page.max(1)).unwrap_or(10) as isize;
                self.picker_move(-page);
            }
            KeyCode::PageDown => {
                let page = self.picker.as_ref().map(|p| p.page.max(1)).unwrap_or(10) as isize;
                self.picker_move(page);
            }
            KeyCode::Home => {
                if let Some(p) = self.picker.as_mut() {
                    p.selected = 0;
                }
            }
            KeyCode::End => {
                let len = self.picker_rows().len();
                if let Some(p) = self.picker.as_mut() {
                    p.selected = len.saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = self.picker.as_mut() {
                    p.query.pop();
                    p.selected = 0;
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.picker_toggle(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(p) = self.picker.as_mut() {
                    p.query.push(c);
                    p.selected = 0;
                }
            }
            _ => {}
        }
    }
    pub fn insert(&mut self, text: &str) {
        if self.focused_kind() != FieldKind::Text {
            return;
        }
        let v = &mut self.editor.values[self.focused];
        let b = byte_at(v, self.cursor);
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take(4096)
            .collect();
        v.insert_str(b, &text);
        self.cursor += text.chars().count();
    }
    fn command(&self) -> Result<SlashCommand, String> {
        let v = &self.editor.values;
        match self.editor.kind.as_str() {
            "config-value" | "config-secret" | "config-entry" => Ok(SlashCommand::Config {
                args: vec![
                    "ui".into(),
                    if self.editor.kind == "config-entry" {
                        "insert"
                    } else {
                        "set"
                    }
                    .into(),
                    loon_core::config_ui::encode(&v[0]),
                    v[1].clone(),
                    v.get(2).cloned().unwrap_or_default(),
                ],
            }),
            "provider" => {
                if v[0].trim().is_empty() || v[1].trim().is_empty() {
                    return Err("Name and API base URL are required".into());
                }
                let mut args = vec![if self.editor.editing { "edit" } else { "add" }.into()];
                args.extend(v.iter().map(|s| {
                    if s.trim().is_empty() {
                        "-".into()
                    } else {
                        s.trim().into()
                    }
                }));
                Ok(SlashCommand::Provider { args })
            }
            "combo" => {
                if v[0].trim().is_empty() || v[2].trim().is_empty() {
                    return Err("Name and at least one member are required".into());
                }
                Ok(SlashCommand::Combo {
                    args: vec![
                        if self.editor.editing { "edit" } else { "add" }.into(),
                        v[0].trim().into(),
                        v[1].trim().into(),
                        v[2].trim().into(),
                    ],
                })
            }
            "mcp_http" => {
                if v[0].trim().is_empty() || v[1].trim().is_empty() {
                    return Err("Name and port/URL are required".into());
                }
                Ok(SlashCommand::Mcp {
                    args: vec![
                        if self.editor.editing {
                            "edit-http"
                        } else {
                            "add-http"
                        }
                        .into(),
                        v[0].trim().into(),
                        v[1].trim().into(),
                        v[2].trim().into(),
                    ],
                })
            }
            "mcp" => {
                if v[0].trim().is_empty() || v[1].trim().is_empty() {
                    return Err("Name and a port, URL or command are required".into());
                }
                let mut args = vec![
                    if self.editor.editing { "edit" } else { "add" }.into(),
                    v[0].trim().into(),
                    v[1].trim().into(),
                ];
                args.extend(
                    v[2].split_whitespace()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
                Ok(SlashCommand::Mcp { args })
            }
            "todo" | "note" => {
                if v[1].trim().is_empty() {
                    return Err("Text is required".into());
                }
                let args = if v[0] == "new" {
                    vec!["add".into(), v[1].clone()]
                } else {
                    vec!["edit".into(), v[0].clone(), v[1].clone()]
                };
                Ok(if self.editor.kind == "todo" {
                    SlashCommand::Todos { args }
                } else {
                    SlashCommand::Notes { args }
                })
            }
            _ => Ok(SlashCommand::Config {
                args: if self.editor.kind == "setting" {
                    vec!["menu-set".into(), v[0].clone(), v[1].clone()]
                } else {
                    vec![v[0].clone(), v[1].clone()]
                },
            }),
        }
    }
}
fn byte_at(text: &str, c: usize) -> usize {
    text.char_indices()
        .nth(c)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}
pub async fn key(app: &mut App, key: KeyEvent, tx: &mpsc::Sender<UiCommand>) -> bool {
    if app.navigator.is_some() {
        app.manager_notice.clear();
    }
    preview_theme(app);
    if let Some(mut form) = app.editor.take() {
        if form.picker.is_some() {
            form.picker_key(key);
            app.editor = Some(form);
            return true;
        }
        let mut close = false;
        let text_field = form.focused_kind() == FieldKind::Text;
        match key.code {
            KeyCode::Esc => close = true,
            KeyCode::Tab | KeyCode::Down => form.focus(1),
            KeyCode::BackTab | KeyCode::Up => form.focus(-1),
            KeyCode::Left if form.focused_kind() == FieldKind::Choice => form.cycle(-1),
            KeyCode::Right if form.focused_kind() == FieldKind::Choice => form.cycle(1),
            KeyCode::Left => form.cursor = form.cursor.saturating_sub(1),
            KeyCode::Right => {
                form.cursor =
                    (form.cursor + 1).min(form.editor.values[form.focused].chars().count())
            }
            KeyCode::Home if text_field => form.cursor = 0,
            KeyCode::End if text_field => {
                form.cursor = form.editor.values[form.focused].chars().count()
            }
            KeyCode::Backspace if text_field => {
                if form.cursor > 0 {
                    let v = &mut form.editor.values[form.focused];
                    let a = byte_at(v, form.cursor - 1);
                    let b = byte_at(v, form.cursor);
                    v.replace_range(a..b, "");
                    form.cursor -= 1;
                }
            }
            KeyCode::Delete if text_field => {
                let v = &mut form.editor.values[form.focused];
                let a = byte_at(v, form.cursor);
                let b = byte_at(v, form.cursor + 1);
                v.replace_range(a..b, "");
            }
            KeyCode::Enter if form.focused_kind() == FieldKind::Multi => form.open_picker(),
            KeyCode::Enter if form.focused + 1 < form.editor.values.len() => form.focus(1),
            KeyCode::Enter | KeyCode::Char('s')
                if key.code == KeyCode::Enter || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                match form.command() {
                    Ok(c) => {
                        app.pending_editor = Some(form.clone());
                        let _ = tx.send(UiCommand::Slash(c)).await;
                        close = true;
                    }
                    Err(e) => form.error = e,
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                form.insert(&c.to_string())
            }
            _ => {}
        }
        if !close {
            app.editor = Some(form);
        } else if let Some(settings) = app.settings_return.take() {
            app.navigator = Some(settings);
        }
        return true;
    }
    if let Some(mut nav) = app.navigator.take() {
        // Quit is global: a modal surface must not trap the user in the application.
        if key.code == KeyCode::Char('q')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && app
                .keys
                .resolve(loon_config::Chord::ctrl('q'), loon_config::Scope::Manager)
                .is_some()
        {
            app.quit = true;
            app.navigator = Some(nav);
            return true;
        }
        // The keymap lives on the app; mirror it onto the navigator so rows and capture agree.
        nav.keys = app.keys.clone();
        if matches!(nav.view, View::Keybindings) {
            let handled = keybindings_key(app, &mut nav, key, tx).await;
            app.navigator = Some(nav);
            return handled;
        }
        let appearance = matches!(&nav.view,View::Menu(t,_) if t=="Appearance");
        let mut close = false;
        let settings = matches!(&nav.view,View::Menu(title,_) if title=="Settings" || title.starts_with("Settings / "));
        match key.code {
            KeyCode::Left | KeyCode::Right if settings => {
                if let Some(row) = nav.rows().get(nav.selected).cloned() {
                    if let Some(path) = row.action.clone().command_path() {
                        let _ = tx
                            .send(UiCommand::Slash(SlashCommand::Config {
                                args: vec![
                                    "ui".into(),
                                    "step".into(),
                                    path,
                                    if key.code == KeyCode::Left { "-1" } else { "1" }.into(),
                                ],
                            }))
                            .await;
                    } else if let Some((setting, current)) = row.label.split_once(" = ") {
                        if let Some(def) = commands::setting_def(setting) {
                            let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                            let choices = def.kind.suggestions();
                            let next = if !choices.is_empty() {
                                let index =
                                    choices.iter().position(|v| v == current).unwrap_or(0) as isize;
                                Some(
                                    choices[(index + delta).rem_euclid(choices.len() as isize)
                                        as usize]
                                        .clone(),
                                )
                            } else if let commands::SettingKind::Int { min, max } = def.kind {
                                let n = current.parse::<u64>().unwrap_or(min);
                                let step = if setting == "autocompact_percent" {
                                    5
                                } else if setting == "autocompact_tokens"
                                    || setting == "context_window"
                                {
                                    1000
                                } else if setting == "tool_output_bytes" {
                                    1024
                                } else {
                                    1
                                };
                                Some(
                                    if delta < 0 {
                                        n.saturating_sub(step).max(min)
                                    } else {
                                        n.saturating_add(step).min(max)
                                    }
                                    .to_string(),
                                )
                            } else {
                                None
                            };
                            if let Some(mut next) = next {
                                if setting == "autocompact_tokens" {
                                    let n = next.parse::<u64>().unwrap_or(0);
                                    if n > 0 && n < 1024 {
                                        next = if delta > 0 { "1024".into() } else { "0".into() };
                                    }
                                }
                                if let View::Menu(_, items) = &mut nav.view {
                                    if let Some(item) =
                                        items.iter_mut().find(|item| item.label == row.label)
                                    {
                                        item.label = format!("{setting} = {next}");
                                    }
                                }
                                let _ = tx
                                    .send(UiCommand::Slash(SlashCommand::Config {
                                        args: vec!["menu-set".into(), setting.into(), next],
                                    }))
                                    .await;
                            }
                        }
                    }
                }
            }
            KeyCode::Char('d') if settings && key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(row) = nav.rows().get(nav.selected) {
                    if let Action::Command(c) = &row.action {
                        if let Some(Ok(SlashCommand::Config { args })) = commands::parse(c) {
                            if args.first().is_some_and(|a| a == "ui")
                                && args.get(1).is_some_and(|a| a == "open" || a == "edit")
                            {
                                let _ = tx
                                    .send(UiCommand::Slash(SlashCommand::Config {
                                        args: vec![
                                            "ui".into(),
                                            "remove".into(),
                                            args.get(2).cloned().unwrap_or_default(),
                                        ],
                                    }))
                                    .await;
                            }
                        }
                    }
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                nav.query.clear();
                nav.selected = 0;
            }
            KeyCode::Esc => {
                if !nav.query.is_empty() {
                    nav.query.clear();
                    nav.selected = 0;
                } else if !nav.back() {
                    close = true;
                }
            }
            KeyCode::Left if nav.query.is_empty() => {
                if !nav.back() {
                    close = true;
                }
            }
            KeyCode::Up => nav.move_by(-1),
            KeyCode::Down => nav.move_by(1),
            KeyCode::PageUp => nav.move_by(-(nav.page as isize)),
            KeyCode::PageDown => nav.move_by(nav.page as isize),
            KeyCode::Home => nav.selected = 0,
            KeyCode::End => nav.selected = nav.rows().len().saturating_sub(1),
            KeyCode::Backspace => {
                nav.query.pop();
                nav.selected = 0;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = tx
                    .send(UiCommand::Slash(SlashCommand::Provider {
                        args: vec!["refresh".into(), "all".into()],
                    }))
                    .await;
            }
            KeyCode::Enter => {
                if let Some(row) = nav.rows().get(nav.selected).cloned() {
                    match row.action {
                        Action::Open(v) => {
                            nav.open(v);
                            app.modal_age = 0;
                        }
                        Action::Close => {
                            if !nav.back() {
                                close = true;
                            }
                        }
                        Action::Command(c) => {
                            let c = if appearance && app.settings_return.is_some() {
                                format!(
                                    "/config menu-set theme {}",
                                    nav.rows()
                                        .get(nav.selected)
                                        .map(|r| r.label.as_str())
                                        .unwrap_or("terminal")
                                )
                            } else if app.settings_return.is_some() && c.starts_with("/model ") {
                                format!(
                                    "/config menu-set model {}",
                                    c.trim_start_matches("/model ")
                                )
                            } else {
                                c
                            };
                            if let Some(Ok(c)) = commands::parse(&c) {
                                if appearance {
                                    app.theme_restore = None;
                                }
                                let keep = settings && matches!(&c, SlashCommand::Config { .. });
                                let _ = tx.send(UiCommand::Slash(c)).await;
                                close = !keep;
                            }
                        }
                    }
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                nav.query.push(c);
                nav.selected = 0;
            }
            _ => {}
        }
        if !close {
            app.navigator = Some(nav);
        } else {
            if let Some((name, transparent)) = app.theme_restore.take() {
                app.palette = name;
                app.transparent = transparent;
            }
            if let Some(settings) = app.settings_return.take() {
                app.navigator = Some(settings);
            }
        }
        preview_theme(app);
        return true;
    }
    false
}
/// Handle one key in the `/keybindings` editor.
///
/// Enter starts a capture, the next key press becomes the binding, and the result is written
/// through `/config` so it persists exactly like any other setting. Captures are checked
/// before they are stored: a reserved chord is refused with the reason, and a chord another
/// action already owns asks the user to confirm with a second Enter rather than silently
/// stealing it.
async fn keybindings_key(
    app: &mut App,
    nav: &mut Navigator,
    key: KeyEvent,
    tx: &mpsc::Sender<UiCommand>,
) -> bool {
    let capture = app.pending_key_capture.take();
    // ── capture mode ────────────────────────────────────────────────────────────
    if let Some(action) = capture {
        if key.code == KeyCode::Esc {
            app.manager_notice = "binding cancelled".into();
            return true;
        }
        let chord = crate::keys::chord(&key);
        let Some(chord) = chord else {
            app.manager_notice = "that key cannot be bound".into();
            return true;
        };
        let label = crate::keys::chord_label(&key);
        // Resolve who else owns it, so the user sees the consequence before it happens.
        let owner = app
            .keys
            .bindings()
            .iter()
            .find(|b| b.action != action && b.chord == chord && b.action.scope() == action.scope())
            .map(|b| b.action);
        let next = if owner.is_some() {
            app.keys.steal(action, chord)
        } else {
            app.keys.rebind(action, chord)
        };
        match next {
            Ok(map) => {
                let stolen = owner
                    .map(|o| format!(" (took it from {})", o.id()))
                    .unwrap_or_default();
                save_keymap(app, &map, format!("{} {label}", action.id()), tx).await;
                app.keys = map;
                app.manager_notice = format!("{} bound to {label}{stolen}", action.id());
            }
            Err(e) => app.manager_notice = e.to_string(),
        }
        return true;
    }
    // ── browse mode ─────────────────────────────────────────────────────────────
    match key.code {
        KeyCode::Esc => {
            if !nav.query.is_empty() {
                nav.query.clear();
                nav.selected = 0;
            } else {
                app.navigator = None;
            }
        }
        KeyCode::Up => nav.move_by(-1),
        KeyCode::Down => nav.move_by(1),
        KeyCode::PageUp => nav.move_by(-(nav.page as isize)),
        KeyCode::PageDown => nav.move_by(nav.page as isize),
        KeyCode::Home => nav.selected = 0,
        KeyCode::End => nav.selected = nav.rows().len().saturating_sub(1),
        KeyCode::Backspace => {
            nav.query.pop();
            nav.selected = 0;
        }
        KeyCode::Enter => {
            if let Some(action) = selected_action(nav) {
                app.pending_key_capture = Some(action);
                app.manager_notice = format!("press the new key for {} (Esc cancels)", action.id());
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(action) = selected_action(nav) {
                let map = app.keys.unbind(action);
                save_keymap(app, &map, format!("{} unbind", action.id()), tx).await;
                app.keys = map;
                app.manager_notice = format!("{} unbound", action.id());
            }
        }
        KeyCode::Char('r')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            let map = app.keys.reset_all();
            save_keymap(app, &map, "reset all".to_string(), tx).await;
            app.keys = map;
            app.manager_notice = "all bindings reset to the profile defaults".into();
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(action) = selected_action(nav) {
                let map = app.keys.reset(action);
                save_keymap(app, &map, format!("{} reset", action.id()), tx).await;
                app.keys = map;
                app.manager_notice = format!("{} restored to its default", action.id());
            }
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Export the effective map as text in the transcript, ready to paste into config.
            app.push_block(crate::Block::System(app.keys.export()));
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            nav.query.push(c);
            nav.selected = 0;
        }
        _ => {}
    }
    nav.keys = app.keys.clone();
    true
}

fn selected_action(nav: &Navigator) -> Option<loon_config::KeyAction> {
    let rows = nav.rows();
    let row = rows.get(nav.selected)?;
    let id = row.label.trim_start_matches('✎').trim();
    loon_config::KeyAction::parse(id.split(" = ").next().unwrap_or(id))
}

/// Persist one keymap change through the real `/keybind` command, which validates against the
/// effective configuration and writes the user config file. The UI's own map is only updated
/// after the write succeeds — the agent answers through the slash-result channel, and a refusal
/// leaves the old map in place.
async fn save_keymap(
    app: &mut App,
    _map: &loon_config::KeyMap,
    argument: String,
    tx: &mpsc::Sender<UiCommand>,
) {
    let args: Vec<String> = argument.split_whitespace().map(str::to_string).collect();
    let _ = tx
        .send(UiCommand::Slash(SlashCommand::Keybind { args }))
        .await;
    app.manager_notice = "saving…".into();
}

/// Consume mouse events while a modal is open; never select transcript through it.
pub fn mouse(app: &mut App, m: MouseEvent) -> bool {
    if let Some(form) = app.editor.as_mut() {
        if form.picker.is_some() {
            match m.kind {
                MouseEventKind::ScrollUp => form.picker_move(-3),
                MouseEventKind::ScrollDown => form.picker_move(3),
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Moved => {
                    let start = form.picker.as_ref().map(|p| p.start).unwrap_or(0);
                    if let Some(rect) = form.picker.as_ref().and_then(|p| p.rect) {
                        if m.column >= rect.x
                            && m.column < rect.right()
                            && m.row >= rect.y
                            && m.row < rect.bottom()
                        {
                            let i = start + (m.row - rect.y) as usize;
                            if i < form.picker_rows().len() {
                                if let Some(p) = form.picker.as_mut() {
                                    p.selected = i;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        } else {
            match m.kind {
                MouseEventKind::ScrollUp => form.focus(-1),
                MouseEventKind::ScrollDown => form.focus(1),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(rect) = form.rect {
                        if rect.contains((m.column, m.row).into()) {
                            let i = form.first + (m.row - rect.y) as usize / 2;
                            if i < form.editor.values.len() && form.editable(i) {
                                form.focused = i;
                                form.cursor = form.editor.values[i].chars().count();
                                if form.focused_kind() == FieldKind::Choice {
                                    form.cycle(1);
                                } else if form.focused_kind() == FieldKind::Multi {
                                    form.open_picker();
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return true;
    }
    let Some(nav) = app.navigator.as_mut() else {
        return false;
    };
    match m.kind {
        MouseEventKind::ScrollUp => nav.move_by(-3),
        MouseEventKind::ScrollDown => nav.move_by(3),
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Moved => {
            if let Some(rect) = nav.rect {
                if m.column >= rect.x
                    && m.column < rect.right()
                    && m.row >= rect.y
                    && m.row < rect.bottom()
                {
                    let i = nav.start + (m.row - rect.y) as usize;
                    if i < nav.rows().len() {
                        nav.selected = i;
                    }
                }
            }
        }
        _ => {}
    }
    true
}
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

pub fn draw(f: &mut Frame, app: &mut App, theme: &crate::theme::Theme) {
    let area = f.area();
    let motion_tick = if app.animations { app.tick } else { 0 };
    let entrance = if app.animations {
        2u16.saturating_sub((app.modal_age / 2) as u16)
    } else {
        0
    };
    f.buffer_mut().set_style(area, theme.dim());
    if let Some(nav) = &mut app.navigator {
        nav.rect = None;
        let mut rect = centered(
            area,
            94,
            ((nav.rows().len() + 9).min(24) as u16)
                .min(area.height.saturating_sub(2))
                .max(6),
        );
        rect.y = rect
            .y
            .saturating_add(entrance)
            .min(area.bottom().saturating_sub(rect.height));
        f.render_widget(Clear, rect);
        let steady = theme.accent();
        let block = Block::default()
            .title(format!(
                " {} · {} ",
                nav.title(),
                if nav.history.is_empty() {
                    "browse"
                } else {
                    "Esc to go back"
                }
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(steady)
            .style(theme.base());
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        if inner.height < 7 || inner.width < 4 {
            return;
        }
        f.render_widget(
            Paragraph::new(format!("  Search: {}", nav.query)).style(theme.accent()),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let rows = nav.rows();
        nav.selected = nav.selected.min(rows.len().saturating_sub(1));
        let page = inner.height.saturating_sub(6).max(1) as usize;
        nav.page = page;
        let start = nav.selected.saturating_sub(page - 1);
        nav.start = start;
        nav.rect = Some(Rect::new(inner.x, inner.y + 2, inner.width, page as u16));
        let tag_w = if inner.width >= 40 { 12 } else { 0 };
        let name_w = inner.width.saturating_sub(5 + tag_w) as usize;
        let text = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(page)
            .map(|(i, r)| {
                let selected = i == nav.selected;
                let style = if selected {
                    theme.menu_selected()
                } else {
                    theme.base()
                };
                let tag = if r.detail == "Recent" {
                    "recent"
                } else if r.detail.starts_with("Provider") {
                    "provider"
                } else if r.detail.starts_with("Combo") {
                    "combo"
                } else if r.detail == "Model" || r.detail == "Enter to select" {
                    "model"
                } else {
                    match r.action {
                        Action::Open(_) => "browse",
                        _ => "",
                    }
                };
                // The effort menu's `ultra` row shimmers like the Loom wordmark while the
                // cursor rests on it: each glyph gets the logo's own travelling hue sweep
                // (fg only, never bg, so transparency is untouched). Only the letters
                // shimmer — the padding, marker and underline stay exactly as selected.
                let label_text = format!("{:<width$}", clip(&r.label, name_w), width = name_w);
                let label = if selected
                    && nav.title() == "Coding effort"
                    && r.label.trim() == "ultra"
                    && app.animations
                {
                    Line::from(
                        r.label
                            .trim()
                            .chars()
                            .enumerate()
                            .map(|(col, ch)| {
                                Span::styled(
                                    ch.to_string(),
                                    style.fg(crate::motion::logo_gradient(app.tick, col, 1)),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                } else {
                    Line::from(vec![Span::styled(label_text, style)])
                };
                Line::from(
                    [
                        vec![Span::styled(if selected { " › " } else { "   " }, style)],
                        label.spans,
                        vec![Span::styled(
                            if tag_w > 0 {
                                format!("  {tag}")
                            } else {
                                String::new()
                            },
                            theme.dim(),
                        )],
                    ]
                    .concat(),
                )
                .style(style)
            })
            .collect::<Vec<_>>();
        f.render_widget(Paragraph::new(text).style(theme.base()), nav.rect.unwrap());
        if rows.is_empty() {
            f.render_widget(
                Paragraph::new("  No matches. Try another search; Ctrl+R refreshes models.")
                    .style(theme.dim()),
                nav.rect.unwrap(),
            );
        }
        if rows.len() > page && nav.rect.unwrap().height > 0 {
            let track = nav.rect.unwrap();
            let thumb = (start * track.height as usize / rows.len())
                .min(track.height.saturating_sub(1) as usize) as u16;
            for y in 0..track.height {
                f.render_widget(
                    Paragraph::new(if y == thumb { "┃" } else { "│" }).style(if y == thumb {
                        theme.accent()
                    } else {
                        theme.dim()
                    }),
                    Rect::new(inner.right().saturating_sub(1), track.y + y, 1, 1),
                );
            }
        }
        let info = if app.manager_notice.is_empty() {
            rows.get(nav.selected)
                .map(|r| r.detail.as_str())
                .unwrap_or("")
        } else {
            &app.manager_notice
        };
        f.render_widget(
            Paragraph::new(format!("  {info}"))
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(theme.dim()),
            Rect::new(inner.x, inner.bottom().saturating_sub(4), inner.width, 2),
        );
        if matches!(&nav.view,View::Menu(title,_) if title=="Settings" || title.starts_with("Settings / "))
        {
            f.render_widget(
                Paragraph::new(
                    "  ←/→ adjust · Enter edit · Ctrl+D remove entry · Ctrl+U clear search",
                )
                .style(theme.accent()),
                Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1),
            );
        }
        f.render_widget(
            Paragraph::new(format!(
                "  ↑↓ move · Enter select · Esc back   {}/{}",
                if rows.is_empty() { 0 } else { nav.selected + 1 },
                rows.len()
            ))
            .style(theme.dim()),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
    if let Some(form) = &mut app.editor {
        form.rect = None;
        if let Some(picker) = &mut form.picker {
            picker.rect = None;
        }
        let labels = form.labels();
        let rect = centered(area, 88, (labels.len() * 2 + 6) as u16);
        f.render_widget(Clear, rect);
        let block = Block::default()
            .title(format!(
                " {} {} ",
                if form.editor.editing { "Edit" } else { "Add" },
                form.editor.kind
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if app.animations {
                theme.accent().fg(crate::motion::pulse(motion_tick))
            } else {
                theme.accent()
            })
            .style(theme.base());
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        if inner.height < 5 || inner.width < 5 {
            return;
        }
        let first = form
            .focused
            .saturating_sub((inner.height.saturating_sub(3) as usize / 2).saturating_sub(1));
        form.first = first;
        form.rect = Some(Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(3),
        ));
        let mut lines = Vec::new();
        for (i, label) in labels
            .iter()
            .enumerate()
            .skip(first)
            .take(inner.height.saturating_sub(3) as usize / 2)
        {
            let value = &form.editor.values[i];
            let kind = form
                .editor
                .fields
                .get(i)
                .map(|s| s.kind)
                .unwrap_or_default();
            let secret = (form.editor.kind == "provider" && i == 2)
                || (form.editor.kind == "config-secret" && i == 1);
            let (shown, text_field) = match kind {
                FieldKind::Choice => (format!("‹ {value} ›"), false),
                FieldKind::Multi => {
                    let members = value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>();
                    let shown = if members.is_empty() {
                        "— press Enter to pick —".to_string()
                    } else {
                        members.join(" → ")
                    };
                    (shown, false)
                }
                FieldKind::Text => (
                    if secret {
                        "•".repeat(value.chars().count())
                    } else {
                        value.clone()
                    },
                    true,
                ),
            };
            let width = inner.width.saturating_sub(5) as usize;
            let offset = if i == form.focused && text_field {
                form.cursor.saturating_sub(width.saturating_sub(1))
            } else {
                0
            };
            let shown = shown.chars().skip(offset).take(width).collect::<String>();
            lines.push(Line::styled(format!("  {label}"), theme.dim()));
            lines.push(Line::styled(
                format!(" {} {shown}", if i == form.focused { "›" } else { " " }),
                if i == form.focused {
                    theme.menu_selected()
                } else {
                    theme.base()
                },
            ));
            if i == form.focused && text_field && inner.height > 3 {
                f.set_cursor_position((
                    inner.x + 3 + (form.cursor - offset).min(width.saturating_sub(1)) as u16,
                    inner.y + (i - first) as u16 * 2 + 1,
                ));
            }
        }
        f.render_widget(Paragraph::new(lines).style(theme.base()), inner);
        f.render_widget(
            Paragraph::new(format!("  {}", form.error)).style(theme.err()),
            Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1),
        );
        let hint = match form
            .editor
            .fields
            .get(form.focused)
            .map(|s| s.kind)
            .unwrap_or_default()
        {
            FieldKind::Choice => {
                "←/→ change · Tab / Shift+Tab · Enter next/save · Ctrl+S save · Esc cancel"
            }
            FieldKind::Multi => "Enter picks models · Tab / Shift+Tab · Ctrl+S save · Esc cancel",
            FieldKind::Text => "Tab / Shift+Tab · Enter next/save · Ctrl+S save · Esc cancel",
        };
        f.render_widget(
            Paragraph::new(format!("  {hint}")).style(theme.dim()),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
        if let Some(picker) = form.picker.as_mut() {
            draw_picker(
                f,
                area,
                &form.editor,
                form.focused,
                picker,
                theme,
                motion_tick,
            );
        }
    }
}
fn draw_picker(
    f: &mut Frame,
    area: Rect,
    editor: &Editor,
    focused: usize,
    picker: &mut Picker,
    theme: &crate::theme::Theme,
    motion_tick: u64,
) {
    picker.rect = None;
    let rect = centered(area, 72, 20);
    f.render_widget(Clear, rect);
    let selected = editor
        .values
        .get(focused)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let choices = editor
        .fields
        .get(focused)
        .map(|s| s.choices.as_slice())
        .unwrap_or(&[]);
    let query = picker.query.to_lowercase();
    let rows = choices
        .iter()
        .filter(|c| c.to_lowercase().contains(&query))
        .collect::<Vec<_>>();
    let block = Block::default()
        .title(format!(" Pick models · {} selected ", selected.len()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if motion_tick > 0 {
            theme.accent().fg(crate::motion::pulse(motion_tick))
        } else {
            theme.accent()
        })
        .style(theme.base());
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height < 4 || inner.width < 4 {
        return;
    }
    f.render_widget(
        Paragraph::new(format!("  Search: {}", picker.query)).style(theme.accent()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    picker.selected = picker.selected.min(rows.len().saturating_sub(1));
    let page = inner.height.saturating_sub(3).max(1) as usize;
    picker.page = page;
    let start = picker.selected.saturating_sub(page - 1);
    picker.start = start;
    picker.rect = Some(Rect::new(inner.x, inner.y + 2, inner.width, page as u16));
    let text = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(page)
        .map(|(i, c)| {
            let style = if i == picker.selected {
                theme.menu_selected()
            } else {
                theme.base()
            };
            let checked = selected.contains(&c.as_str());
            Line::from(vec![
                Span::styled(if i == picker.selected { " › " } else { "   " }, style),
                Span::styled(if checked { "✓ " } else { "  " }, style),
                Span::styled((*c).clone(), style),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();
    f.render_widget(
        Paragraph::new(text).style(theme.base()),
        picker.rect.unwrap(),
    );
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new("  No models match.").style(theme.dim()),
            picker.rect.unwrap(),
        );
    }
    f.render_widget(
        Paragraph::new(format!(
            "  ↑↓ · Space/Enter toggle · Esc done   {}/{}",
            if rows.is_empty() {
                0
            } else {
                picker.selected + 1
            },
            rows.len()
        ))
        .style(theme.dim()),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
}
fn clip(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if n + w > width.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
        n += w;
    }
    out
}

/// Preview is purely local; saving happens only through the selected command.
pub fn preview_theme(app: &mut App) {
    let Some(nav) = app.navigator.as_mut() else {
        return;
    };
    if !matches!(&nav.view,View::Menu(t,_) if t=="Appearance") {
        return;
    }
    if app.theme_restore.is_none() {
        app.theme_restore = Some((app.palette.clone(), app.transparent));
        nav.selected = nav
            .rows()
            .iter()
            .position(|r| r.label == app.palette)
            .unwrap_or(0);
    }
    if let Some(row) = nav.rows().get(nav.selected) {
        app.palette = row.label.clone();
        app.transparent = app.palette == "terminal";
    }
}
