//! Loon configuration: XDG path resolution, TOML schema, validation.
//!
//! Precedence: built-in defaults → user `config.toml` → CLI/session overrides (applied by
//! callers on top of a loaded [`Config`]). Saving goes through `toml_edit` so user comments
//! and unknown extension fields survive a round trip.

pub mod paths;
pub mod schema;
pub mod validate;

pub use paths::LoonPaths;
pub use schema::{
    ApiFamily, Appearance, AppearanceMode, CapState, CapabilityOverrides, Combo, ComboMember,
    ComboMode, ComboRequires, ModelDef, PermissionMode, PermissionRule, Permissions, Provider,
    SessionCfg,
};
pub use validate::{ConfigError, Finding, Validator};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Root config file contents. Unknown fields are preserved on save (serde deny_unknown_fields
/// is deliberately NOT used at the document level; per-table validation is done by hand).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub appearance: Appearance,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub session: SessionCfg,
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, Provider>,
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, ModelDef>,
    #[serde(default)]
    pub combos: std::collections::BTreeMap<String, Combo>,
}

impl Config {
    /// Built-in defaults: the deterministic mock provider/model so Loon is usable with no
    /// network, no credentials and no user config file.
    pub fn builtin() -> Config {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "mock".to_string(),
            Provider {
                base_url: "http://mock.invalid/v1".into(),
                api: ApiFamily::Chat,
                api_key_env: None,
                headers: Default::default(),
                timeout_secs: 30,
                models: vec![],
                mock: true,
            },
        );
        let mut models = std::collections::BTreeMap::new();
        models.insert(
            "mock".to_string(),
            ModelDef {
                provider: "mock".into(),
                id: "mock-1".into(),
                context_window: Some(128_000),
                max_output_tokens: Some(8_192),
                capabilities: Default::default(),
            },
        );
        Config {
            schema_version: schema::SCHEMA_VERSION,
            appearance: Appearance::default(),
            permissions: Permissions::default(),
            session: SessionCfg::default(),
            providers,
            models,
            combos: Default::default(),
        }
    }

    /// Load the user config if present, else the built-in defaults. Returns every
    /// validation finding; the caller decides whether errors are fatal.
    pub fn load_or_builtin(paths: &LoonPaths) -> Result<(Config, Vec<Finding>), ConfigError> {
        let path = paths.config_file();
        if !path.exists() {
            return Ok((Config::builtin(), Vec::new()));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ConfigError::Io {
            path: path.clone(),
            source: e,
        })?;
        match toml::from_str::<Config>(&text) {
            Ok(cfg) => {
                let findings = Validator::new(&cfg).validate();
                Ok((cfg, findings))
            }
            Err(e) => Err(ConfigError::Parse {
                path: path.clone(),
                message: e.to_string(),
            }),
        }
    }

    /// Save while preserving comments/unknown fields of the existing document.
    pub fn save(&self, paths: &LoonPaths) -> Result<(), ConfigError> {
        let path = paths.config_file();
        let new_text: String = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        if path.exists() {
            let old: String = std::fs::read_to_string(&path).map_err(|e| ConfigError::Io {
                path: path.clone(),
                source: e,
            })?;
            let mut doc: toml_edit::DocumentMut =
                old.parse()
                    .map_err(|e: toml_edit::TomlError| ConfigError::Parse {
                        path: path.clone(),
                        message: format!("existing config is not valid TOML: {e}"),
                    })?;
            let overlay: toml_edit::DocumentMut =
                new_text
                    .parse()
                    .map_err(|e: toml_edit::TomlError| ConfigError::Parse {
                        path: path.clone(),
                        message: e.to_string(),
                    })?;
            // Overlay known top-level tables/items onto the existing document; unknown
            // tables and comments in `doc` are left untouched.
            for (k, v) in overlay.as_table().iter() {
                doc.insert(k, v.clone());
            }
            write_atomic(&path, doc.to_string())?;
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ConfigError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            write_atomic(&path, new_text)?;
        }
        Ok(())
    }
}

fn write_atomic(path: &PathBuf, text: String) -> Result<(), ConfigError> {
    use std::io::Write;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| ConfigError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.write_all(text.as_bytes()).map_err(|e| ConfigError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.sync_all().map_err(|e| ConfigError::Io {
            path: tmp.clone(),
            source: e,
        })?;
    }
    std::fs::rename(&tmp, path).map_err(|e| ConfigError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(())
}

/// An example config users can start from (`loon config init`).
pub fn example_toml() -> &'static str {
    r#"# Loon configuration. See `loon config --help`. Comments are preserved on save.
schema_version = 1

[appearance]
# "transparent" (default) never paints an application-owned background: the terminal
# (and your compositor blur) shows through. "opaque" must be opted into explicitly.
mode = "transparent"
color = "auto"          # auto | true | 256 | 16 | mono

[permissions]
# bypass | ask | allowlist. Bypass skips approval prompts but explicit constraints
# below still apply.
mode = "bypass"

[session]
# Model reference: a [models] key or "combo:NAME".
model = "mock"
max_steps = 24

# ── Providers: any number of OpenAI-compatible endpoints ─────────────────────────
# [providers.ollama]
# base_url = "http://127.0.0.1:11434/v1"
# api = "chat"              # chat | responses
# api_key_env = ""          # env var holding the key, if the endpoint needs one
# models = ["llama3.1:8b"]

# ── Models: named references into a provider ─────────────────────────────────────
# [models.local]
# provider = "ollama"
# id = "llama3.1:8b"
# context_window = 131072

# ── Combos: ordered model sets across any providers ──────────────────────────────
# [combos.daily-driver]
# mode = "fallback"          # fallback | round_robin
# members = [
#   { provider = "ollama", model = "llama3.1:8b" },
# ]
"#
}
