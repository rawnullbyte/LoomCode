//! Provider registry: configuration → live endpoints, model aliases, combos.
//!
//! Credentials resolve lazily per request from the environment (`api_key_env`); they are
//! never stored in the registry, config, journal, or checkpoints.

use crate::chat::ChatCompletionsAdapter;
use crate::http::{Endpoint, HttpTransport};
use crate::mock::MockProvider;
use crate::responses::ResponsesAdapter;
use loon_config::{ApiFamily, CapState, Combo, ModelDef, Provider};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilitySnapshot {
    pub tool_calling: CapState,
    pub vision: CapState,
}

#[derive(Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Provider>,
    models: BTreeMap<String, ModelDef>,
    combos: BTreeMap<String, Combo>,
    /// Shared pooled client; kept here so all adapters and future surfaces
    /// (files/batch/embeddings) reuse one connection pool.
    #[allow(dead_code)]
    http: Arc<HttpTransport>,
    pub chat: Arc<ChatCompletionsAdapter>,
    pub responses: Arc<ResponsesAdapter>,
    pub mock: Arc<MockProvider>,
    /// Providers that asked us to back off, and until when. Shared across clones so a
    /// cooldown earned in one request is honoured by every later one.
    cooldowns: Arc<std::sync::Mutex<BTreeMap<String, std::time::Instant>>>,
}

/// What a model reference (`alias`, `provider/model`, or `combo:NAME`) resolved to.
#[derive(Clone)]
pub enum ResolvedModel {
    Single {
        provider_id: String,
        model_id: String,
    },
    Combo {
        name: String,
    },
}

impl ProviderRegistry {
    pub fn from_config(cfg: &loon_config::Config) -> Result<ProviderRegistry, String> {
        let http = Arc::new(HttpTransport::new().map_err(|e| e.to_string())?);
        let mock = Arc::new(MockProvider::new());
        let chat = Arc::new(ChatCompletionsAdapter::new(http.clone()));
        let responses = Arc::new(ResponsesAdapter::new(http.clone()));
        Ok(ProviderRegistry {
            providers: cfg.providers.clone(),
            models: cfg.models.clone(),
            combos: cfg.combos.clone(),
            http,
            chat,
            responses,
            mock,
            cooldowns: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        })
    }

