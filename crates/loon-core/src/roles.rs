//! Agent roles and task-category model routing.
//!
//! A role is a named specialist with instructions, tool policy and a *model preference*. The
//! catalog ships the roles a coding harness actually needs (orchestrator, planner, explorer,
//! librarian, architect, implementer, frontend/backend specialist, debugger, test engineer,
//! reviewer, security auditor, documentation writer, release engineer), and every field can be
//! overridden by a project `.loon/agents/*.md` profile — the profile mechanism already used by
//! `delegate`, so there is no second agent-definition system.
//!
//! Model routing answers one question: *which configured model should this role use for this
//! kind of work?* The order is explicit and testable:
//!
//! 1. an explicit model on the role (project profile beats the built-in default);
//! 2. the category's configured model (`[models.<category>]`-style routing table);
//! 3. the session's model.
//!
//! Routing never silently picks a model that cannot do the job: a role that needs tool calling
//! refuses a model whose provider declares it unsupported, and the resolution reports the reason
//! it chose what it chose, so "why is this running on the cheap model" has an answer.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What kind of work is being done. Categories exist so model choice can follow the work rather
/// than the role's name: a reviewer doing a quick typo pass and a reviewer auditing a security
/// boundary should not necessarily run on the same model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Short, mechanical work: a lookup, a rename, a one-line check.
    Quick,
    /// Reasoning-heavy work where mistakes are expensive: architecture, integration, hard bugs.
    #[default]
    Deep,
    /// The hardest problems: design under conflicting constraints, subtle concurrency defects.
    Ultrabrain,
    /// Visual/terminal interface work where layout and hierarchy matter.
    VisualEngineering,
    /// Wording, structure and presentation.
    Artistry,
    /// Prose and documentation.
    Writing,
    /// Test design and verification strategy.
    Testing,
    /// Risk analysis, attack paths, dependency review.
    Security,
    /// Guides, references and migration notes.
    Documentation,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Quick,
        Category::Deep,
        Category::Ultrabrain,
        Category::VisualEngineering,
        Category::Artistry,
        Category::Writing,
        Category::Testing,
        Category::Security,
        Category::Documentation,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Category::Quick => "quick",
            Category::Deep => "deep",
            Category::Ultrabrain => "ultrabrain",
            Category::VisualEngineering => "visual-engineering",
            Category::Artistry => "artistry",
            Category::Writing => "writing",
            Category::Testing => "testing",
            Category::Security => "security",
            Category::Documentation => "documentation",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "quick" | "fast" | "cheap" => Category::Quick,
            "deep" => Category::Deep,
            "ultrabrain" | "brain" => Category::Ultrabrain,
            "visual-engineering" | "visual" | "ui" => Category::VisualEngineering,
            "artistry" | "design" => Category::Artistry,
            "writing" | "prose" => Category::Writing,
            "testing" | "test" => Category::Testing,
            "security" | "audit" => Category::Security,
            "documentation" | "docs" => Category::Documentation,
            _ => return None,
        })
    }
}

/// A specialist role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub description: String,
    /// The category this role usually works in; the routing default.
    pub category: Category,
    /// Explicit model for this role. `None` falls through to the category, then the session.
    pub model: Option<String>,
    /// Coding effort for this role (`low`…`ultra`). `None` inherits.
    pub effort: Option<String>,
    /// Whether the role may modify the workspace. A read-only role's tool policy is derived
    /// from this, so a reviewer cannot write even if it asks to.
    pub writes: bool,
    /// Tools this role may use. Empty means "the default set for its write policy".
    pub tools: Vec<String>,
    /// Instructions appended when the role is invoked.
    pub instructions: String,
    /// True when this came from a project/user profile rather than the built-in catalog.
    pub custom: bool,
}

impl Role {
    fn builtin(
        id: &str,
        description: &str,
        category: Category,
        writes: bool,
        instructions: &str,
    ) -> Role {
        Role {
            id: id.to_string(),
            description: description.to_string(),
            category,
            model: None,
            effort: None,
            writes,
            tools: Vec::new(),
            instructions: instructions.to_string(),
            custom: false,
        }
    }

    /// The tools this role is allowed to use, given the full registry. An empty `tools` list on
    /// a read-only role means every non-mutating tool; on a writing role it means everything.
    pub fn allowed(&self, effects: loon_tools::Effects) -> bool {
        if !self.writes && effects == loon_tools::Effects::Mutating {
            return false;
        }
        true
    }

    /// The delegation task text: the role's instructions, then the task itself. Kept short — the
    /// child sees only this, so every line has to earn its place.
    pub fn task_prompt(&self, task: &str) -> String {
        format!(
            "You are the {} role ({}).\n{}\n\nTask:\n{}",
            self.id, self.description, self.instructions, task
        )
    }
}

