//! Explicit execution modes.
//!
//! A mode is not a label: each one *derives* the runtime switches it implies, so selecting
//! `plan` really restricts the toolset and selecting `ultrawork` really turns on the
//! independent completion supervisor. [`Mode::transition`] is the single place that mapping
//! lives, which is why `/mode`, the `ultrawork`/`ulw` keywords and the mode indicator can
//! never disagree with what the agent actually does.
//!
//! Modes can only ever *restrict*: a user's explicit deny rules and the selected permission
//! mode always win, and no mode can widen permissions. `bypass` is not reachable from here at
//! all — it is a permission setting, chosen deliberately, and never implied by a keyword.

use serde::{Deserialize, Serialize};
use std::fmt;

/// What the user asked the session to do, at the level of execution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Direct, proportionate execution. The default.
    #[default]
    Normal,
    /// Inspect and produce a written plan; no workspace mutations.
    Plan,
    /// Execute an approved plan, tracking every item.
    Execute,
    /// Autonomous coordinated execution with independent completion review.
    Ultrawork,
    /// Parallel collaborative execution across specialists.
    Team,
    /// Documentation research and specialist review before implementation.
    Research,
    /// Inspect changes without modifying anything.
    Review,
    /// Reproduce and trace before changing code.
    Debug,
    /// Inspect risks and report actionable findings.
    Security,
    /// The highest configured coding effort for a hard task.
    UltraCode,
    /// Retry incomplete work using new evidence rather than repeating a failed action.
    Repair,
}

