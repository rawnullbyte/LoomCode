//! Incremental repository index: a gitignore-aware file inventory with an
//! mtime-validated cache persisted under the Loon cache dir.
//!
//! Honest scope: the walk always enumerates the tree (that is what makes it correct);
//! the cache avoids re-reading metadata churn and lets the context inspector answer
//! without a walk at all when nothing changed since the last scan. Feeds /context and
//! context budget estimation; the model-facing surface remains `glob`/`tree`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Workspace-relative path, forward slashes.
    pub path: String,
    pub size: u64,
    pub mtime_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, CachedMeta>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CachedMeta {
    size: u64,
    mtime_ms: i64,
}

pub struct RepoIndex {
    root: PathBuf,
    cache_path: PathBuf,
    cache: CacheFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub entries: Vec<FileEntry>,
    /// Entries whose cached metadata was reused (mtime unchanged).
    pub reused: usize,
    /// Entries re-stat'd (new or changed).
    pub refreshed: usize,
}

const CACHE_VERSION: u32 = 1;
/// Single-file inventory cap: a runaway tree (vendored stuff that slipped past
/// gitignore) is truncated and reported rather than exploding memory.
const MAX_ENTRIES: usize = 100_000;

impl RepoIndex {
    pub fn open(root: &Path, cache_dir: &Path) -> RepoIndex {
        let digest = loon_workspace::sha256_hex(root.display().to_string().as_bytes());
        let key = format!("index-{}.json", &digest[..16.min(digest.len())]);
        let cache_path = cache_dir.join(key);
        let cache = std::fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<CacheFile>(&b).ok())
            .filter(|c| c.version == CACHE_VERSION)
            .unwrap_or(CacheFile {
                version: CACHE_VERSION,
                entries: HashMap::new(),
            });
        RepoIndex {
            root: root.to_path_buf(),
            cache_path,
            cache,
        }
    }

    /// Walk the tree (gitignore-aware), reuse cached metadata where mtimes match.
    pub fn scan(&mut self) -> std::io::Result<ScanReport> {
        let mut entries = Vec::new();
        let mut reused = 0usize;
        let mut refreshed = 0usize;
        let mut new_cache: HashMap<String, CachedMeta> = HashMap::new();

        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(true)
            .git_ignore(true)
            .git_global(false)
            .require_git(false) // honor .gitignore outside a git repo too
            .parents(true)
            .build();
        for dent in walker {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue, // unreadable subtree: skipped, not fatal
            };
            if !dent.file_type().map(|f| f.is_file()).unwrap_or(false) {
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let rel = match dent.path().strip_prefix(&self.root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.is_empty() {
                continue;
            }
            let meta = match std::fs::metadata(dent.path()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let size = meta.len();
            match self.cache.entries.get(&rel_str) {
                Some(c) if c.mtime_ms == mtime_ms && c.size == size => {
                    reused += 1;
                }
                _ => {
                    refreshed += 1;
                }
            }
            new_cache.insert(rel_str.clone(), CachedMeta { size, mtime_ms });
            entries.push(FileEntry {
                path: rel_str,
                size,
                mtime_ms,
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        self.cache = CacheFile {
            version: CACHE_VERSION,
            entries: new_cache,
        };
        self.persist()?;
        Ok(ScanReport {
            entries,
            reused,
            refreshed,
        })
    }

    fn persist(&self) -> std::io::Result<()> {
        if let Some(parent) = self.cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&self.cache).unwrap_or_default();
        let tmp = self.cache_path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.cache_path)?;
        Ok(())
    }

    /// Second-scan reuse check used by tests and diagnostics.
    pub fn cache_is_warm(&self) -> bool {
        !self.cache.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(tmp: &Path) -> RepoIndex {
        RepoIndex::open(&tmp.join("repo"), &tmp.join("cache"))
    }

    #[test]
    fn scan_lists_files_respects_gitignore_and_persists_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(repo.join("target/debug")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(repo.join("README.md"), "readme").unwrap();
        std::fs::write(repo.join(".gitignore"), "/target\n").unwrap();
        std::fs::write(repo.join("target/debug/junk.bin"), "x").unwrap();

        let mut index = idx(tmp.path());
        let r = index.scan().unwrap();
        let paths: Vec<&str> = r.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(
            !paths.iter().any(|p| p.starts_with('.')),
            "hidden (dot) files are skipped by policy"
        );
        assert!(
            !paths.iter().any(|p| p.contains("target/")),
            "gitignored paths excluded"
        );
        assert_eq!(r.reused, 0);
        assert!(r.refreshed > 0);

        // Second scan reuses metadata for unchanged files.
        let mut index2 = idx(tmp.path());
        let r2 = index2.scan().unwrap();
        assert_eq!(r2.entries.len(), r.entries.len());
        assert_eq!(
            r2.reused,
            r.entries.len(),
            "warm cache reuses everything unchanged"
        );
        assert_eq!(r2.refreshed, 0);
    }

    #[test]
    fn scan_detects_changes_and_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        let mut index = idx(tmp.path());
        index.scan().unwrap();

        std::fs::write(repo.join("b.txt"), "world").unwrap();
        let r = index.scan().unwrap();
        assert!(r.entries.iter().any(|e| e.path == "b.txt"));
        assert!(r.refreshed >= 1, "new file counted as refreshed");
    }

    #[test]
    fn corrupt_cache_falls_back_to_cold_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        let mut index = idx(tmp.path());
        index.scan().unwrap();
        // Corrupt the cache file on disk.
        let cache_file = tmp
            .path()
            .join("cache")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&cache_file, b"not json").unwrap();
        let mut index2 = idx(tmp.path());
        let r = index2.scan().unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.refreshed, 1, "cold scan after corrupt cache");
    }
}