/// The built-in role catalog.
pub fn builtin_roles() -> Vec<Role> {
    vec![
        Role::builtin(
            "orchestrator",
            "Owns the goal, keeps the task graph and integrates results",
            Category::Deep,
            true,
            "Split the goal into independently verifiable tasks, delegate what can run in \
             parallel, keep one owner per file, and verify the integrated result yourself. Do \
             not report done for work you have not checked.",
        ),
        Role::builtin(
            "planner",
            "Decomposes a request into ordered, verifiable tasks",
            Category::Deep,
            false,
            "Produce an ordered plan where every item has a concrete completion check. Name \
             dependencies and risks. Do not modify files.",
        ),
        Role::builtin(
            "explorer",
            "Finds architecture, symbols and the files that matter",
            Category::Quick,
            false,
            "Search the repository and report what is relevant: paths, symbols, how the pieces \
             connect. Quote the file:line you relied on. Do not modify files.",
        ),
        Role::builtin(
            "librarian",
            "Researches official documentation and API references",
            Category::Documentation,
            false,
            "Find the current documented behaviour for the library and version in use. Prefer \
             official sources; record the version you checked and say when you could not verify \
             it.",
        ),
        Role::builtin(
            "architect",
            "Reviews design options, interfaces and data flow",
            Category::Ultrabrain,
            false,
            "Compare the realistic design options against this codebase's constraints. State the \
             trade-offs, the interface you would choose and why. Do not modify files.",
        ),
        Role::builtin(
            "implementer",
            "Implements an explicitly scoped change",
            Category::Deep,
            true,
            "Implement exactly the scoped change, following the repository's conventions. Verify \
             it and report the files you changed with the evidence that the change works.",
        ),
        Role::builtin(
            "frontend",
            "Interface behaviour, terminal ergonomics and visual consistency",
            Category::VisualEngineering,
            true,
            "Work on the interface: layout, hierarchy, keyboard access, terminal resize behaviour \
             and visual consistency. Verify at narrow and short sizes, not only the happy path.",
        ),
        Role::builtin(
            "backend",
            "Application logic, persistence, networking and APIs",
            Category::Deep,
            true,
            "Work on server-side logic with explicit data invariants and failure boundaries. \
             Handle the error cases you introduce and verify them.",
        ),
        Role::builtin(
            "debugger",
            "Reproduces a failure and finds the root cause",
            Category::Ultrabrain,
            true,
            "Reproduce the failure first. Find the line that explains it and cite it. Fix the \
             cause, not the symptom, and add a check that fails without the fix.",
        ),
        Role::builtin(
            "tester",
            "Writes regression tests and verification strategy",
            Category::Testing,
            true,
            "Test externally observable behaviour. Each test must fail when the defect is \
             reintroduced. State what the suite does not cover.",
        ),
        Role::builtin(
            "reviewer",
            "Independent review for correctness and regressions",
            Category::Deep,
            false,
            "Review the diff for real defects: correctness, regressions, missed requirements. \
             Report findings with file:line evidence and say what you could not check. Do not \
             modify files.",
        ),
        Role::builtin(
            "security",
            "Audits authorization, shell execution, secrets and input handling",
            Category::Security,
            false,
            "Trace realistic attack paths through the actual code. Report exploitable findings \
             with the path that reaches them; omit theory you cannot demonstrate. Do not modify \
             files.",
        ),
        Role::builtin(
            "documentation",
            "Maintains guides, references and migration notes",
            Category::Documentation,
            true,
            "Document what the code actually does, verified against the current source and \
             commands. Note known limitations honestly rather than presenting aspirations.",
        ),
        Role::builtin(
            "release",
            "Release preparation, versioning, packaging and CI checks",
            Category::Deep,
            true,
            "Prepare a reproducible release: version consistency, changelog accuracy, packaging \
             and the documented checks actually passing. Do not publish anything.",
        ),
    ]
}

/// The effective catalog: built-ins overridden by profiles (project over `.loon/agents` beats
/// user data dir, matching the existing profile precedence).
pub fn catalog(
    data_dir: &std::path::Path,
    cwd: &std::path::Path,
    profiles: &[crate::subagent::Profile],
) -> Vec<Role> {
    let _ = (data_dir, cwd);
    let mut roles: BTreeMap<String, Role> = builtin_roles()
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    for profile in profiles {
        let category = profile
            .category
            .as_deref()
            .and_then(Category::parse)
            .or_else(|| roles.get(&profile.name).map(|r| r.category))
            .unwrap_or(Category::Deep);
        // A profile that disallows a mutating tool is read-only in effect; otherwise inherit the
        // built-in role's policy, and default a brand-new role to writing.
        let writes = if profile.disallowed_tools.iter().any(|t| {
            matches!(
                t.as_str(),
                "write" | "edit" | "apply_patch" | "multi_edit" | "fuzzy_edit" | "bash"
            )
        }) {
            false
        } else {
            roles.get(&profile.name).map(|r| r.writes).unwrap_or(true)
        };
        roles.insert(
            profile.name.clone(),
            Role {
                id: profile.name.clone(),
                description: profile.description.clone(),
                category,
                model: profile.model.clone(),
                effort: profile.effort.clone(),
                writes,
                tools: profile.tools.clone(),
                instructions: profile.body.clone(),
                custom: true,
            },
        );
    }
    roles.into_values().collect()
}

