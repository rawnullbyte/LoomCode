//! Schema-backed configuration browser. Edits stay in a draft until validated and saved.
//! Paths are RFC 6901 pointers encoded as hex in UI commands; secrets never enter labels/results.
use crate::{
    catalog::{Editor, FieldSpec, MenuItem},
    Agent, AgentEvent,
};
use loon_config::Config;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) struct Draft {
    snapshot: loon_config::editor::Snapshot,
    original: Value,
    pub value: Value,
}
pub fn encode(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
fn decode(s: &str) -> Result<String, String> {
    if !s.is_ascii() || !s.len().is_multiple_of(2) || s.len() > 8192 {
        return Err("Invalid settings path".into());
    }
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "Invalid settings path".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    String::from_utf8(bytes).map_err(|_| "Invalid settings path".into())
}
fn child(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}
fn key(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or("")
        .replace("~1", "/")
        .replace("~0", "~")
}
pub fn secret(path: &str) -> bool {
    path.ends_with("/api_key") || path.contains("/headers/") || path.contains("/env/")
}
fn tree(cfg: &Config) -> Result<Value, String> {
    let mut value = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    // Optional secrets are omitted on disk but must remain discoverable in the UI.
    if value["context7"].get("api_key").is_none() {
        value["context7"]["api_key"] = Value::Null;
    }
    if let Some(servers) = value["mcp"]["servers"].as_object_mut() {
        for server in servers.values_mut() {
            if server.get("url").is_none() {
                server["url"] = Value::Null;
            }
        }
    }
    Ok(value)
}
fn summary(path: &str, v: &Value) -> String {
    if secret(path) {
        return if v.is_null() || v.as_str().is_some_and(str::is_empty) {
            "not set"
        } else {
            "configured ••••••••"
        }
        .into();
    }
    match v {
        Value::Object(m) => format!("{} fields ›", m.len()),
        Value::Array(a) => format!("{} items ›", a.len()),
        Value::Null => "default / not set".into(),
        Value::String(s) => s.chars().take(120).collect(),
        _ => v.to_string(),
    }
}
fn choices(path: &str) -> Vec<String> {
    let parts: Vec<_> = path.split('/').filter(|s| !s.is_empty()).collect();
    let vals: &[&str] = match parts.as_slice() {
        ["appearance", "palette"] => &[
            "terminal", "dark", "light", "ocean", "violet", "ember", "graphite",
        ],
        ["appearance", "mode"] => &["transparent", "opaque"],
        ["appearance", "color"] => &["auto", "true", "c256", "c16", "mono"],
        ["permissions", "mode"] => &["bypass", "auto", "ask", "allowlist"],
        ["session", "effort"] => &["", "low", "medium", "high", "xhigh", "ultra"],
        ["providers", _, "api"] => &["chat", "responses"],
        ["combos", _, "mode"] => &["fallback", "round_robin"],
        ["models", _, "capabilities", _] => &["", "unknown", "supported", "unsupported"],
        _ => &[],
    };
    vals.iter().map(|v| v.to_string()).collect()
}
fn optional(path: &str) -> bool {
    matches!(
        key(path).as_str(),
        "path_prefix"
            | "command_prefix"
            | "host"
            | "api_key"
            | "api_key_env"
            | "context_window"
            | "max_output_tokens"
            | "max_retries"
            | "min_context_window"
            | "cost_ceiling"
            | "check_command"
            | "effort"
            | "autocompact_tokens"
            | "matcher"
            | "command"
            | "url"
            | "tool_calling"
            | "parallel_tool_calls"
            | "vision"
            | "strict_schema"
    ) && !matches!(path, "/session/max_retries" | "/context7/api_key_env")
        && !(path.starts_with("/providers/") && path.ends_with("/command"))
        && !(path.starts_with("/mcp/") && path.ends_with("/command"))
        && !(path.contains("/requires/")
            && (path.ends_with("/tool_calling") || path.ends_with("/vision")))
}
fn parse_value(path: &str, current: &Value, text: &str) -> Result<Value, String> {
    if optional(path) && text.is_empty() {
        return Ok(Value::Null);
    }
    if current.is_boolean() {
        return text
            .parse::<bool>()
            .map(Value::Bool)
            .map_err(|_| "Choose true or false".into());
    }
    if current.is_number()
        || (current.is_null()
            && matches!(
                key(path).as_str(),
                "context_window"
                    | "max_output_tokens"
                    | "max_retries"
                    | "min_context_window"
                    | "cost_ceiling"
                    | "autocompact_tokens"
            ))
    {
        let value: Value =
            serde_json::from_str(text).map_err(|_| "Enter a valid number".to_string())?;
        if !value.is_number() {
            return Err("Enter a valid number".into());
        }
        return Ok(value);
    }
    let allowed = choices(path);
    if !allowed.is_empty() && !allowed.iter().any(|v| v == text) {
        return Err("Choose one of the available values".into());
    }
    Ok(Value::String(text.into()))
}
fn extensible(path: &str) -> bool {
    matches!(
        path,
        "/providers" | "/models" | "/combos" | "/mcp/servers" | "/hooks"
    ) || path.ends_with("/headers")
        || path.ends_with("/env")
}
fn help(path: &str) -> &'static str {
    match path {
        "/context7"=>"API key is masked and stored in the selected config (0600 on Unix). The environment variable takes precedence. Test uses draft credentials.",
        "/context7/enabled"=>"Enable or disable native documentation tools",
        "/context7/api_key"=>"Masked API key; blank keeps an existing key. Use Clear saved key to remove it. Environment credentials take precedence.",
        "/context7/api_key_env"=>"Optional environment-based credential source; no restart needed when using the UI-managed key",
        "/context7/timeout_secs"=>"Request timeout in seconds, 1–120",
        "/appearance"=>"Theme, transparency, terminal color depth and ASCII glyph mode",
        "/permissions"=>"Mode, extra write roots and allow/ask/deny rules; explicit denies take precedence",
        "/permissions/allowed_hosts"=>"Reserved host list; use host-scoped permission rules for runtime enforcement",
        "/session"=>"Default model, context/compaction, retries, effort and verification",
        "/ui"=>"Motion, suggestions, footer, banner, placeholder and spinner text",
        "/providers"=>"Add an endpoint, then configure its credentials, headers, timeout, advertised models and reasoning levels",
        "/models"=>"Named models, token limits and capability overrides",
        "/combos"=>"Routing mode, ordered members, retries, capability requirements and rotation",
        "/hooks"=>"Lifecycle event lists; command or HTTP hooks execute after changes are saved",
        "/mcp"|"/mcp/servers"=>"HTTP or stdio MCP servers, arguments, environment and timeout; reconnect on save",
        _ if path.ends_with("/cost_ceiling")=>"Reserved cost metadata; billing enforcement is not implemented",
        _ if path.ends_with("/timeout_secs")=>"Timeout in seconds",
        _ if path.ends_with("/max_retries")=>"Retry budget; use an empty optional value to inherit the request default",
        _=>"Enter edits a field or opens a collection. Left/Right adjusts choices/numbers. Changes are staged until Save all changes.",
    }
}
fn item(label: impl Into<String>, detail: impl Into<String>, op: &str, path: &str) -> MenuItem {
    MenuItem {
        label: label.into(),
        detail: detail.into(),
        command: format!("/config ui {op} {}", encode(path)),
    }
}
impl Agent {
    fn ensure_config_draft(&mut self) -> Result<(), String> {
        if self.settings_draft.is_none() {
            let snapshot = loon_config::editor::Snapshot::load(
                &self.loon_paths(),
                self.ctx.ws.root(),
                loon_config::editor::Scope::User,
            )?;
            let value = tree(&snapshot.config)?;
            self.settings_draft = Some(Draft {
                snapshot,
                original: value.clone(),
                value,
            });
        }
        Ok(())
    }
    fn open_config_page(&mut self, path: &str) -> Result<(), String> {
        self.ensure_config_draft()?;
        let draft = self.settings_draft.as_ref().unwrap();
        let value = draft
            .value
            .pointer(path)
            .ok_or("Setting no longer exists")?;
        let mut rows = Vec::new();
        if !path.is_empty() {
            rows.push(item(
                "← Parent category",
                "Back to the containing settings",
                "open",
                parent(path),
            ));
        }
        match value {
            Value::Object(m) => {
                for (k, v) in m {
                    if k != "schema_version" {
                        let p = child(path, k);
                        rows.push(item(
                            format!("{k} = {}", summary(&p, v)),
                            help(&p),
                            if v.is_object() || v.is_array() {
                                "open"
                            } else {
                                "edit"
                            },
                            &p,
                        ));
                    }
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    let p = child(path, &i.to_string());
                    rows.push(item(
                        format!("{}. {}", i + 1, summary(&p, v)),
                        help(&p),
                        if v.is_object() || v.is_array() {
                            "open"
                        } else {
                            "edit"
                        },
                        &p,
                    ));
                }
            }
            _ => return Err("This setting is a field, not a category".into()),
        }
        if value.is_array() || extensible(path) {
            rows.push(item(
                "+ Add entry",
                "Add a list item or named entry to the draft",
                "add",
                path,
            ));
        }
        if path == "/context7" {
            rows.push(item(
                "Clear saved key",
                "Removes only the saved key; environment credentials remain available",
                "clear",
                "/context7/api_key",
            ));
            rows.push(item(
                "Test connection",
                "Search for a small documentation result with the draft credentials",
                "test",
                path,
            ));
        }
        let pp = parent(path);
        if draft.value.pointer(pp).is_some_and(|v| v.is_array()) || extensible(pp) {
            rows.push(item(
                "Remove this entry",
                "Remove from the draft; save to apply",
                "remove",
                path,
            ));
        }
        rows.push(item(
            if draft.original != draft.value {
                "Save all changes • pending"
            } else {
                "Save all changes"
            },
            "Validate the complete configuration, persist and apply it; keep this menu open",
            "save",
            path,
        ));
        rows.push(item(
            "Discard staged changes",
            "Reload saved values without changing the configuration",
            "discard",
            path,
        ));
        if path.is_empty() {
            for scope in ["user", "project", "local"] {
                rows.push(item(format!("Scope: {scope}"),"Edit user defaults, shared project overrides, or local project overrides. Save/discard current changes before switching.","scope",scope));
            }
        }
        if path.is_empty() {
            rows.push(MenuItem {
                label: "Quick settings".into(),
                detail: "Return to common preferences".into(),
                command: "/settings".into(),
            });
        }
        self.emit(AgentEvent::OpenMenu {
            title: format!(
                "Settings / {}",
                if path.is_empty() {
                    format!("All configuration ({})", draft.snapshot.scope.name())
                } else {
                    format!(
                        "{} ({})",
                        path.trim_start_matches('/').replace('/', " / "),
                        draft.snapshot.scope.name()
                    )
                }
            ),
            items: rows,
        });
        Ok(())
    }
    pub(crate) fn refresh_context7(&mut self, cfg: &loon_config::schema::Context7) {
        let options = loon_tools::context7::Options {
            enabled: cfg.enabled,
            api_key: cfg.api_key.clone(),
            api_key_env: cfg.api_key_env.clone(),
            timeout_secs: cfg.timeout_secs,
            ..Default::default()
        };
        self.tools = self
            .tools
            .clone()
            .without(&[
                "context7_search",
                "context7_resolve_library",
                "context7_query_docs",
            ])
            .with_extra(
                [
                    loon_tools::context7::Operation::Search,
                    loon_tools::context7::Operation::Resolve,
                    loon_tools::context7::Operation::Query,
                ]
                .into_iter()
                .map(|operation| {
                    Arc::new(loon_tools::context7::Context7Tool {
                        operation,
                        options: options.clone(),
                    }) as Arc<dyn loon_tools::Tool>
                })
                .collect(),
            );
        self.refresh_tool_search();
    }
    pub(crate) fn context7_status(&self) -> String {
        let cfg = Config::load_effective(&self.loon_paths(), self.ctx.ws.root())
            .map(|v| v.0.context7)
            .unwrap_or_default();
        let env = std::env::var(&cfg.api_key_env).is_ok_and(|v| !v.trim().is_empty());
        format!("Context7: {} · {} · {}s timeout\nEnvironment: {} · /settings → Context7 to configure or test",if cfg.enabled{"enabled"}else{"disabled"},if env{"environment key available"}else if cfg.api_key.is_some(){"saved key configured"}else{"key missing"},cfg.timeout_secs,cfg.api_key_env)
    }
    pub(crate) async fn config_ui_command(&mut self, args: Vec<String>) -> Result<String, String> {
        let op = args.first().map(String::as_str).unwrap_or("open");
        let path = decode(args.get(1).map(String::as_str).unwrap_or(""))?;
        self.ensure_config_draft()?;
        match op {
            "scope" => {
                let draft = self.settings_draft.as_ref().unwrap();
                if draft.value != draft.original {
                    return Err("Save or discard staged changes before switching scopes".into());
                }
                let scope = loon_config::editor::Scope::parse(&path)
                    .ok_or("Unknown configuration scope")?;
                let snapshot = loon_config::editor::Snapshot::load(
                    &self.loon_paths(),
                    self.ctx.ws.root(),
                    scope,
                )?;
                let value = tree(&snapshot.config)?;
                self.settings_draft = Some(Draft {
                    snapshot,
                    original: value.clone(),
                    value,
                });
                self.open_config_page("")?;
            }
            "open" => self.open_config_page(&path)?,
            "edit" => {
                let value = self
                    .settings_draft
                    .as_ref()
                    .unwrap()
                    .value
                    .pointer(&path)
                    .ok_or("Missing setting")?;
                if value.is_object() || value.is_array() {
                    return self.config_ui_open_category(&path);
                }
                let options = if value.is_boolean() {
                    vec!["true".into(), "false".into()]
                } else {
                    choices(&path)
                };
                let text = if secret(&path) || value.is_null() {
                    String::new()
                } else {
                    value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string())
                };
                self.emit(AgentEvent::OpenEditor(Editor {
                    kind: if secret(&path) {
                        "config-secret"
                    } else {
                        "config-value"
                    }
                    .into(),
                    values: if secret(&path) {
                        vec![path.clone(), text, "replace".into()]
                    } else {
                        vec![path.clone(), text]
                    },
                    editing: true,
                    fields: {
                        let mut fields = vec![
                            FieldSpec::text(),
                            if options.is_empty() {
                                FieldSpec::text()
                            } else {
                                FieldSpec::choice(options)
                            },
                        ];
                        if secret(&path) {
                            fields.push(FieldSpec::choice(["replace", "keep", "clear"]));
                        }
                        fields
                    },
                }));
            }
            "set" => {
                let text = args.get(2).ok_or("Missing value")?;
                let draft = self.settings_draft.as_mut().unwrap();
                let current = draft.value.pointer(&path).ok_or("Missing setting")?;
                let action = args.get(3).map(String::as_str).unwrap_or("replace");
                if secret(&path) && action == "clear" {
                    *draft.value.pointer_mut(&path).ok_or("Missing setting")? = if optional(&path) {
                        Value::Null
                    } else {
                        json!("")
                    };
                } else if !(secret(&path) && (text.is_empty() || action == "keep")) {
                    let value = parse_value(&path, current, text)?;
                    *draft.value.pointer_mut(&path).ok_or("Missing setting")? = value;
                }
                self.open_config_page(parent(&path))?;
            }
            "step" => {
                let delta = args
                    .get(2)
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|n| [-1, 1].contains(n))
                    .ok_or("Invalid adjustment")?;
                let draft = self.settings_draft.as_mut().unwrap();
                let value = draft.value.pointer_mut(&path).ok_or("Missing setting")?;
                let mut options = choices(&path);
                if value.is_boolean() {
                    options = vec!["true".into(), "false".into()];
                }
                let next = if !options.is_empty() {
                    let current = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    let i = options.iter().position(|s| s == &current).unwrap_or(0) as i64;
                    options[(i + delta).rem_euclid(options.len() as i64) as usize].clone()
                } else if let Some(n) = value.as_u64() {
                    let step = if path.ends_with("context_window")
                        || path.ends_with("autocompact_tokens")
                        || path.ends_with("tool_output_bytes")
                    {
                        1024
                    } else {
                        1
                    };
                    if delta < 0 {
                        n.saturating_sub(step)
                    } else {
                        n.saturating_add(step)
                    }
                    .to_string()
                } else {
                    return Ok(String::new());
                };
                *value = parse_value(&path, value, &next)?;
                self.open_config_page(parent(&path))?;
            }
            "clear" => {
                if path != "/context7/api_key" {
                    return Err("Unsupported clear action".into());
                }
                self.settings_draft.as_mut().unwrap().value["context7"]["api_key"] = Value::Null;
                self.open_config_page("/context7")?;
            }
            "add" => {
                let value = self
                    .settings_draft
                    .as_ref()
                    .unwrap()
                    .value
                    .pointer(&path)
                    .ok_or("Missing collection")?;
                if value.is_array() {
                    self.insert_config_entry(&path, None)?;
                } else if extensible(&path) {
                    self.emit(AgentEvent::OpenEditor(Editor {
                        kind: "config-entry".into(),
                        values: vec![path.clone(), String::new()],
                        editing: true,
                        fields: vec![
                            FieldSpec::text(),
                            if path == "/hooks" {
                                FieldSpec::choice([
                                    "SessionStart",
                                    "UserPromptSubmit",
                                    "PreToolUse",
                                    "PostToolUse",
                                    "PostToolUseFailure",
                                    "Stop",
                                    "StopFailure",
                                    "PreCompact",
                                    "PostCompact",
                                    "SessionEnd",
                                ])
                            } else {
                                FieldSpec::text()
                            },
                        ],
                    }));
                } else {
                    return Err("This category has fixed fields".into());
                }
            }
            "insert" => {
                self.insert_config_entry(&path, Some(args.get(2).ok_or("Missing entry name")?))?;
            }
            "remove" => {
                let name = key(&path);
                let container = self
                    .settings_draft
                    .as_mut()
                    .unwrap()
                    .value
                    .pointer_mut(parent(&path))
                    .ok_or("Missing parent")?;
                if let Some(a) = container.as_array_mut() {
                    let i = name.parse::<usize>().map_err(|_| "Invalid index")?;
                    if i >= a.len() {
                        return Err("Missing entry".into());
                    }
                    a.remove(i);
                } else if extensible(parent(&path)) {
                    container
                        .as_object_mut()
                        .ok_or("Missing map")?
                        .remove(&name);
                } else {
                    return Err("Required config fields cannot be removed".into());
                }
                self.open_config_page(parent(&path))?;
            }
            "discard" => {
                let scope = self.settings_draft.as_ref().unwrap().snapshot.scope;
                let snapshot = loon_config::editor::Snapshot::load(
                    &self.loon_paths(),
                    self.ctx.ws.root(),
                    scope,
                )?;
                let value = tree(&snapshot.config)?;
                self.settings_draft = Some(Draft {
                    snapshot,
                    original: value.clone(),
                    value,
                });
                if self
                    .settings_draft
                    .as_ref()
                    .unwrap()
                    .value
                    .pointer(&path)
                    .is_some()
                {
                    self.open_config_page(&path)?;
                } else {
                    self.open_config_page("")?;
                }
            }
            "save" => {
                self.save_config_draft().await?;
                self.open_config_page(&path)?;
                return Ok("Configuration saved and applied. This menu stays open.".into());
            }
            "test" => {
                let cfg: Config =
                    serde_json::from_value(self.settings_draft.as_ref().unwrap().value.clone())
                        .map_err(|_| "Fix invalid configuration fields before testing")?;
                let options = loon_tools::context7::Options {
                    enabled: cfg.context7.enabled,
                    api_key: cfg.context7.api_key,
                    api_key_env: cfg.context7.api_key_env,
                    timeout_secs: cfg.context7.timeout_secs,
                    ..Default::default()
                };
                let tool = loon_tools::context7::Context7Tool {
                    operation: loon_tools::context7::Operation::Search,
                    options,
                };
                let result =
                    loon_tools::Tool::run(&tool, json!({"query":"Rust documentation"}), &self.ctx)
                        .await;
                self.open_config_page("/context7")?;
                return result.map(|_|"Context7 connection succeeded with the draft credentials; save to apply changes.".into()).map_err(|e|e.to_string());
            }
            _ => return Err("Unknown settings action".into()),
        }
        Ok(String::new())
    }
    // Non-async helper avoids recursive futures when a stale UI asks to edit a category.
    fn config_ui_open_category(&mut self, path: &str) -> Result<String, String> {
        self.open_config_page(path)?;
        Ok(String::new())
    }
    fn insert_config_entry(&mut self, path: &str, name: Option<&str>) -> Result<(), String> {
        let first_provider = self.settings_draft.as_ref().unwrap().value["providers"]
            .as_object()
            .and_then(|m| m.keys().next())
            .cloned()
            .unwrap_or("mock".into());
        let value = match path {
            "/providers" => serde_json::to_value(loon_config::Provider {
                base_url: "http://127.0.0.1:11434/v1".into(),
                ..Default::default()
            })
            .unwrap(),
            "/models" => {
                json!({"provider":first_provider,"id":"model-id","context_window":null,"max_output_tokens":null,"capabilities":{"tool_calling":null,"parallel_tool_calls":null,"vision":null,"strict_schema":null}})
            }
            "/combos" => {
                json!({"mode":"fallback","members":[],"requires":{"tool_calling":false,"vision":false,"min_context_window":null},"cost_ceiling":null,"persistent_rotation":false})
            }
            "/mcp/servers" => {
                json!({"command":"","url":"http://127.0.0.1:3000/mcp","args":[],"env":{},"timeout_secs":60})
            }
            "/hooks" => json!([]),
            _ if path.starts_with("/hooks/") => {
                json!({"matcher":null,"command":"true","url":null,"timeout_secs":10})
            }
            _ if path.ends_with("/members") => {
                json!({"provider":first_provider,"model":"model-id","max_retries":null})
            }
            "/permissions/allow_rules" | "/permissions/ask_rules" | "/permissions/deny_rules" => {
                json!({"tool":"read","path_prefix":null,"command_prefix":null,"host":null})
            }
            _ => json!(""),
        };
        let container = self
            .settings_draft
            .as_mut()
            .unwrap()
            .value
            .pointer_mut(path)
            .ok_or("Missing collection")?;
        let new_path = if let Some(array) = container.as_array_mut() {
            let i = array.len();
            array.push(value);
            child(path, &i.to_string())
        } else {
            let name = name.ok_or("Name required")?;
            if name.trim().is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
                return Err("Use a nonempty name of at most 200 characters".into());
            }
            if matches!(path, "/providers" | "/models" | "/combos" | "/mcp/servers") {
                crate::catalog::valid_name(name)?;
            }
            let map = container.as_object_mut().ok_or("Not a collection")?;
            if map.contains_key(name) {
                return Err("An entry with that name already exists".into());
            }
            map.insert(name.into(), value);
            child(path, name)
        };
        if self
            .settings_draft
            .as_ref()
            .unwrap()
            .value
            .pointer(&new_path)
            .is_some_and(|v| v.is_object() || v.is_array())
        {
            self.open_config_page(&new_path)
        } else {
            self.open_config_page(path)
        }
    }
    async fn save_config_draft(&mut self) -> Result<(), String> {
        let draft = self.settings_draft.as_ref().ok_or("No settings draft")?;
        // Never echo deserialization errors: they can contain a pasted credential.
        let cfg: Config = serde_json::from_value(draft.value.clone()).map_err(|_| {
            "Invalid field type; check numbers, choices and required values".to_string()
        })?;
        let prepared = draft.snapshot.prepare(&cfg)?;
        let scope = draft.snapshot.scope;
        let current = Config::load_effective(&self.loon_paths(), self.ctx.ws.root())
            .map_err(|_| "Invalid current configuration")?
            .0;
        let effective = prepared.effective.clone();
        let mcp_changed = current.mcp != effective.mcp;
        // Prepare a new sandbox before saving so invalid write roots cannot partially apply.
        let roots = effective
            .permissions
            .extra_write_roots
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        let workspace = self
            .ctx
            .ws
            .with_write_roots(roots)
            .map_err(|_| "Invalid extra write roots")?;
        self.settings_draft
            .as_ref()
            .unwrap()
            .snapshot
            .commit(&prepared)?;
        Arc::make_mut(&mut self.registry).reconfigure(&effective);
        self.cfg.max_retries = effective.session.max_retries;
        self.cfg.allow_retry_after_partial_stream =
            effective.session.allow_retry_after_partial_stream;
        self.cfg.check_command = effective.session.check_command.clone();
        self.cfg.ultra_rounds = effective.session.ultra_rounds;
        self.cfg.tool_output_bytes = effective.session.tool_output_bytes;
        self.ctx.output_limit = effective.session.tool_output_bytes;
        self.ctx.ws = Arc::new(workspace);
        self.cfg.permissions = effective.permissions.clone();
        self.set_permission_mode(effective.permissions.mode);
        self.effort = effective.session.effort.clone();
        self.autocompact_tokens = effective.session.autocompact_tokens;
        self.tips = effective.ui.tips;
        self.cfg.model_ref = effective.session.model.clone();
        self.cfg.context_window = match self.registry.resolve(&effective.session.model)? {
            loon_providers::ResolvedModel::Single {
                provider_id,
                model_id,
            } => self
                .registry
                .context_window(&provider_id, &model_id)
                .unwrap_or(128_000),
            loon_providers::ResolvedModel::Combo { name } => self
                .registry
                .combo(&name)
                .and_then(|c| {
                    c.members
                        .iter()
                        .filter_map(|m| self.registry.context_window(&m.provider, &m.model))
                        .min()
                })
                .unwrap_or(128_000),
        };
        self.emit(AgentEvent::ModelChanged {
            model: self.cfg.model_ref.clone(),
        });
        self.cfg.context_window = effective
            .session
            .context_window
            .unwrap_or(self.cfg.context_window);
        self.refresh_context7(&effective.context7);
        self.refresh_delegate();
        self.emit(AgentEvent::ModeChanged {
            mode: format!("{:?}", effective.permissions.mode).to_lowercase(),
        });
        self.emit(AgentEvent::EffortChanged {
            level: self.effort.clone().unwrap_or("medium".into()),
        });
        self.emit(AgentEvent::SetPalette {
            name: effective.appearance.palette.clone(),
        });
        self.transparent_request =
            effective.appearance.mode == loon_config::AppearanceMode::Transparent;
        self.emit(AgentEvent::SetTransparent {
            transparent: self.transparent_request,
        });
        for (key, value) in [
            ("animations", effective.ui.animations.to_string()),
            ("popup_rows", effective.ui.popup_rows.to_string()),
            ("context_window", self.cfg.context_window.to_string()),
            ("tips", effective.ui.tips.to_string()),
            ("tip_text", effective.ui.tip_text.clone()),
            ("footer", effective.ui.footer.to_string()),
            ("welcome_banner", effective.ui.welcome_banner.to_string()),
            (
                "spinner_verbs",
                serde_json::to_string(&effective.ui.spinner_verbs).unwrap(),
            ),
            ("ascii_only", effective.appearance.ascii_only.to_string()),
            (
                "color",
                serde_json::to_value(effective.appearance.color)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
        ] {
            self.emit(AgentEvent::UiPreference {
                key: key.into(),
                value,
            });
        }
        self.worklog
            .update(|s| {
                s.effort = self.effort.clone();
                s.model = Some(self.cfg.model_ref.clone());
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        let snapshot =
            loon_config::editor::Snapshot::load(&self.loon_paths(), self.ctx.ws.root(), scope)?;
        let value = tree(&snapshot.config)?;
        self.settings_draft = Some(Draft {
            snapshot,
            original: value.clone(),
            value,
        });
        if mcp_changed {
            self.reload_mcp().await?;
        }
        Ok(())
    }
}
