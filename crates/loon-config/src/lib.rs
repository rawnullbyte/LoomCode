//! Loon configuration: XDG path resolution, TOML schema, validation.
//!
//! Precedence: built-in defaults → user `config.toml` → CLI/session overrides (applied by
//! callers on top of a loaded [`Config`]). Saving goes through `toml_edit` so user comments
//! and unknown extension fields survive a round trip.

pub mod editor;
pub mod paths;
pub mod schema;
pub mod tokens;
pub mod validate;

pub use paths::LoonPaths;
pub use schema::{
    ApiFamily, Appearance, AppearanceMode, CapState, CapabilityOverrides, Combo, ComboMember,
    ComboMode, ComboRequires, Effort, Mcp, McpServerConfig, ModelDef, PermissionMode,
    PermissionRule, Permissions, Provider, SessionCfg, UiCfg,
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
    pub ui: UiCfg,
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, Provider>,
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, ModelDef>,
    #[serde(default)]
    pub combos: std::collections::BTreeMap<String, Combo>,
    #[serde(default)]
    pub mcp: Mcp,
    #[serde(default)]
    pub context7: schema::Context7,
    /// Lifecycle hooks keyed by event name.
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, Vec<schema::Hook>>,
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
                api_key: None,
                headers: Default::default(),
                timeout_secs: 30,
                models: vec![],
                mock: true,
                reasoning_efforts: vec![],
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
            ui: UiCfg::default(),
            providers,
            models,
            combos: Default::default(),
            mcp: Mcp::default(),
            context7: Default::default(),
            hooks: Default::default(),
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

    /// User settings followed by project and local overlays. Scalars override;
    /// tables merge recursively; arrays replace (never silently broaden permission lists).
    pub fn load_effective(
        paths: &LoonPaths,
        cwd: &std::path::Path,
    ) -> Result<(Config, Vec<Finding>), ConfigError> {
        let (base, _) = Self::load_or_builtin(paths)?;
        let cfg = base.with_overlays(cwd)?;
        let findings = Validator::new(&cfg).validate();
        Ok((cfg, findings))
    }

    /// Apply project/local overlays to this candidate, including before an interactive save.
    pub fn with_overlays(&self, cwd: &std::path::Path) -> Result<Config, ConfigError> {
        let base = self;
        let mut value = toml::Value::try_from(base).map_err(|e| ConfigError::Parse {
            path: cwd.join(".loon/config.toml"),
            message: e.to_string(),
        })?;
        for path in [
            cwd.join(".loon/config.toml"),
            cwd.join(".loon/config.local.toml"),
        ] {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let overlay: toml::Value =
                        toml::from_str(&text).map_err(|e| ConfigError::Parse {
                            path: path.clone(),
                            message: e.to_string(),
                        })?;
                    merge_settings(&mut value, overlay);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ConfigError::Io { path, source: e }),
            }
        }
        let cfg: Config = value
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Parse {
                path: cwd.join(".loon/config.toml"),
                message: e.to_string(),
            })?;
        Ok(cfg)
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
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp).map_err(|e| ConfigError::Io {
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

[ui]
# Syntax highlighting in code excerpts and fenced blocks. Set false for a plain
# transcript — useful when a terminal theme fights the token colours, and faster
# on very large outputs.
syntax = true
# Rotating placeholder tip in the prompt ("Try \"write a test for src/lib.rs\"").
tips = true
# Permission-mode footer row.
footer = true

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

fn merge_settings(base: &mut toml::Value, overlay: toml::Value) {
    if let (Some(table), toml::Value::Table(next)) = (base.as_table_mut(), &overlay) {
        for (key, value) in next {
            if let Some(existing) = table.get_mut(key) {
                merge_settings(existing, value.clone());
            } else {
                table.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overlay;
    }
}

#[cfg(test)]
mod example_config_tests {
    use super::*;

    /// `loom config init` writes `example_toml()`, so it must parse as a typed `Config` — a
    /// comment or key that drifts out of sync with the schema would ship a broken starter
    /// file to every new user.
    #[test]
    fn the_example_config_parses_into_a_typed_config() {
        let cfg: Config = toml::from_str(example_toml())
            .expect("the example config must parse against the current schema");
        // Spot-check that the documented `[ui]` keys actually take effect.
        assert!(cfg.ui.syntax, "the example turns syntax highlighting on");
        assert!(cfg.ui.tips);
        assert!(cfg.ui.footer);
    }

    /// Every key the ULTRA plan added must be reachable from the example, or a user cannot
    /// discover the setting without reading the source.
    #[test]
    fn the_example_documents_the_ui_switches() {
        let text = example_toml();
        for key in ["syntax", "tips", "footer"] {
            assert!(
                text.contains(key),
                "`{key}` must appear in the example config"
            );
        }
    }
}
