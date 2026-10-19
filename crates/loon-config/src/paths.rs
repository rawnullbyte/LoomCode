//! Loon's on-disk locations. `LOON_HOME` (test/isolation override) nests everything under
//! one directory; otherwise XDG base dirs are honored with documented fallbacks.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct LoonPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl LoonPaths {
    pub fn resolve() -> LoonPaths {
        if let Ok(home) = std::env::var("LOON_HOME") {
            if !home.trim().is_empty() {
                let base = PathBuf::from(home);
                return LoonPaths {
                    config_dir: base.join("config"),
                    data_dir: base.join("data"),
                    state_dir: base.join("state"),
                    cache_dir: base.join("cache"),
                    runtime_dir: base.join("runtime"),
                };
            }
        }
        let config = xdg_dir("XDG_CONFIG_HOME", ".config");
        let data = xdg_dir("XDG_DATA_HOME", ".local/share");
        let state = xdg_dir("XDG_STATE_HOME", ".local/state");
        let cache = xdg_dir("XDG_CACHE_HOME", ".cache");
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| state.join("runtime"));
        LoonPaths {
            config_dir: config.join("loon"),
            data_dir: data.join("loon"),
            state_dir: state.join("loon"),
            cache_dir: cache.join("loon"),
            runtime_dir: runtime.join("loon"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// Create the directories Loon needs up front. Missing XDG_RUNTIME_DIR fallback uses
    /// state dir with 0700 perms.
    pub fn ensure(&self) -> std::io::Result<()> {
        for d in [
            &self.config_dir,
            &self.data_dir,
            &self.state_dir,
            &self.cache_dir,
            &self.runtime_dir,
        ] {
            std::fs::create_dir_all(d)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.runtime_dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

fn xdg_dir(env: &str, fallback: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env) {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return p;
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(fallback)
}
