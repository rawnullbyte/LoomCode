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
        }
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
        let mut combos: Vec<String> = self.combos.keys().map(|c| format!("combo:{c}")).collect();
        out.append(&mut combos);
        out.sort();
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
            if self.providers.contains_key(p) {
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
        let api_key = match &p.api_key_env {
            Some(envv) => match std::env::var(envv) {
                Ok(v) if !v.trim().is_empty() => Some(v),
                _ => {
                    return Err(format!(
                        "provider `{provider_id}` needs its API key in env var `{envv}` (not set or empty)"
                    ))
                }
            },
            None => None,
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
