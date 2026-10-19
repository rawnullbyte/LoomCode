//! Cross-field validation with path-aware, actionable errors and warnings.

use crate::schema::{ApiFamily, CapState, ComboMode};
use crate::Config;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// A single validation finding. `errors` block loading; `warnings` do not.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub path: String,
    pub message: String,
    pub is_error: bool,
}

pub struct Validator<'a> {
    cfg: &'a Config,
}

impl<'a> Validator<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Validator { cfg }
    }

    pub fn validate(&self) -> Vec<Finding> {
        let mut f = Vec::new();
        let err = |path: &str, message: String| Finding {
            path: path.into(),
            message,
            is_error: true,
        };
        let warn = |path: &str, message: String| Finding {
            path: path.into(),
            message,
            is_error: false,
        };

        if self.cfg.schema_version > crate::schema::SCHEMA_VERSION {
            f.push(err(
                "schema_version",
                format!(
                    "config schema {} is newer than this build supports (<= {}); upgrade Loon",
                    self.cfg.schema_version,
                    crate::schema::SCHEMA_VERSION
                ),
            ));
        }

        if self.cfg.providers.is_empty() {
            f.push(warn(
                "providers",
                "no providers configured; only the built-in mock provider is available".into(),
            ));
        }

        for (name, p) in &self.cfg.providers {
            let base = format!("providers.{name}");
            if name.is_empty() {
                f.push(err(&base, "provider id must not be empty".into()));
            }
            if p.mock {
                continue; // built-in mock provider is exempt from URL/auth checks
            }
            match url_check(&p.base_url) {
                Ok(scheme) => {
                    if scheme != "http" && scheme != "https" {
                        f.push(err(
                            &base,
                            format!("base_url must be http(s), got scheme `{scheme}`"),
                        ));
                    }
                }
                Err(e) => f.push(err(
                    &base,
                    format!("base_url `{}` is not a valid URL: {e}", p.base_url),
                )),
            }
            if let Some(envv) = &p.api_key_env {
                if envv.trim().is_empty() {
                    f.push(err(
                        &base,
                        "api_key_env must not be empty (omit it for no-auth endpoints)".into(),
                    ));
                }
            }
            if p.timeout_secs == 0 {
                f.push(err(
                    &format!("{base}.timeout_secs"),
                    "timeout must be > 0".into(),
                ));
            }
        }

        for (name, m) in &self.cfg.models {
            let base = format!("models.{name}");
            match self.cfg.providers.get(&m.provider) {
                None => f.push(err(
                    &base,
                    format!(
                        "references unknown provider `{}` (known: {:?})",
                        m.provider,
                        provider_names(self.cfg)
                    ),
                )),
                Some(p) => {
                    if !p.models.is_empty() && !p.models.iter().any(|x| x == &m.id) {
                        f.push(warn(
                            &base,
                            format!(
                                "model id `{}` is not in providers.{}.models list",
                                m.id, m.provider
                            ),
                        ));
                    }
                }
            }
            if let Some(cw) = m.context_window {
                if cw == 0 {
                    f.push(err(
                        &format!("{base}.context_window"),
                        "context_window must be > 0".into(),
                    ));
                }
            }
        }

        for (name, c) in &self.cfg.combos {
            let base = format!("combos.{name}");
            if c.members.is_empty() {
                f.push(err(&base, "combo must have at least one member".into()));
            }
            for (i, m) in c.members.iter().enumerate() {
                let mb = format!("{base}.members[{i}]");
                if !self.cfg.providers.contains_key(&m.provider) {
                    f.push(err(
                        &mb,
                        format!(
                            "references unknown provider `{}` (known: {:?})",
                            m.provider,
                            provider_names(self.cfg)
                        ),
                    ));
                }
                // Resolve the member's model: `model` may be an alias or a raw provider id.
                let model_known = self.cfg.models.iter().any(|(_n, md)| {
                    md.provider == m.provider && (md.id == m.model || _n == &m.model)
                });
                let provider_advertises = self
                    .cfg
                    .providers
                    .get(&m.provider)
                    .map(|p| p.models.iter().any(|x| x == &m.model))
                    .unwrap_or(false);
                if !model_known && !provider_advertises {
                    f.push(warn(
                        &mb,
                        format!(
                            "model `{}` not found under provider `{}`; combo will surface it as unresolvable at send time",
                            m.model, m.provider
                        ),
                    ));
                }
            }
            if c.cost_ceiling.map(|c| c <= 0.0).unwrap_or(false) {
                f.push(err(
                    &format!("{base}.cost_ceiling"),
                    "cost ceiling must be > 0".into(),
                ));
            }
            if c.persistent_rotation && c.mode != ComboMode::RoundRobin {
                f.push(err(
                    &format!("{base}.persistent_rotation"),
                    "only meaningful for round_robin combos".into(),
                ));
            }
            // Requirement vs member capability conflicts.
            for (i, m) in c.members.iter().enumerate() {
                if let Some(md) = self.cfg.models.get(&m.model).or_else(|| {
                    self.cfg
                        .models
                        .iter()
                        .find(|(_, md)| md.provider == m.provider && md.id == m.model)
                        .map(|(_, md)| md)
                }) {
                    let conflicts = |req: bool, cap: Option<CapState>| {
                        req && cap == Some(CapState::Unsupported)
                    };
                    if conflicts(c.requires.tool_calling, md.capabilities.tool_calling) {
                        f.push(err(
                            &format!("{base}.members[{i}]"),
                            "combo requires tool_calling but this member's model declares it unsupported".into(),
                        ));
                    }
                    if conflicts(c.requires.vision, md.capabilities.vision) {
                        f.push(err(
                            &format!("{base}.members[{i}]"),
                            "combo requires vision but this member's model declares it unsupported"
                                .into(),
                        ));
                    }
                    if let (Some(min), Some(cw)) =
                        (c.requires.min_context_window, md.context_window)
                    {
                        if cw < min {
                            f.push(err(
                                &format!("{base}.members[{i}]"),
                                format!(
                                    "combo requires context >= {min} tokens but member has {cw}"
                                ),
                            ));
                        }
                    }
                }
            }
        }

        // Default model must resolve to an alias or combo:NAME.
        let mref = &self.cfg.session.model;
        if let Some(cname) = mref.strip_prefix("combo:") {
            if !self.cfg.combos.contains_key(cname) {
                f.push(err(
                    "session.model",
                    format!(
                        "combo `{cname}` does not exist (known: {:?})",
                        self.cfg.combos.keys().collect::<Vec<_>>()
                    ),
                ));
            }
        } else if !self.cfg.models.contains_key(mref) {
            f.push(err(
                "session.model",
                format!(
                    "model alias `{mref}` does not exist (known: {:?})",
                    self.cfg.models.keys().collect::<Vec<_>>()
                ),
            ));
        }

        match self.cfg.appearance.mode {
            crate::schema::AppearanceMode::Transparent => {}
            crate::schema::AppearanceMode::Opaque => {
                f.push(warn("appearance.mode", "opaque mode is explicitly enabled; application-owned cells will paint a background".into()));
            }
        }

        f.sort_by(|a, b| a.path.cmp(&b.path));
        f
    }

    /// Errors only — callers that refuse to start on invalid config.
    pub fn errors(&self) -> Vec<Finding> {
        self.validate().into_iter().filter(|f| f.is_error).collect()
    }
}