impl Mode {
    /// Every mode, in the order `/mode` lists them.
    pub const ALL: [Mode; 11] = [
        Mode::Normal,
        Mode::Plan,
        Mode::Execute,
        Mode::Ultrawork,
        Mode::Team,
        Mode::Research,
        Mode::Review,
        Mode::Debug,
        Mode::Security,
        Mode::UltraCode,
        Mode::Repair,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Plan => "plan",
            Mode::Execute => "execute",
            Mode::Ultrawork => "ultrawork",
            Mode::Team => "team",
            Mode::Research => "research",
            Mode::Review => "review",
            Mode::Debug => "debug",
            Mode::Security => "security",
            Mode::UltraCode => "ultracode",
            Mode::Repair => "repair",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "normal" | "off" | "default" => Mode::Normal,
            "plan" | "planning" => Mode::Plan,
            "execute" | "exec" | "approved-plan" => Mode::Execute,
            "ultrawork" | "ulw" => Mode::Ultrawork,
            "team" => Mode::Team,
            "research" | "deep-research" | "deepresearch" => Mode::Research,
            "review" => Mode::Review,
            "debug" => Mode::Debug,
            "security" | "security-audit" | "audit" => Mode::Security,
            "ultracode" | "uc" => Mode::UltraCode,
            "repair" | "repair-loop" => Mode::Repair,
            // Short aliases users type out of habit.
            "up" => Mode::Ultrawork,
            _ => return None,
        })
    }

    /// One-line explanation for the picker.
    pub fn description(self) -> &'static str {
        match self {
            Mode::Normal => "Direct, proportionate execution",
            Mode::Plan => "Inspect and write a plan; no workspace changes until you approve it",
            Mode::Execute => "Execute the approved plan, tracking every item",
            Mode::Ultrawork => {
                "Autonomous coordinated execution with independent completion review"
            }
            Mode::Team => "Parallel collaborative execution across specialist agents",
            Mode::Research => "Research documentation and specialist review before implementing",
            Mode::Review => "Inspect changes and report findings without modifying anything",
            Mode::Debug => "Reproduce and trace the problem before changing code",
            Mode::Security => "Inspect risks and report actionable findings",
            Mode::UltraCode => "Maximum configured coding effort for a difficult task",
            Mode::Repair => "Retry incomplete work using new evidence, not a repeated attempt",
        }
    }

    /// The runtime switches this mode implies.
    pub fn effects(self) -> Effects {
        match self {
            Mode::Normal => Effects::default(),
            Mode::Plan => Effects {
                plan_mode: true,
                ..Effects::default()
            },
            Mode::Execute => Effects {
                track_plan: true,
                ..Effects::default()
            },
            Mode::Ultrawork => Effects {
                ultraloop: true,
                track_plan: true,
                ..Effects::default()
            },
            Mode::Team => Effects {
                team: true,
                track_plan: true,
                ..Effects::default()
            },
            Mode::Research => Effects {
                research: true,
                ..Effects::default()
            },
            Mode::Review => Effects {
                read_only: true,
                ..Effects::default()
            },
            Mode::Debug => Effects {
                debug: true,
                ..Effects::default()
            },
            Mode::Security => Effects {
                read_only: true,
                security: true,
                ..Effects::default()
            },
            Mode::UltraCode => Effects {
                ultraloop: true,
                max_effort: true,
                track_plan: true,
                ..Effects::default()
            },
            Mode::Repair => Effects {
                repair: true,
                track_plan: true,
                ..Effects::default()
            },
        }
    }

    /// Guidance appended to the system prompt while this mode is active. Short by design: the
    /// per-mode skills carry the detail, and a long block would cost every request.
    pub fn guidance(self) -> Option<&'static str> {
        match self {
            Mode::Normal => None,
            Mode::Plan => Some(
                "The session is in plan mode. Inspect, read and research freely; do not modify \
                 workspace files or run mutating commands. Produce an ordered, verifiable plan \
                 and stop for approval.",
            ),
            Mode::Execute => Some(
                "The plan is approved. Execute it item by item, marking progress as each item \
                 is genuinely finished, and run the verification each item names.",
            ),
            Mode::Ultrawork => Some(
                "Work autonomously and coordinate subagents where the work splits cleanly. An \
                 independent supervisor reviews completion: do not report done until the goal \
                 is verified, and act on its next steps rather than restating the plan.",
            ),
            Mode::Team => Some(
                "Split independent work across specialists, give each only the context it \
                 needs, keep one owner per file, and integrate the results yourself before \
                 reporting. Sequence work that touches the same file.",
            ),
            Mode::Research => Some(
                "Ground the work in current documentation before writing code: resolve the \
                 library and version, read the relevant reference, and record what is \
                 established versus assumed. Then implement against what you verified.",
            ),
            Mode::Review => Some(
                "Review only: do not modify files. Report concrete defects with file:line \
                 evidence, distinguish verified findings from suspicions, and say what you \
                 could not check.",
            ),
            Mode::Debug => Some(
                "Reproduce the defect before changing anything, find the line that explains it, \
                 state the cause with evidence, then fix it and add a regression check that \
                 fails without the fix.",
            ),
            Mode::Security => Some(
                "Audit only: do not modify files. Trace realistic attack paths through the \
                 actual code, report exploitable findings with the path that reaches them, and \
                 omit theoretical issues you cannot show are reachable.",
            ),
            Mode::UltraCode => Some(
                "This is a hard task: investigate root causes, keep a durable plan, implement \
                 thoroughly, review your own diff and validate every affected workflow before \
                 claiming completion.",
            ),
            Mode::Repair => Some(
                "The previous attempt did not satisfy the requirement. Read the failing \
                 evidence, state what was actually wrong, and change your approach — repeating \
                 the same call will fail again.",
            ),
        }
    }

    /// Parse a mode from a *leading keyword* in a prompt.
    ///
    /// Deliberately narrower than [`Mode::parse`]: only tokens that cannot be ordinary English
    /// words route implicitly. `review the parser` is a request to review something, not a
    /// request to enter review mode, so `review`, `plan`, `debug`, `team`, `research`,
    /// `security`, `execute` and `repair` are reachable only through `/mode` — where the user
    /// has explicitly said "the next word is a mode". Keywords such as `ultrawork`/`ulw` have no
    /// other meaning, so they route.
    pub fn from_keyword(word: &str) -> Option<Mode> {
        Some(match word.trim().to_ascii_lowercase().as_str() {
            "ultrawork" | "ulw" => Mode::Ultrawork,
            "ultracode" => Mode::UltraCode,
            "deepresearch" => Mode::Research,
            _ => return None,
        })
    }

    /// Whether the mode implies read-only workspace access. Permission rules are still applied
    /// on top; this can restrict, never widen.
    pub fn read_only(self) -> bool {
        // Plan mode is enforced by the same existing check that hides and rejects mutating
        // tools, so it is read-only in effect even though `read_only` records the sharper
        // review/audit cases used for messaging.
        self.effects().read_only || self.effects().plan_mode
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The runtime switches a mode derives. `Default` is `normal`: every flag false.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effects {
    /// No workspace mutations (plan/review/security).
    pub plan_mode: bool,
    /// An independent supervisor reviews completion.
    pub ultraloop: bool,
    /// Keep a durable plan and require every item to be closed before completion.
    pub track_plan: bool,
    /// Parallel specialist workers.
    pub team: bool,
    /// Documentation research is part of the work.
    pub research: bool,
    /// Read-only inspection only.
    pub read_only: bool,
    /// Reproduce before changing.
    pub debug: bool,
    /// Security focus.
    pub security: bool,
    /// Raise effort to the highest configured level.
    pub max_effort: bool,
    /// Continue an unfinished attempt with new evidence.
    pub repair: bool,
}

/// Recognise a mode keyword at the start of a prompt.
///
/// Both `ultrawork`/`ulw` style keywords (OMO compatibility) and this application's own
/// `ultracode` are accepted. The keyword is only taken when it leads the prompt and is
/// followed by whitespace or a separator, so a sentence that happens to start with "debug"
/// as an ordinary word — "debugging the parser" — is not hijacked. Returns the mode and the
/// remaining text.
pub fn route(prompt: &str) -> Option<(Mode, String)> {
    let trimmed = prompt.trim_start();
    let (word, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest),
        None => (trimmed, ""),
    };
    // A trailing separator is common when the user writes `ulw: fix the tests`.
    let word = word.trim_end_matches([':', ',']);
    // Only unambiguous keywords route: see [`Mode::from_keyword`]. A bare keyword with no task
    // after it is not a request either.
    let mode = Mode::from_keyword(word)?;
    if rest.trim().is_empty() {
        return None;
    }
    Some((mode, rest.trim().to_string()))
}

