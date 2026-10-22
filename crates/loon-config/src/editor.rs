//! Typed UI snapshots and sparse, comment-preserving writes at each configuration scope.
use crate::{Config, LoonPaths, Validator};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Local,
}
impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
    fn index(self) -> usize {
        match self {
            Self::User => 0,
            Self::Project => 1,
            Self::Local => 2,
        }
    }
}
pub struct Snapshot {
    pub scope: Scope,
    pub config: Config,
    paths: [PathBuf; 3],
    sources: [Option<String>; 3],
}
pub struct Prepared {
    pub effective: Config,
    pub selected: Config,
    text: String,
    path: PathBuf,
}
fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Unable to read a configuration file".into()),
    }
}
fn compose(sources: &[Option<String>; 3], last: usize) -> Result<Config, String> {
    let base = if let Some(text) = &sources[0] {
        toml::from_str::<Config>(text)
            .map_err(|_| "User configuration is not valid TOML or has invalid field types")?
    } else {
        Config::builtin()
    };
    let mut value =
        toml::Value::try_from(base).map_err(|_| "Unable to read configuration values")?;
    for text in sources.iter().take(last + 1).skip(1).flatten() {
        let overlay: toml::Value =
            toml::from_str(text).map_err(|_| "Project/local configuration is not valid TOML")?;
        crate::merge_settings(&mut value, overlay);
    }
    value
        .try_into()
        .map_err(|_| "Configuration has an invalid field type".into())
}
fn validate(cfg: &Config) -> Result<(), String> {
    let errors = Validator::new(cfg).errors();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors
            .iter()
            .map(|e| format!("{}: {}", e.path, e.message))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
impl Snapshot {
    pub fn load(paths: &LoonPaths, cwd: &Path, scope: Scope) -> Result<Self, String> {
        let paths = [
            paths.config_file(),
            cwd.join(".loon/config.toml"),
            cwd.join(".loon/config.local.toml"),
        ];
        let sources = [read(&paths[0])?, read(&paths[1])?, read(&paths[2])?];
        let config = compose(&sources, scope.index())?;
        Ok(Self {
            scope,
            config,
            paths,
            sources,
        })
    }
    fn unchanged(&self) -> Result<(), String> {
        for (i, path) in self.paths.iter().enumerate() {
            if read(path)? != self.sources[i] {
                return Err("Configuration changed outside this editor. Save nothing; discard/reload the draft before retrying.".into());
            }
        }
        Ok(())
    }
    pub fn prepare(&self, candidate: &Config) -> Result<Prepared, String> {
        self.unchanged()?;
        validate(candidate)?;
        let before =
            toml::Value::try_from(&self.config).map_err(|_| "Invalid original configuration")?;
        let after =
            toml::Value::try_from(candidate).map_err(|_| "Invalid candidate configuration")?;
        let full: toml_edit::DocumentMut = toml::to_string_pretty(candidate)
            .map_err(|_| "Unable to serialize settings")?
            .parse()
            .map_err(|_| "Unable to serialize settings")?;
        let i = self.scope.index();
        let initial = if i == 0 && self.sources[i].is_none() {
            toml::to_string_pretty(&self.config).map_err(|_| "Unable to serialize defaults")?
        } else {
            self.sources[i].clone().unwrap_or_default()
        };
        let mut doc: toml_edit::DocumentMut = initial
            .parse()
            .map_err(|_| "Existing configuration is not valid TOML")?;
        if i > 0 {
            let parent = toml::Value::try_from(compose(&self.sources, i - 1)?)
                .map_err(|_| "Invalid inherited settings")?;
            reject_inherited_removal("", &before, &after, &parent)?;
        }
        patch(&before, &after, full.as_table(), doc.as_table_mut());
        let text = doc.to_string();
        let mut sources = self.sources.clone();
        sources[i] = Some(text.clone());
        let selected = compose(&sources, i)?;
        let effective = compose(&sources, 2)?;
        validate(&selected)?;
        validate(&effective)?;
        Ok(Prepared {
            effective,
            selected,
            text,
            path: self.paths[i].clone(),
        })
    }
    pub fn commit(&self, prepared: &Prepared) -> Result<(), String> {
        self.unchanged()?;
        if let Some(parent) = prepared.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| "Unable to create the settings directory")?;
        }
        crate::write_atomic(&prepared.path, prepared.text.clone())
            .map_err(|_| "Unable to save configuration".to_string())
    }
}
fn patch(
    before: &toml::Value,
    after: &toml::Value,
    source: &dyn toml_edit::TableLike,
    dest: &mut dyn toml_edit::TableLike,
) {
    let (Some(old), Some(new)) = (before.as_table(), after.as_table()) else {
        return;
    };
    for name in old.keys() {
        if !new.contains_key(name) {
            dest.remove(name);
        }
    }
    for (name, value) in new {
        if old.get(name) == Some(value) {
            continue;
        }
        if let (Some(previous), Some(next_source)) = (
            old.get(name).filter(|v| v.is_table() && value.is_table()),
            source.get(name).and_then(toml_edit::Item::as_table_like),
        ) {
            if dest
                .get(name)
                .and_then(toml_edit::Item::as_table_like)
                .is_none()
            {
                dest.insert(name, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            patch(
                previous,
                value,
                next_source,
                dest.get_mut(name).unwrap().as_table_like_mut().unwrap(),
            );
        } else if let Some(item) = source.get(name) {
            dest.insert(name, item.clone());
        }
    }
}
fn named_map(path: &str) -> bool {
    matches!(
        path,
        "/providers" | "/models" | "/combos" | "/mcp/servers" | "/hooks"
    ) || path.ends_with("/headers")
        || path.ends_with("/env")
}
fn reject_inherited_removal(
    path: &str,
    before: &toml::Value,
    after: &toml::Value,
    inherited: &toml::Value,
) -> Result<(), String> {
    if let (Some(old), Some(new)) = (before.as_table(), after.as_table()) {
        for (name, value) in old {
            if !new.contains_key(name) && named_map(path) && inherited.get(name).is_some() {
                return Err("An inherited named entry cannot be removed by an overlay. Select its parent scope to remove it.".into());
            }
            if let (Some(next), Some(parent)) = (new.get(name), inherited.get(name)) {
                reject_inherited_removal(&format!("{path}/{name}"), value, next, parent)?;
            }
        }
    }
    Ok(())
}
