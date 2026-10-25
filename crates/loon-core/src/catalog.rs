//! Live provider/model/combo management. UI snapshots never contain credentials.
use crate::{Agent, AgentEvent};
use loon_config::{Combo, ComboMember, ComboMode, Config, ModelDef};
use loon_providers::ProviderRegistry;
use std::sync::Arc;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Catalog {
    pub models: Vec<String>,
    pub recent: Vec<String>,
    pub providers: Vec<ProviderSummary>,
    pub combos: Vec<ComboSummary>,
    pub mcps: Vec<McpSummary>,
    /// Names of every tool currently registered (so a server view can list its own).
    pub tools: Vec<String>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderSummary {
    pub name: String,
    pub url: String,
    pub api: String,
    pub count: usize,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComboSummary {
    pub name: String,
    pub mode: String,
    pub members: Vec<String>,
}
/// One configured MCP server, as the UI sees it. Never contains secrets beyond the
/// command line the user themselves configured.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpSummary {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
    /// How many tools this server currently exposes to the agent.
    pub tools: usize,
    /// Empty when the server connected; otherwise the reason it did not.
    pub error: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MenuItem {
    pub label: String,
    pub detail: String,
    pub command: String,
}
/// How the UI edits one [`Editor`] field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub enum FieldKind {
    /// Free text with a caret.
    #[default]
    Text,
    /// One value from `choices`, cycled with ←/→.
    Choice,
    /// Several values from `choices`, toggled in a picker; stored comma-separated.
    Multi,
}

/// Per-field editing metadata, parallel to [`Editor::values`]. Missing entries are
/// free text, so an editor built without specs behaves exactly like before.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FieldSpec {
    pub kind: FieldKind,
    pub choices: Vec<String>,
}