/// The session's mode plus the durable flags it last applied, so a mode survives `/resume` and
/// a manual `/plan off` does not silently disagree with the indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModeState {
    #[serde(default)]
    pub mode: Mode,
    /// Effective plan-mode flag after applying the mode (and after any manual override).
    #[serde(default)]
    pub plan_mode: bool,
    #[serde(default)]
    pub read_only: bool,
}

/// What to write when a mode is selected. Computed without touching the agent so it is
/// testable and the caller applies it in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub mode: Mode,
    pub plan_mode: bool,
    pub read_only: bool,
    pub ultraloop: Option<bool>,
    pub effort: Option<&'static str>,
}

impl Mode {
    /// Compute the transition for this mode given the session's current ultraloop flag.
    ///
    /// `ultraloop: None` leaves the existing supervisor setting alone. Modes that imply the
    /// supervisor turn it on; returning to `Normal` does **not** turn it off, because the user
    /// may have enabled it explicitly with `/ultraloop on` and a mode change should not undo
    /// a deliberate setting.
    pub fn transition(self, ultraloop_now: bool) -> Transition {
        let effects = self.effects();
        let ultraloop = if effects.ultraloop {
            (!ultraloop_now).then_some(true)
        } else {
            None
        };
        Transition {
            mode: self,
            plan_mode: effects.plan_mode || effects.read_only,
            read_only: effects.read_only,
            ultraloop,
            effort: effects.max_effort.then_some("ultra"),
        }
    }