    /// Test helper: registry with scripted mock behavior.
    pub fn with_mock(mock: Arc<MockProvider>) -> ProviderRegistry {
        let http = Arc::new(HttpTransport::default());
        let chat = Arc::new(ChatCompletionsAdapter::new(http.clone()));
        let responses = Arc::new(ResponsesAdapter::new(http.clone()));
        ProviderRegistry {
            providers: ConfigShim::builtin_providers(),
            models: ConfigShim::builtin_models(),
            combos: BTreeMap::new(),
            http,
            chat,
            responses,
            mock,
            cooldowns: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// Reload definitions without replacing pooled transports or the mock script queue.
    pub fn reconfigure(&mut self, cfg: &loon_config::Config) {
        self.providers = cfg.providers.clone();
        self.models = cfg.models.clone();
        self.combos = cfg.combos.clone();
        self.cooldowns
            .lock()
            .expect("cooldown lock")
            .retain(|name, _| self.providers.contains_key(name));
    }

    /// Remember that a provider asked us to wait. `model` scopes the cooldown when the
    /// failure was model-specific (a rate limit usually is per account, so the provider
    /// id is the default key).
    pub fn note_cooldown(&self, provider: &str, wait: std::time::Duration) {
        let until = std::time::Instant::now() + wait.min(std::time::Duration::from_secs(300));
        self.cooldowns
            .lock()
            .expect("cooldown lock")
            .insert(provider.to_string(), until);
    }

    /// Remaining cooldown for a provider, or `None` when it may be used now.
    pub fn cooldown_remaining(&self, provider: &str) -> Option<std::time::Duration> {
        let mut guard = self.cooldowns.lock().expect("cooldown lock");
        let now = std::time::Instant::now();
        match guard.get(provider) {
            Some(until) if *until > now => Some(*until - now),
            Some(_) => {
                guard.remove(provider);
                None
            }
            None => None,
        }
    }

    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    pub fn model_refs(&self) -> Vec<String> {
        let mut refs: Vec<String> = self
            .models
            .values()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        for (name, p) in &self.providers {
            refs.extend(p.models.iter().map(|id| format!("{name}/{id}")));
        }
        refs.sort();
        refs.dedup();
        refs
    }

    pub fn reasoning_level(&self, provider: &str, requested: &str) -> Option<String> {
        let levels = &self.providers.get(provider)?.reasoning_efforts;
        let order = ["low", "medium", "high", "xhigh", "max"];
        let rank = order.iter().position(|s| *s == requested)?;
        order[..=rank]
            .iter()
            .rev()
            .find(|s| levels.iter().any(|l| l == **s))
            .map(|s| s.to_string())
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.get(id)
    }

    pub fn model_def(&self, alias: &str) -> Option<&ModelDef> {
        self.models.get(alias)
    }

    pub fn combo(&self, name: &str) -> Option<&Combo> {
        self.combos.get(name)
    }

    pub fn combo_names(&self) -> Vec<String> {
        self.combos.keys().cloned().collect()
    }

    /// All selectable model references: model names and `combo:NAME` entries.
    pub fn list_refs(&self) -> Vec<String> {
        let mut out: Vec<String> = self.models.keys().cloned().collect();
        out.extend(self.model_refs());
        let mut combos: Vec<String> = self.combos.keys().map(|c| format!("combo:{c}")).collect();
        out.append(&mut combos);
        out.sort();
        out.dedup();
        out
    }

    pub fn resolve(&self, model_ref: &str) -> Result<ResolvedModel, String> {
        if let Some(name) = model_ref.strip_prefix("combo:") {
            if self.combos.contains_key(name) {
                return Ok(ResolvedModel::Combo {
                    name: name.to_string(),
                });
            }
            return Err(format!(
                "combo `{name}` is not configured (known: {:?})",
                self.combo_names()
            ));
        }
        if self.models.contains_key(model_ref) {
            return Ok(ResolvedModel::Single {
                provider_id: self.models[model_ref].provider.clone(),
                model_id: self.models[model_ref].id.clone(),
            });
        }
        // `provider/model` form
        if let Some((p, m)) = model_ref.split_once('/') {
            if self.providers.contains_key(p) && !m.trim().is_empty() {
                return Ok(ResolvedModel::Single {
                    provider_id: p.to_string(),
                    model_id: m.to_string(),
                });
            }
        }
        Err(format!(
            "model reference `{model_ref}` matches no alias or combo (aliases: {:?}, combos: {:?})",
            self.models.keys().collect::<Vec<_>>(),
            self.combo_names()
        ))
    }

    /// Effective endpoint for a provider. Credentials resolve here, per request, from the
    /// environment — and never leave this origin.
    pub fn endpoint_for(&self, provider_id: &str) -> Result<Endpoint, String> {
        let p = self
            .providers
            .get(provider_id)
            .ok_or_else(|| format!("unknown provider `{provider_id}`"))?;
        if p.mock {
            return Ok(Endpoint::from_parts(
                "http://mock.invalid/v1",
                None,
                &p.headers,
                std::time::Duration::from_secs(p.timeout_secs),
            ));
        }
        // Credential precedence: an inline key written by /provider wins; otherwise
        // the configured env var must actually resolve (empty/unset is an error, not
        // a silent anonymous request).
        let api_key = match (&p.api_key, &p.api_key_env) {
            (Some(k), _) if !k.trim().is_empty() => Some(k.clone()),
            (_, Some(envv)) => match std::env::var(envv) {
                Ok(v) if !v.trim().is_empty() => Some(v),
                _ => {
                    return Err(format!(
                        "provider `{provider_id}` needs its API key in env var `{envv}` (not set or empty)"
                    ))
                }
            },
            _ => None,
        };
        Ok(Endpoint::from_parts(
            &p.base_url,
            api_key,
            &p.headers,
            std::time::Duration::from_secs(p.timeout_secs),
        ))
    }

    pub fn api_family(&self, provider_id: &str) -> ApiFamily {
        self.providers
            .get(provider_id)
            .map(|p| p.api)
            .unwrap_or_default()
    }

    /// Merged capability view for (provider, model): config overrides win over adapter
    /// defaults; anything unset stays `unknown`, and unknown is never treated as supported.
    pub fn capability(&self, provider_id: &str, model_ref: &str) -> CapabilitySnapshot {
        let mut snap = CapabilitySnapshot {
            tool_calling: CapState::Unknown,
            vision: CapState::Unknown,
        };
        if let Some(m) = self
            .models
            .get(model_ref)
            .filter(|m| m.provider == provider_id)
            .or_else(|| {
                self.models
                    .values()
                    .find(|m| m.provider == provider_id && m.id == model_ref)
            })
        {
            if let Some(t) = m.capabilities.tool_calling {
                snap.tool_calling = t;
            }
            if let Some(v) = m.capabilities.vision {
                snap.vision = v;
            }
        }
        snap
    }

    pub fn output_limit(&self, provider: &str, model: &str) -> Option<u64> {
        self.models
            .values()
            .find(|m| m.provider == provider && m.id == model)
            .and_then(|m| m.max_output_tokens)
    }

    pub fn context_window(&self, provider_id: &str, model_ref: &str) -> Option<u64> {
        self.models
            .get(model_ref)
            .filter(|m| m.provider == provider_id)
            .or_else(|| {
                self.models
                    .values()
                    .find(|m| m.provider == provider_id && m.id == model_ref)
            })
            .and_then(|m| m.context_window)
    }

    pub fn is_mock(&self, provider_id: &str) -> bool {
        self.providers
            .get(provider_id)
            .map(|p| p.mock)
            .unwrap_or(false)
    }
}

/// Minimal config-shaped defaults so `with_mock` needs no full Config.
struct ConfigShim;
impl ConfigShim {
    fn builtin_providers() -> BTreeMap<String, Provider> {
        let mut m = BTreeMap::new();
        m.insert(
            "mock".into(),
            Provider {
                base_url: "http://mock.invalid/v1".into(),
                mock: true,
                ..Default::default()
            },
        );
        m
    }
    fn builtin_models() -> BTreeMap<String, ModelDef> {
        let mut m = BTreeMap::new();
        m.insert(
            "mock".into(),
            ModelDef {
                provider: "mock".into(),
                id: "mock-1".into(),
                context_window: Some(128_000),
                max_output_tokens: None,
                capabilities: Default::default(),
            },
        );
        m
    }
}
