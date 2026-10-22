//! Bounded, fresh instruction discovery. Nested guidance is returned with path-based
//! tool results; general workspace rules are re-injected after compaction.
use std::path::{Path, PathBuf};

/// Small Markdown frontmatter reader for scalar/list fields. It deliberately does
/// not execute substitutions, YAML tags, or shell fragments.
pub fn frontmatter(text: &str) -> (std::collections::BTreeMap<String, String>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (Default::default(), text);
    };
    let Some(end) = rest.find("\n---") else {
        return (Default::default(), text);
    };
    let mut fields = std::collections::BTreeMap::new();
    let mut key = String::new();
    for line in rest[..end].lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if !key.is_empty() {
                let entry = fields.entry(key.clone()).or_insert_with(String::new);
                entry.push(' ');
                entry.push_str(line.trim().trim_start_matches("- "));
            }
        } else if let Some((k, v)) = line.split_once(':') {
            key = k.trim().to_string();
            fields.insert(key.clone(), v.trim().trim_matches(['\'', '"']).to_string());
        }
    }
    (fields, rest[end + 4..].trim_start_matches(['\r', '\n']))
}
pub fn list(value: &str) -> Vec<String> {
    value
        .trim_matches(['[', ']'])
        .split(',')
        .map(|s| s.trim().trim_matches(['\'', '"']).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
fn append(out: &mut String, path: &Path) {
    if out.chars().count() >= 24_000 {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(path) {
        let text = text.chars().take(8_000).collect::<String>();
        if !text.trim().is_empty() {
            out.push_str(&format!("\nGuidance from {}:\n{text}\n", path.display()));
        }
    }
}
fn rules(root: &Path, target: Option<&Path>) -> String {
    let dir = root.join(".loon/rules");
    let mut paths: Vec<PathBuf> = ignore::WalkBuilder::new(&dir)
        .max_depth(Some(6))
        .hidden(false)
        .build()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && e.path().extension().is_some_and(|x| x == "md")
        })
        .map(|e| e.into_path())
        .take(200)
        .collect();
    paths.sort();
    let mut out = String::new();
    for path in paths {
        if out.chars().count() >= 24_000 {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (fields, body) = frontmatter(&text);
        let patterns = fields.get("paths").map(|s| list(s)).unwrap_or_default();
        let applies = if patterns.is_empty() {
            target.is_none()
        } else {
            target
                .and_then(|p| p.strip_prefix(root).ok())
                .is_some_and(|rel| {
                    patterns.iter().any(|pattern| {
                        glob::Pattern::new(pattern).is_ok_and(|p| p.matches_path(rel))
                    })
                })
        };
        if applies {
            out.push_str(&format!(
                "\nRule {}:\n{}\n",
                path.display(),
                body.chars().take(8_000).collect::<String>()
            ));
        }
    }
    out
}
pub fn workspace(root: &Path) -> String {
    let mut out = String::new();
    append(&mut out, &root.join("CLAUDE.md"));
    out.push_str(&rules(root, None));
    out
}
pub fn for_tool(root: &Path, args: &serde_json::Value) -> String {
    let Some(path) = args["path"].as_str() else {
        return String::new();
    };
    let path = root.join(path);
    let Ok(root) = root.canonicalize() else {
        return String::new();
    };
    // A new file has no canonical path yet; resolve its existing parent.
    let resolved = path.canonicalize().ok().or_else(|| {
        path.parent()?
            .canonicalize()
            .ok()
            .map(|p| p.join(path.file_name().unwrap_or_default()))
    });
    let Some(path) = resolved.filter(|p| p.starts_with(&root)) else {
        return String::new();
    };
    let dir = if path.is_dir() {
        path.clone()
    } else {
        path.parent().unwrap_or(&root).to_path_buf()
    };
    let mut dirs: Vec<_> = dir
        .ancestors()
        .take_while(|p| p.starts_with(&root) && *p != root)
        .collect();
    dirs.reverse();
    let mut out = String::new();
    for dir in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            append(&mut out, &dir.join(name));
        }
    }
    out.push_str(&rules(&root, Some(&path)));
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_rules_and_nested_guidance_match_only_target() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".loon/rules")).unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/AGENTS.md"), "rust conventions").unwrap();
        std::fs::write(
            d.path().join(".loon/rules/rust.md"),
            "---\npaths: [src/**/*.rs]\n---\nreview unsafe",
        )
        .unwrap();
        std::fs::write(d.path().join("src/a.rs"), "").unwrap();
        let text = for_tool(d.path(), &serde_json::json!({"path":"src/a.rs"}));
        assert!(text.contains("rust conventions"));
        assert!(text.contains("review unsafe"));
        assert!(!workspace(d.path()).contains("review unsafe"));
        assert!(for_tool(d.path(), &serde_json::json!({"path":"../outside.rs"})).is_empty());
    }
}