fn provider_names(cfg: &Config) -> Vec<String> {
    cfg.providers.keys().cloned().collect()
}

fn url_check(s: &str) -> Result<String, String> {
    // Minimal scheme://host[:port][/path] check; avoids a url crate dependency here.
    let (scheme, rest) = s.split_once("://").ok_or("missing `://`")?;
    if scheme.is_empty() || rest.is_empty() {
        return Err("empty scheme or host".into());
    }
    if rest.starts_with('/') || rest.contains(' ') {
        return Err("invalid host".into());
    }
    Ok(scheme.to_string())
}

impl ApiFamily {
    pub fn parse(s: &str) -> Option<ApiFamily> {
        match s {
            "chat" | "chat_completions" => Some(ApiFamily::Chat),
            "responses" => Some(ApiFamily::Responses),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;
    use std::collections::BTreeMap;

    fn cfg_with(combos: BTreeMap<String, Combo>) -> Config {
        let mut c = Config::builtin();
        c.combos = combos;
        c
    }

    #[test]
    fn builtin_is_valid() {
        let c = Config::builtin();
        let errs = Validator::new(&c).errors();
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn combo_unknown_provider_is_error() {
        let mut combos = BTreeMap::new();
        combos.insert(
            "x".into(),
            Combo {
                mode: ComboMode::Fallback,
                members: vec![ComboMember {
                    provider: "nope".into(),
                    model: "m".into(),
                    max_retries: None,
                }],
                requires: Default::default(),
                cost_ceiling: None,
                persistent_rotation: false,
            },
        );
        let errs = Validator::new(&cfg_with(combos)).errors();
        assert!(errs.iter().any(|e| e.path.contains("combos.x.members[0]")
            && e.message.contains("unknown provider `nope`")));
    }

    #[test]
    fn combo_requires_tool_but_model_unsupported_is_error() {
        let mut c = Config::builtin();
        c.models.insert(
            "notools".into(),
            ModelDef {
                provider: "mock".into(),
                id: "mock-1".into(),
                context_window: Some(1000),
                max_output_tokens: None,
                capabilities: CapabilityOverrides {
                    tool_calling: Some(CapState::Unsupported),
                    ..Default::default()
                },
            },
        );
        let mut combos = BTreeMap::new();
        combos.insert(
            "x".into(),
            Combo {
                mode: ComboMode::Fallback,
                members: vec![ComboMember {
                    provider: "mock".into(),
                    model: "notools".into(),
                    max_retries: None,
                }],
                requires: ComboRequires {
                    tool_calling: true,
                    ..Default::default()
                },
                cost_ceiling: None,
                persistent_rotation: false,
            },
        );
        c.combos = combos;
        let errs = Validator::new(&c).errors();
        assert!(
            errs.iter().any(|e| e.message.contains("tool_calling")),
            "{errs:?}"
        );
    }

    #[test]
    fn default_model_must_resolve() {
        let mut c = Config::builtin();
        c.session.model = "combo:missing".into();
        let errs = Validator::new(&c).errors();
        assert!(errs.iter().any(|e| e.path == "session.model"));
    }
}
