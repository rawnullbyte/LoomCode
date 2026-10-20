//! Skills: user-defined slash commands as plain Markdown files.
//!
//! Two scopes, both read fresh at session start:
//! - global — `<data_dir>/skills/<name>.md`
//! - workspace — `<cwd>/.loon/skills/<name>.md`
//!
//! The file body is the prompt template. If it contains `{{args}}`, the arguments the
//! user typed after the command replace it; otherwise the arguments are appended on
//! their own line. Skills are user-authored prompt shortcuts, not sandboxed programs —
//! they expand to ordinary user turns.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Command name (file stem), as typed after `/`.
    pub name: String,
    pub path: PathBuf,
    /// Which scope it came from; workspace shadows global on the same name.
    pub scope: &'static str,
    pub body: String,
}

/// Load every `.md` skill from both scopes. Workspace skills shadow global ones with
/// the same name. Unreadable files are skipped, never fatal.
pub fn load(data_dir: &Path, cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for (scope, dir) in [
        ("global", data_dir.join("skills")),
        ("workspace", cwd.join(".loon").join("skills")),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                continue; // not a valid command name; ignored, not fatal
            }
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            if body.trim().is_empty() {
                continue;
            }
            out.push(Skill {
                name: name.to_string(),
                path,
                scope,
                body,
            });
        }
    }
    // workspace shadows global
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| {
        if a.name == b.name {
            // keep the later-inserted (workspace) one
            std::mem::swap(&mut a.body, &mut b.body);
            std::mem::swap(&mut a.scope, &mut b.scope);
            std::mem::swap(&mut a.path, &mut b.path);
            true
        } else {
            false
        }
    });
    out
}

/// Expand a skill body into the prompt sent as the user turn.
pub fn expand(body: &str, args: &str) -> String {
    let args = args.trim();
    if body.contains("{{args}}") {
        body.replace("{{args}}", args)
    } else if args.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\nAdditional arguments: {args}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_both_scopes_and_workspace_shadows_global() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let ws = tmp.path().join("proj");
        std::fs::create_dir_all(data.join("skills")).unwrap();
        std::fs::create_dir_all(ws.join(".loon").join("skills")).unwrap();
        std::fs::write(
            data.join("skills").join("review.md"),
            "global review {{args}}",
        )
        .unwrap();
        std::fs::write(data.join("skills").join("explain.md"), "global explain").unwrap();
        std::fs::write(
            ws.join(".loon").join("skills").join("review.md"),
            "workspace review",
        )
        .unwrap();

        let skills = load(&data, &ws);
        assert_eq!(skills.len(), 2, "review deduped to the workspace copy");
        let review = skills.iter().find(|s| s.name == "review").unwrap();
        assert_eq!(review.scope, "workspace");
        assert_eq!(review.body, "workspace review");
        assert!(skills.iter().any(|s| s.name == "explain"));
    }

    #[test]
    fn invalid_names_and_empty_bodies_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let ws = tmp.path().join("proj");
        std::fs::create_dir_all(data.join("skills")).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(data.join("skills").join("bad name!.md"), "x").unwrap();
        std::fs::write(data.join("skills").join("empty.md"), "   \n").unwrap();
        std::fs::write(data.join("skills").join("notes.txt"), "not md").unwrap();
        std::fs::write(data.join("skills").join("ok_name-2.md"), "fine").unwrap();
        let skills = load(&data, &ws);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "ok_name-2");
    }

    #[test]
    fn expand_substitutes_or_appends_arguments() {
        assert_eq!(
            expand("review {{args}} carefully", "src/lib.rs"),
            "review src/lib.rs carefully"
        );
        assert_eq!(
            expand("no placeholder", "extra"),
            "no placeholder\n\nAdditional arguments: extra"
        );
        assert_eq!(expand("no placeholder", ""), "no placeholder");
        assert_eq!(expand("a {{args}} b", "  spaced  "), "a spaced b");
    }
}