    /// Read-only modes refuse file mutation regardless of permission mode. Kept separate from
    /// `plan_mode` so the rejection message can name the actual reason.
    pub fn blocks_mutation(self) -> bool {
        self.effects().plan_mode || self.effects().read_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_round_trips_through_its_id_and_has_a_description() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.id()), Some(mode), "{mode:?}");
            assert!(!mode.description().is_empty(), "{mode:?}");
        }
        let mut ids: Vec<&str> = Mode::ALL.iter().map(|m| m.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(count, ids.len(), "mode ids must be unique");
    }

    /// Choosing `plan` must *be* plan mode, not merely say so: the flag plan-mode enforcement
    /// already reads (`WorkState::plan_mode`) is what the transition sets.
    #[test]
    fn plan_and_review_modes_really_block_mutation() {
        for mode in [Mode::Plan, Mode::Review, Mode::Security] {
            let t = mode.transition(false);
            assert!(t.plan_mode, "{mode:?} must set plan mode");
            assert!(mode.blocks_mutation(), "{mode:?} must block mutation");
            assert!(mode.read_only(), "{mode:?} is read-only");
        }
        assert!(!Mode::Normal.transition(false).plan_mode);
        assert!(!Mode::Execute.blocks_mutation());
        assert!(!Mode::Debug.blocks_mutation());
    }

    #[test]
    fn ultrawork_turns_the_supervisor_on_and_ultracode_raises_effort() {
        let t = Mode::Ultrawork.transition(false);
        assert_eq!(t.ultraloop, Some(true));
        assert!(!t.plan_mode, "ultrawork must be able to write");

        let t = Mode::UltraCode.transition(false);
        assert_eq!(t.ultraloop, Some(true));
        assert_eq!(t.effort, Some("ultra"));

        // Already on: no redundant write, so a mode change cannot restart a review loop.
        assert_eq!(Mode::Ultrawork.transition(true).ultraloop, None);
    }

    /// A deliberate `/ultraloop on` must survive a switch back to normal mode.
    #[test]
    fn leaving_ultrawork_does_not_silently_disable_the_supervisor() {
        assert_eq!(Mode::Normal.transition(true).ultraloop, None);
        assert_eq!(Mode::Normal.transition(false).ultraloop, None);
    }

    #[test]
    fn keywords_route_at_the_start_of_a_prompt_and_strip_themselves() {
        assert_eq!(
            route("ultrawork fix the parser"),
            Some((Mode::Ultrawork, "fix the parser".into()))
        );
        assert_eq!(
            route("ulw: add a regression test"),
            Some((Mode::Ultrawork, "add a regression test".into()))
        );
        assert_eq!(
            route("  ultracode untangle the scheduler"),
            Some((Mode::UltraCode, "untangle the scheduler".into()))
        );
        assert_eq!(
            route("deepresearch pick the right version"),
            Some((Mode::Research, "pick the right version".into()))
        );
    }

    /// Routing is intentionally narrower than `/mode`: an ordinary English word in front of a
    /// sentence must not switch the session's strategy. `review the parser` is a task, not a
    /// mode change; `/mode review` is how the user asks for the mode.
    #[test]
    fn ordinary_english_words_never_route_even_as_the_first_word() {
        for prompt in [
            "review the parser",
            "plan the migration",
            "debug the flaky test",
            "team meeting notes are in docs",
            "research shows three approaches",
            "security headers are missing",
            "execute the script",
            "repair the fence",
        ] {
            assert_eq!(route(prompt), None, "`{prompt}` must not change the mode");
            let first = prompt.split_whitespace().next().unwrap();
            assert!(
                Mode::parse(first).is_some(),
                "`{first}` is still a valid /mode name"
            );
        }
    }

    /// Ordinary prose must not be hijacked into a mode: a keyword needs a following prompt, is
    /// only matched at the start, and `normal` is never routed implicitly.
    #[test]
    fn ordinary_words_are_not_treated_as_modes() {
        assert_eq!(route("ultrawork"), None, "a bare keyword is not a task");
        assert_eq!(
            route("please ultrawork this later"),
            None,
            "a keyword mid-sentence must not route"
        );
        assert_eq!(route("ultraworking the parser"), None, "only whole words");
        assert_eq!(route("ultraworkteam matters"), None, "no prefix matching");
        assert_eq!(route(""), None);
    }

    #[test]
    fn the_prompt_guidance_names_the_real_constraint() {
        assert!(Mode::Plan
            .guidance()
            .is_some_and(|g| g.contains("do not modify")));
        assert!(Mode::Review
            .guidance()
            .is_some_and(|g| g.contains("do not modify")));
        assert!(Mode::Security
            .guidance()
            .is_some_and(|g| g.contains("do not modify")));
        assert!(Mode::Debug
            .guidance()
            .is_some_and(|g| g.contains("Reproduce")));
        assert_eq!(Mode::Normal.guidance(), None, "normal adds no instructions");
    }

    #[test]
    fn debug_and_research_keep_write_access() {
        assert!(!Mode::Debug.read_only());
        assert!(!Mode::Research.read_only());
        assert!(!Mode::Team.read_only());
    }
}