impl FieldSpec {
    pub fn text() -> Self {
        Self::default()
    }
    pub fn choice<I, S>(choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind: FieldKind::Choice,
            choices: choices.into_iter().map(Into::into).collect(),
        }
    }
    pub fn multi<I, S>(choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind: FieldKind::Multi,
            choices: choices.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Editor {
    pub kind: String,
    pub values: Vec<String>,
    pub editing: bool,
    /// How each value is edited; parallel to `values`.
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
}

pub(crate) fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("name must contain letters, digits, - or _".into());
    }
    Ok(())
}
impl Agent {
    pub fn catalog(&self) -> Catalog {
        let recent = std::fs::read(self.data_dir.join("recent-models.json"))
            .ok()
            .and_then(|v| serde_json::from_slice::<Vec<String>>(&v).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| self.registry.resolve(r).is_ok())
            .take(5)
            .collect();
        let providers = self
            .registry
            .provider_names()
            .into_iter()
            .filter_map(|name| {
                let p = self.registry.provider(&name)?;
                let count = self
                    .registry
                    .model_refs()
                    .iter()
                    .filter(|m| m.starts_with(&format!("{name}/")))
                    .count();
                Some(ProviderSummary {
                    name,
                    url: p.base_url.clone(),
                    api: p.api.as_str().into(),
                    count,
                })
            })
            .collect();
        let combos = self
            .registry
            .combo_names()
            .into_iter()
            .filter_map(|name| {
                let c = self.registry.combo(&name)?;
                Some(ComboSummary {
                    name,
                    mode: match c.mode {
                        ComboMode::Fallback => "fallback",
                        ComboMode::RoundRobin => "round_robin",
                    }
                    .into(),
                    members: c
                        .members
                        .iter()
                        .map(|m| format!("{}/{}", m.provider, m.model))
                        .collect(),
                })
            })
            .collect();
        let (cfg, _) =
            Config::load_effective(&self.loon_paths(), self.ctx.ws.root()).unwrap_or_default();
        let mcps = cfg
            .mcp
            .servers
            .iter()
            .map(|(name, server)| {
                let prefix = format!("mcp_{name}_");
                let tools = self
                    .tools
                    .specs()
                    .iter()
                    .filter(|t| t.name.starts_with(&prefix))
                    .count();
                McpSummary {
                    name: name.clone(),
                    command: server.url.clone().unwrap_or_else(|| server.command.clone()),
                    args: server.args.clone(),
                    timeout_secs: server.timeout_secs,
                    tools,
                    error: self.mcp_errors.get(name).cloned().unwrap_or_default(),
                }
            })
            .collect();
        Catalog {
            models: self.registry.model_refs(),
            recent,
            providers,
            combos,
            mcps,
            tools: self.tools.specs().iter().map(|s| s.name.clone()).collect(),
        }
    }
    pub(crate) fn open_catalog(&self, view: &str) {
        self.emit(AgentEvent::OpenCatalog {
            view: view.into(),
            catalog: self.catalog(),
        });
    }
    pub(crate) fn save_live_config(&mut self, cfg: Config) -> Result<(), String> {
        let errors: Vec<_> = loon_config::Validator::new(&cfg)
            .validate()
            .into_iter()
            .filter(|f| f.is_error)
            .collect();
        if !errors.is_empty() {
            return Err(errors
                .iter()
                .map(|f| format!("{}: {}", f.path, f.message))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        let effective = cfg
            .with_overlays(self.ctx.ws.root())
            .map_err(|e| e.to_string())?;
        if !loon_config::Validator::new(&effective).errors().is_empty() {
            return Err("Configuration conflicts with project/local overrides".into());
        }
        cfg.save(&self.loon_paths()).map_err(|e| e.to_string())?;
        Arc::make_mut(&mut self.registry).reconfigure(&effective);
        self.refresh_delegate();
        Ok(())
    }
    pub(crate) fn choose_model(&mut self, reference: &str) -> Result<String, String> {
        let resolved = self.registry.resolve(reference)?;
        if self.cfg.context_window > 0 {
            self.cfg.context_window = match resolved {
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
        }
        self.worklog
            .update(|s| {
                s.model = Some(reference.into());
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        self.cfg.model_ref = reference.into();
        self.emit(AgentEvent::UiPreference {
            key: "context_window".into(),
            value: self.cfg.context_window.to_string(),
        });
        self.refresh_delegate();
        let mut recent = self.catalog().recent;
        recent.retain(|r| r != reference);
        recent.insert(0, reference.into());
        recent.truncate(5);
        let data = serde_json::to_vec(&recent).map_err(|e| e.to_string())?;
        std::fs::write(self.data_dir.join("recent-models.json"), data)
            .map_err(|e| e.to_string())?;
        let (mut cfg, _) =
            Config::load_or_builtin(&self.loon_paths()).map_err(|e| e.to_string())?;
        cfg.session.model = reference.into();
        cfg.save(&self.loon_paths()).map_err(|e| e.to_string())?;
        self.emit(AgentEvent::ModelChanged {
            model: reference.into(),
        });
        Ok(format!("model: {reference}"))
    }
    pub(crate) async fn provider_command(&mut self, args: Vec<String>) -> Result<String, String> {
        let op = args.first().map(String::as_str).unwrap_or("browse");
        let name = args.get(1).map(String::as_str).unwrap_or("");
        if op == "browse" || (op == "list" && self.events.is_some()) {
            self.open_catalog("providers");
            return Ok(String::new());
        }
        let (mut cfg, _) =
            Config::load_or_builtin(&self.loon_paths()).map_err(|e| e.to_string())?;
        match op {
            "list" => Ok(self
                .catalog()
                .providers
                .iter()
                .map(|p| format!("{}  {}  {}  {} models", p.name, p.api, p.url, p.count))
                .collect::<Vec<_>>()
                .join("\n")),
            "add" | "edit" if args.len() <= 2 && self.events.is_some() => {
                let values = if op == "edit" {
                    let p = cfg.providers.get(name).ok_or("unknown provider")?;
                    vec![
                        name.into(),
                        p.base_url.clone(),
                        String::new(),
                        p.api.as_str().into(),
                        p.api_key_env.clone().unwrap_or_default(),
                        p.reasoning_efforts.join(","),
                    ]
                } else {
                    vec![
                        String::new(),
                        String::new(),
                        String::new(),
                        "chat".into(),
                        String::new(),
                        String::new(),
                    ]
                };
                self.emit(AgentEvent::OpenEditor(Editor {
                    kind: "provider".into(),
                    values,
                    editing: op == "edit",
                    fields: vec![
                        FieldSpec::text(),
                        FieldSpec::text(),
                        FieldSpec::text(),
                        FieldSpec::choice(["chat", "responses"]),
                        FieldSpec::text(),
                        FieldSpec::text(),
                    ],
                }));
                Ok(String::new())
            }
            "add" | "edit" | "save" => {
                valid_name(name)?;
                if op == "add" && cfg.providers.contains_key(name) {
                    return Err("provider already exists; use edit".into());
                }
                if op == "edit" && !cfg.providers.contains_key(name) {
                    return Err("unknown provider".into());
                }
                let url = args.get(2).ok_or("usage: /providers add NAME URL [KEY|-] [chat|responses] [KEY_ENV|-] [reasoning levels]")?;
                let url = loon_providers::http::normalize_base_url(url);
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err("endpoint must be an http(s) API base URL".into());
                }
                let mut def = cfg.providers.get(name).cloned().unwrap_or_default();
                def.base_url = url;
                def.mock = false;
                if let Some(key) = args.get(3).filter(|s| !s.is_empty() && s.as_str() != "-") {
                    def.api_key = Some(key.clone());
                }
                if let Some(api) = args.get(4) {
                    def.api = match api.as_str() {
                        "chat" => loon_config::ApiFamily::Chat,
                        "responses" => loon_config::ApiFamily::Responses,
                        _ => return Err("API must be chat or responses".into()),
                    };
                }
                if let Some(env) = args.get(5).filter(|s| !s.is_empty() && s.as_str() != "-") {
                    def.api_key_env = Some(env.clone());
                    def.api_key = None;
                }
                if let Some(levels) = args.get(6) {
                    def.reasoning_efforts = levels
                        .split(',')
                        .filter(|s| !s.is_empty() && *s != "-")
                        .map(str::to_string)
                        .collect();
                }
                cfg.providers.insert(name.into(), def);
                self.save_live_config(cfg)?;
                self.emit(AgentEvent::MemberNote(format!(
                    "provider {name} saved; discovering models…"
                )));
                let result = self.refresh_provider(name).await;
                if self.events.is_some() {
                    self.open_catalog("providers");
                }
                Ok(match result { Ok(note) => format!("provider {name} saved and active; {note}"), Err(e) => format!("provider {name} saved and active; discovery failed: {e}. Use /providers refresh {name} or /model {name}/MODEL_ID.") })
            }
            "refresh" | "test" => {
                if name.is_empty() || name == "all" {
                    let names = self.registry.provider_names();
                    // Independent endpoints are polled concurrently; config writes remain serialized.
                    let registry = self.registry.clone();
                    let mut jobs = tokio::task::JoinSet::new();
                    for n in names {
                        if !registry.is_mock(&n) {
                            let reg = registry.clone();
                            jobs.spawn(async move {
                                let result = discover(&reg, &n).await;
                                (n, result)
                            });
                        }
                    }
                    let mut notes = Vec::new();
                    while let Some(done) = jobs.join_next().await {
                        let (n, result) = done.map_err(|e| e.to_string())?;
                        match result {
                            Ok(models) => notes.push(self.install_models(&n, models)?),
                            Err(e) => notes.push(format!("{n}: {e}")),
                        }
                    }
                    return Ok(notes.join("\n"));
                }
                let note = self.refresh_provider(name).await?;
                Ok(note)
            }
            "remove" | "delete" => {
                if !cfg.providers.contains_key(name) {
                    return Err("unknown provider".into());
                }
                let used: Vec<_> = cfg
                    .combos
                    .iter()
                    .filter(|(_, c)| c.members.iter().any(|m| m.provider == name))
                    .map(|(n, _)| n.clone())
                    .collect();
                if !used.is_empty() {
                    return Err(format!(
                        "provider is used by combos: {}. Edit/remove them first.",
                        used.join(", ")
                    ));
                }
                if let Ok(loon_providers::ResolvedModel::Single { provider_id, .. }) =
                    self.registry.resolve(&self.cfg.model_ref)
                {
                    if provider_id == name {
                        return Err("switch to another model before removing its provider".into());
                    }
                }
                cfg.providers.remove(name);
                cfg.models.retain(|_, m| m.provider != name);
                self.save_live_config(cfg)?;
                self.open_catalog("providers");
                Ok(format!("removed provider {name}"))
            }
            "key-clear" => {
                let p = cfg.providers.get_mut(name).ok_or("unknown provider")?;
                p.api_key = None;
                p.api_key_env = None;
                self.save_live_config(cfg)?;
                Ok(format!("credentials cleared for {name}"))
            }
            _ => Err("/providers [list|add|edit|refresh|remove|key-clear] [NAME]".into()),
        }
    }
    pub(crate) async fn refresh_provider(&mut self, name: &str) -> Result<String, String> {
        let models = discover(&self.registry, name).await?;
        self.install_models(name, models)
    }
    fn install_models(&mut self, name: &str, models: Vec<String>) -> Result<String, String> {
        let (mut cfg, _) =
            Config::load_or_builtin(&self.loon_paths()).map_err(|e| e.to_string())?;
        let p = cfg.providers.get_mut(name).ok_or("unknown provider")?;
        p.models = models.clone();
        cfg.models.retain(|alias, m| {
            m.provider != name || alias != &format!("{name}/{}", m.id) || models.contains(&m.id)
        });
        for id in &models {
            cfg.models
                .entry(format!("{name}/{id}"))
                .or_insert_with(|| ModelDef {
                    provider: name.into(),
                    id: id.clone(),
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: Default::default(),
                });
        }
        self.save_live_config(cfg)?;
        self.emit(AgentEvent::CatalogUpdated {
            catalog: self.catalog(),
        });
        Ok(format!("{name}: {} models fetched", models.len()))
    }
    pub(crate) fn combo_command(&mut self, args: Vec<String>) -> Result<String, String> {
        let op = args.first().map(String::as_str).unwrap_or("browse");
        let name = args.get(1).map(String::as_str).unwrap_or("");
        if op == "browse" || (op == "list" && self.events.is_some()) {
            self.open_catalog("combos");
            return Ok(String::new());
        }
        let (mut cfg, _) =
            Config::load_or_builtin(&self.loon_paths()).map_err(|e| e.to_string())?;
        match op {
            "list" => Ok(self
                .catalog()
                .combos
                .iter()
                .map(|c| format!("{} ({}) {}", c.name, c.mode, c.members.join(" → ")))
                .collect::<Vec<_>>()
                .join("\n")),
            "add" | "edit" if args.len() <= 2 && self.events.is_some() => {
                let values = if op == "edit" {
                    let c = cfg.combos.get(name).ok_or("unknown combo")?;
                    vec![
                        name.into(),
                        match c.mode {
                            ComboMode::Fallback => "fallback",
                            ComboMode::RoundRobin => "round_robin",
                        }
                        .into(),
                        c.members
                            .iter()
                            .map(|m| format!("{}/{}", m.provider, m.model))
                            .collect::<Vec<_>>()
                            .join(","),
                    ]
                } else {
                    vec![String::new(), "fallback".into(), String::new()]
                };
                // Existing members stay pickable even if their model definition was
                // removed from the live catalog.
                let mut choices = self.registry.model_refs();
                for member in values[2]
                    .split(',')
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                {
                    if !choices.iter().any(|c| c == member) {
                        choices.push(member.to_string());
                    }
                }
                self.emit(AgentEvent::OpenEditor(Editor {
                    kind: "combo".into(),
                    values,
                    editing: op == "edit",
                    fields: vec![
                        FieldSpec::text(),
                        FieldSpec::choice(["fallback", "round_robin"]),
                        FieldSpec::multi(choices),
                    ],
                }));
                Ok(String::new())
            }
            "add" | "edit" | "save" => {
                valid_name(name)?;
                if op == "add" && cfg.combos.contains_key(name) {
                    return Err("combo already exists; use edit".into());
                }
                if op == "edit" && !cfg.combos.contains_key(name) {
                    return Err("unknown combo".into());
                }
                let mut pos = 2;
                let mode = match args.get(2).map(String::as_str) {
                    Some("round_robin") => {
                        pos = 3;
                        ComboMode::RoundRobin
                    }
                    Some("fallback") => {
                        pos = 3;
                        ComboMode::Fallback
                    }
                    _ => ComboMode::Fallback,
                };
                let mut members = Vec::new();
                for token in args[pos..]
                    .iter()
                    .flat_map(|s| s.split(','))
                    .filter(|s| !s.trim().is_empty())
                {
                    let token = token.trim();
                    let (provider, model) = if let Some((p, m)) = token.split_once(':') {
                        (p.to_string(), m.to_string())
                    } else {
                        match self.registry.resolve(token)? {
                            loon_providers::ResolvedModel::Single {
                                provider_id,
                                model_id,
                            } => (provider_id, model_id),
                            _ => return Err("nested combos are unsupported".into()),
                        }
                    };
                    if !cfg.providers.contains_key(&provider) || model.is_empty() {
                        return Err(format!("invalid member {token}"));
                    }
                    if !members
                        .iter()
                        .any(|m: &ComboMember| m.provider == provider && m.model == model)
                    {
                        members.push(ComboMember {
                            provider,
                            model,
                            max_retries: None,
                        });
                    }
                }
                if members.is_empty() {
                    return Err("at least one provider/model member required".into());
                }
                let mut combo = cfg.combos.get(name).cloned().unwrap_or(Combo {
                    mode,
                    members: vec![],
                    requires: Default::default(),
                    cost_ceiling: None,
                    persistent_rotation: false,
                });
                combo.mode = mode;
                combo.members = members;
                cfg.combos.insert(name.into(), combo);
                self.save_live_config(cfg)?;
                self.open_catalog("combos");
                Ok(format!("combo:{name} saved and active"))
            }
            "remove" | "delete" => {
                if self.cfg.model_ref == format!("combo:{name}") {
                    return Err("switch models before removing the active combo".into());
                }
                if cfg.combos.remove(name).is_none() {
                    return Err("unknown combo".into());
                }
                self.save_live_config(cfg)?;
                self.open_catalog("combos");
                Ok(format!("removed combo {name}"))
            }
            _ => Err(
                "/combos [list|add|edit|remove] [NAME] [fallback|round_robin] provider/model ..."
                    .into(),
            ),
        }
    }
    /// /mcp — browse, add, edit, remove and reconnect MCP servers. Every change
    /// reconnects so the new tool set is live without restarting the session.
    pub(crate) async fn mcp_command(&mut self, args: Vec<String>) -> Result<String, String> {
        let op = args.first().map(String::as_str).unwrap_or("browse");
        let name = args.get(1).map(String::as_str).unwrap_or("");
        if op == "browse" || (op == "list" && self.events.is_some()) {
            self.open_catalog("mcps");
            return Ok(String::new());
        }
        if op == "list" {
            let c = self.catalog();
            if c.mcps.is_empty() {
                return Ok(
                    "no MCP servers configured — /mcp add NAME PORT|URL|COMMAND [ARGS...]".into(),
                );
            }
            return Ok(c
                .mcps
                .iter()
                .map(|m| {
                    format!(
                        "{}  {} {}  {} tools{}",
                        m.name,
                        m.command,
                        m.args.join(" "),
                        m.tools,
                        if m.error.is_empty() {
                            String::new()
                        } else {
                            format!("  ({})", m.error)
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"));
        }
        let (mut cfg, _) =
            Config::load_or_builtin(&self.loon_paths()).map_err(|e| e.to_string())?;
        match op {
            "add" | "edit" if args.len() <= 2 && self.events.is_some() => {
                let existing = cfg.mcp.servers.get(name);
                if op == "edit" && existing.is_none() {
                    return Err("unknown MCP server".into());
                }
                let http = existing.map(|s| s.url.is_some()).unwrap_or(true);
                let values = if http {
                    vec![
                        if op == "edit" {
                            name.into()
                        } else {
                            String::new()
                        },
                        existing.and_then(|s| s.url.clone()).unwrap_or_default(),
                        existing.map(|s| s.timeout_secs).unwrap_or(60).to_string(),
                    ]
                } else {
                    let s = existing.unwrap();
                    vec![
                        name.into(),
                        s.command.clone(),
                        s.args.join(" "),
                        String::new(),
                        s.timeout_secs.to_string(),
                    ]
                };
                self.emit(AgentEvent::OpenEditor(Editor {
                    kind: if http { "mcp_http" } else { "mcp" }.into(),
                    fields: vec![FieldSpec::text(); values.len()],
                    values,
                    editing: op == "edit",
                }));
                Ok(String::new())
            }
            "add-http" | "edit-http" => {
                valid_name(name)?;
                if op == "add-http" && cfg.mcp.servers.contains_key(name) {
                    return Err("MCP server already exists".into());
                }
                let url =
                    crate::mcp::normalize_endpoint(args.get(2).ok_or("provide a port or URL")?)?
                        .ok_or("provide a port or http(s) URL")?;
                let timeout = args
                    .get(3)
                    .map(|s| s.parse::<u64>())
                    .transpose()
                    .map_err(|_| "invalid timeout")?
                    .unwrap_or(60);
                if timeout == 0 || timeout > 600 {
                    return Err("timeout must be 1–600 seconds".into());
                }
                cfg.mcp.servers.insert(
                    name.into(),
                    loon_config::McpServerConfig {
                        command: String::new(),
                        url: Some(url),
                        args: vec![],
                        env: Default::default(),
                        timeout_secs: timeout,
                    },
                );
                self.save_live_config(cfg)?;
                self.reload_mcp().await?;
                self.open_catalog("mcps");
                Ok(format!("saved MCP endpoint {name}"))
            }
            "add" | "edit" | "save" => {
                valid_name(name)?;
                if op == "add" && cfg.mcp.servers.contains_key(name) {
                    return Err("MCP server already exists; use edit".into());
                }
                if op == "edit" && !cfg.mcp.servers.contains_key(name) {
                    return Err("unknown MCP server".into());
                }
                let command = args
                    .get(2)
                    .map(|c| c.trim())
                    .filter(|c| !c.is_empty())
                    .ok_or("usage: /mcp add NAME PORT|URL|COMMAND [ARGS...]")?;
                let mut server =
                    cfg.mcp
                        .servers
                        .get(name)
                        .cloned()
                        .unwrap_or(loon_config::McpServerConfig {
                            command: String::new(),
                            url: None,
                            args: vec![],
                            env: Default::default(),
                            timeout_secs: 60,
                        });
                let endpoint = crate::mcp::normalize_endpoint(command)?;
                server.url = endpoint;
                server.command = if server.url.is_some() {
                    String::new()
                } else {
                    command.to_string()
                };
                server.args = args[3..]
                    .iter()
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty())
                    .collect();
                if server.url.is_some() && !server.args.is_empty() {
                    return Err(
                        "HTTP MCP servers take a port or URL, without command arguments".into(),
                    );
                }
                cfg.mcp.servers.insert(name.into(), server);
                self.save_live_config(cfg)?;
                self.reload_mcp().await?;
                if self.events.is_some() {
                    self.open_catalog("mcps");
                }
                Ok(format!("MCP server {name} saved and connected"))
            }
            "remove" | "delete" => {
                if cfg.mcp.servers.remove(name).is_none() {
                    return Err("unknown MCP server".into());
                }
                self.save_live_config(cfg)?;
                self.reload_mcp().await?;
                if self.events.is_some() {
                    self.open_catalog("mcps");
                }
                Ok(format!("removed MCP server {name}"))
            }
            "reload" | "refresh" => {
                self.reload_mcp().await?;
                let c = self.catalog();
                let tools: usize = c.mcps.iter().map(|m| m.tools).sum();
                Ok(format!(
                    "reconnected {} MCP server(s), {tools} tools available",
                    c.mcps.len()
                ))
            }
            _ => Err("/mcp [list|add NAME COMMAND [ARGS...]|edit NAME|remove NAME|reload]".into()),
        }
    }
}
impl Agent {
    /// Reconnect every configured MCP server and swap the live tool set. Failures are
    /// reported per server instead of aborting the others.
    pub(crate) async fn reload_mcp(&mut self) -> Result<(), String> {
        let (cfg, _) = Config::load_effective(&self.loon_paths(), self.ctx.ws.root())
            .map_err(|e| e.to_string())?;
        let (tools, errors) = crate::mcp::connect_all(&cfg.mcp.servers).await;
        self.mcp_errors = cfg
            .mcp
            .servers
            .keys()
            .filter_map(|name| {
                errors
                    .iter()
                    .find(|e| e.contains(&format!("`{name}`")))
                    .map(|e| (name.clone(), e.clone()))
            })
            .collect();
        // Drop the old MCP tools before adding the new ones.
        let names: Vec<String> = self
            .tools
            .specs()
            .iter()
            .map(|s| s.name.clone())
            .filter(|n| n.starts_with("mcp_"))
            .collect();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        self.tools = self.tools.clone().without(&borrowed).with_extra(tools);
        self.loaded_mcp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.refresh_tool_search();
        self.refresh_delegate();
        for e in &errors {
            self.emit(AgentEvent::Warning(e.clone()));
        }
        self.emit(AgentEvent::CatalogUpdated {
            catalog: self.catalog(),
        });
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "MCP connection failed (configuration retained): {}",
                errors.join("; ")
            ))
        }
    }
}

async fn discover(registry: &ProviderRegistry, name: &str) -> Result<Vec<String>, String> {
    let mut endpoint = registry.endpoint_for(name)?;
    endpoint.timeout = endpoint.timeout.min(std::time::Duration::from_secs(15));
    loon_providers::HttpTransport::new()
        .map_err(|e| e.to_string())?
        .list_models(&endpoint)
        .await
        .map_err(|e| e.to_string())
}

impl Agent {
    pub fn set_permission_mode(&mut self, mode: loon_config::PermissionMode) {
        self.cfg.permissions.mode = mode;
        self.permissions = Arc::new(crate::permissions::PermissionEngine::new(
            &self.cfg.permissions,
            Some(self.loon_paths().state_dir.join("audit.jsonl")),
        ));
        self.refresh_delegate();
        self.refresh_tool_search();
    }
    /// Rebuild the subagent toolset against the CURRENT config and link it to the
    /// session's task graph: children get a clone of the parent's worklog (same lock), so
    /// `task_fanout` can hand tasks to children and those children can mark their own
    /// task. Called once at session start, after the worklog exists.
    pub fn refresh_orchestration(&mut self) {
        self.refresh_delegate();
    }

    pub(crate) fn refresh_delegate(&mut self) {
        self.cfg.extra_tools = self
            .tools
            .specs()
            .iter()
            .filter(|s| s.name.starts_with("mcp_"))
            .filter_map(|s| self.tools.get(&s.name))
            .collect();
        if self.tools.get("delegate").is_some() {
            // Rebuild delegation against the CURRENT config, but keep the background
            // hub: it owns the running children, so a fresh one would orphan them and
            // hide their reports from background_list.
            let spec = crate::subagent::ChildSpec {
                paths: self.loon_paths(),
                cfg: self.cfg.clone(),
                registry: self.registry.clone(),
                cwd: self.ctx.ws.root().to_path_buf(),
                // Children share the parent's task graph so a fanned-out child can mark
                // its own task. The clone shares the lock, so writes are serialised.
                tasks: Some(self.worklog.clone()),
                parent: Some(self.session_id()),
            };
            self.tools = self
                .tools
                .clone()
                .without(&[
                    "delegate",
                    "background_spawn",
                    "background_list",
                    "background_message",
                    "background_kill",
                ])
                .with_extra(vec![
                    crate::subagent::delegate_tool(
                        spec.paths.clone(),
                        spec.cfg.clone(),
                        spec.registry.clone(),
                        spec.cwd.clone(),
                        Some(self.worklog.clone()),
                    ),
                    std::sync::Arc::new(crate::subagent::BackgroundSpawnTool {
                        spec,
                        hub: self.background.clone(),
                    }),
                    std::sync::Arc::new(crate::subagent::BackgroundListTool {
                        hub: self.background.clone(),
                    }),
                    std::sync::Arc::new(crate::subagent::BackgroundMessageTool {
                        hub: self.background.clone(),
                    }),
                    std::sync::Arc::new(crate::subagent::BackgroundKillTool {
                        hub: self.background.clone(),
                    }),
                ]);
        }
    }
    pub(crate) async fn refresh_catalogs(&mut self, names: Vec<String>) {
        self.emit(AgentEvent::MemberNote(format!(
            "fetching models from {} provider(s)…",
            names.len()
        )));
        let mut jobs = tokio::task::JoinSet::new();
        for name in names {
            let registry = self.registry.clone();
            jobs.spawn(async move {
                let result = discover(&registry, &name).await;
                (name, result)
            });
        }
        while let Some(Ok((name, result))) = jobs.join_next().await {
            match result {
                Ok(ids) => {
                    if let Err(e) = self.install_models(&name, ids) {
                        self.emit(AgentEvent::Warning(e));
                    }
                }
                Err(e) => self.emit(AgentEvent::Warning(format!(
                    "{name}: {e}; cached models kept. /providers refresh {name} retries."
                ))),
            }
        }
    }
}