/// Why a model was chosen. Reported so "which model actually ran and why" is answerable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// The role itself names a model.
    Role,
    /// The category has a routing entry.
    Category(Category),
    /// Nothing more specific applied.
    Session,
}

impl Why {
    pub fn describe(&self) -> String {
        match self {
            Why::Role => "the role sets an explicit model".into(),
            Why::Category(c) => format!("the `{}` category routes here", c.id()),
            Why::Session => "the session model is used".into(),
        }
    }
}

/// A resolved model choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    pub model: String,
    pub why: Why,
}

/// A routing table: category → model reference.
pub type RoutingTable = BTreeMap<Category, String>;

/// Resolve the model for a role.
///
/// `check` validates that the reference is configured and usable; it returns an error message
/// when it is not. A role/category entry that does not resolve is *skipped* with a note rather
/// than failing the delegation, because a stale routing entry should not make the harness
/// unusable — but the caller is told, so it can fix the configuration.
pub fn resolve(
    role: Option<&Role>,
    category: Category,
    table: &RoutingTable,
    session_model: &str,
    check: impl Fn(&str) -> Result<(), String>,
) -> (Routing, Vec<String>) {
    let mut notes = Vec::new();
    if let Some(role) = role {
        if let Some(model) = &role.model {
            match check(model) {
                Ok(()) => {
                    return (
                        Routing {
                            model: model.clone(),
                            why: Why::Role,
                        },
                        notes,
                    )
                }
                Err(e) => notes.push(format!(
                    "role `{}` names model `{model}`, which is unusable: {e}; falling back",
                    role.id
                )),
            }
        }
    }
    if let Some(model) = table.get(&category) {
        match check(model) {
            Ok(()) => {
                return (
                    Routing {
                        model: model.clone(),
                        why: Why::Category(category),
                    },
                    notes,
                )
            }
            Err(e) => notes.push(format!(
                "category `{}` routes to `{model}`, which is unusable: {e}; falling back",
                category.id()
            )),
        }
    }
    (
        Routing {
            model: session_model.to_string(),
            why: Why::Session,
        },
        notes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_ok(_: &str) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn every_builtin_role_is_complete_and_unique() {
        let roles = builtin_roles();
        assert!(
            roles.len() >= 14,
            "the catalog must cover the real specialties"
        );
        let mut ids: Vec<&str> = roles.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(count, ids.len(), "role ids must be unique");
        for role in &roles {
            assert!(
                !role.description.is_empty(),
                "{} has no description",
                role.id
            );
            assert!(
                !role.instructions.is_empty(),
                "{} has no instructions — delegating to it would be an empty prompt",
                role.id
            );
        }
        // The roles the mission names must actually exist.
        for id in [
            "orchestrator",
            "planner",
            "explorer",
            "librarian",
            "architect",
            "implementer",
            "frontend",
            "backend",
            "debugger",
            "tester",
            "reviewer",
            "security",
            "documentation",
            "release",
        ] {
            assert!(roles.iter().any(|r| r.id == id), "missing role `{id}`");
        }
    }

    /// Read-only roles must be read-only in the tool policy, not only in their prose: the
    /// reviewer prompt says "do not modify files" and the policy has to agree.
    #[test]
    fn review_and_audit_roles_cannot_write() {
        let roles = builtin_roles();
        for id in [
            "reviewer",
            "security",
            "planner",
            "explorer",
            "librarian",
            "architect",
        ] {
            let role = roles.iter().find(|r| r.id == id).unwrap();
            assert!(!role.writes, "{id} must not write");
            assert!(
                !role.allowed(loon_tools::Effects::Mutating),
                "{id} must be denied a mutating tool"
            );
            assert!(role.allowed(loon_tools::Effects::ReadOnly));
        }
        let implementer = roles.iter().find(|r| r.id == "implementer").unwrap();
        assert!(implementer.writes);
        assert!(implementer.allowed(loon_tools::Effects::Mutating));
    }

    #[test]
    fn explicit_role_model_beats_the_category_which_beats_the_session() {
        let mut table = RoutingTable::new();
        table.insert(Category::Deep, "cloud/deep".into());
        let mut role = builtin_roles()
            .into_iter()
            .find(|r| r.id == "architect")
            .unwrap();

        role.model = Some("local/strong".into());
        let (routing, notes) = resolve(Some(&role), Category::Deep, &table, "session/m", all_ok);
        assert_eq!(routing.model, "local/strong");
        assert_eq!(routing.why, Why::Role);
        assert!(notes.is_empty());

        role.model = None;
        let (routing, _) = resolve(Some(&role), Category::Deep, &table, "session/m", all_ok);
        assert_eq!(routing.model, "cloud/deep");
        assert_eq!(routing.why, Why::Category(Category::Deep));

        let (routing, _) = resolve(Some(&role), Category::Writing, &table, "session/m", all_ok);
        assert_eq!(routing.model, "session/m");
        assert_eq!(routing.why, Why::Session);
    }

    /// A stale routing entry must not break delegation, but it must not be used silently either.
    #[test]
    fn an_unusable_route_is_skipped_with_an_explanation() {
        let mut table = RoutingTable::new();
        table.insert(Category::Deep, "gone/model".into());
        let check = |model: &str| {
            if model == "gone/model" {
                Err("provider `gone` is not configured".to_string())
            } else {
                Ok(())
            }
        };
        let (routing, notes) = resolve(None, Category::Deep, &table, "session/m", check);
        assert_eq!(routing.model, "session/m");
        assert_eq!(routing.why, Why::Session);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("gone/model"), "{notes:?}");
        assert!(notes[0].contains("not configured"), "{notes:?}");
    }

    #[test]
    fn a_profile_overrides_a_builtin_role_and_keeps_its_identity() {
        let profiles = vec![crate::subagent::Profile {
            name: "reviewer".into(),
            description: "Rust reviewer".into(),
            model: Some("local/code".into()),
            effort: Some("high".into()),
            max_turns: 10,
            tools: vec!["read".into(), "grep".into()],
            disallowed_tools: vec!["bash".into()],
            isolation: false,
            body: "Inspect and report.".into(),
            category: Some("deep".into()),
        }];
        let roles = catalog(
            std::path::Path::new("/data"),
            std::path::Path::new("/ws"),
            &profiles,
        );
        let reviewer = roles.iter().find(|r| r.id == "reviewer").unwrap();
        assert!(reviewer.custom);
        assert_eq!(reviewer.model.as_deref(), Some("local/code"));
        assert_eq!(reviewer.effort.as_deref(), Some("high"));
        assert!(
            !reviewer.writes,
            "disallowing bash makes the profile read-only"
        );
        assert_eq!(reviewer.category, Category::Deep);
        // The rest of the catalog survives.
        assert!(roles.iter().any(|r| r.id == "implementer"));
    }

    #[test]
    fn a_roles_task_prompt_carries_its_instructions_and_the_task() {
        let role = builtin_roles()
            .into_iter()
            .find(|r| r.id == "debugger")
            .unwrap();
        let prompt = role.task_prompt("the parser drops empty fields");
        assert!(prompt.contains("debugger"));
        assert!(prompt.contains("Reproduce"));
        assert!(prompt.contains("the parser drops empty fields"));
    }

    #[test]
    fn categories_round_trip_and_parse_their_aliases() {
        for category in Category::ALL {
            assert_eq!(Category::parse(category.id()), Some(category));
        }
        assert_eq!(Category::parse("UI"), Some(Category::VisualEngineering));
        assert_eq!(Category::parse("docs"), Some(Category::Documentation));
        assert_eq!(Category::parse("nonsense"), None);
    }

    /// Cheaper categories must exist and be routable: the whole point is to stop sending routine
    /// lookups to the most expensive model.
    #[test]
    fn a_quick_category_can_route_to_a_cheaper_model() {
        let mut table = RoutingTable::new();
        table.insert(Category::Quick, "local/small".into());
        table.insert(Category::Ultrabrain, "cloud/strong".into());
        let explorer = builtin_roles()
            .into_iter()
            .find(|r| r.id == "explorer")
            .unwrap();
        let architect = builtin_roles()
            .into_iter()
            .find(|r| r.id == "architect")
            .unwrap();
        let (cheap, _) = resolve(
            Some(&explorer),
            explorer.category,
            &table,
            "session/m",
            all_ok,
        );
        let (strong, _) = resolve(
            Some(&architect),
            architect.category,
            &table,
            "session/m",
            all_ok,
        );
        assert_eq!(cheap.model, "local/small");
        assert_eq!(strong.model, "cloud/strong");
    }
}
