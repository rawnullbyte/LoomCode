//! Rewind machinery: undo/redo of mutating tool transactions, conversation/workspace
//! rewind to a request boundary, forks and named checkpoints.
//!
//! Preservation rules (target spec §6.3):
//! - restore verifies the file's *current* content hash before overwriting; a mismatch is
//!   a conflict — the file is left untouched and reported, never silently overwritten;
//! - restores are per-file atomic (temp + rename) and journaled;
//! - conversation rewind only rewrites in-memory history plus a `Rewound` journal event;
//!   a resume replay honors `truncate_to_seq`, so the two agree;
//! - git index, commits, and anything outside the workspace are never touched.

use crate::LoonPaths;
use loon_events::model::{BlobRef, EvId, Event, EventKind};
use loon_events::Journal;
use loon_workspace::{sha256_hex, Workspace};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindMode {
    ConversationOnly,
    WorkspaceOnly,
    ConversationAndWorkspace,
}

impl RewindMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RewindMode::ConversationOnly => "conversation",
            RewindMode::WorkspaceOnly => "workspace",
            RewindMode::ConversationAndWorkspace => "conversation+workspace",
        }
    }
    pub fn parse(s: &str) -> Option<RewindMode> {
        match s {
            "conversation" => Some(RewindMode::ConversationOnly),
            "workspace" => Some(RewindMode::WorkspaceOnly),
            "both" | "conversation+workspace" => Some(RewindMode::ConversationAndWorkspace),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub files_restored: usize,
    pub conflicts: Vec<String>,
}

impl std::fmt::Display for RestoreReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} file(s) restored", self.files_restored)?;
        if !self.conflicts.is_empty() {
            write!(
                f,
                ", {} conflict(s) left untouched: {}",
                self.conflicts.len(),
                self.conflicts.join("; ")
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error("no request matched the rewind target")]
    NoTarget,
    #[error(transparent)]
    Events(#[from] loon_events::EventsError),
}

/// Mutation ledger entry parsed from the journal.
#[derive(Debug, Clone)]
struct Mutation {
    tx: EvId,
    seq: u64,
    path: String,
    #[cfg_attr(not(feature = "rewind_debug"), allow(dead_code))]
    tool: String,
    before_sha: String,
    after_sha: String,
    before_blob: Option<String>,
    after_blob: Option<String>,
}

fn mutations(events: &[Event]) -> Vec<Mutation> {
    let mut out = Vec::new();
    for e in crate::replay::active_events(events, None) {
        if let EventKind::FileMutated {
            tx,
            path,
            tool,
            before_sha256,
            after_sha256,
            before_blob,
            after_blob,
            ..
        } = &e.kind
        {
            out.push(Mutation {
                tx: *tx,
                seq: e.seq,
                path: path.clone(),
                tool: tool.clone(),
                before_sha: before_sha256.clone(),
                after_sha: after_sha256.clone(),
                before_blob: before_blob.as_ref().map(|b| b.sha256.clone()),
                after_blob: after_blob.as_ref().map(|b| b.sha256.clone()),
            });
        }
    }
    out
}

/// Which transactions have been undone/redone, in event order.
fn undo_redo_marks(events: &[Event]) -> Vec<(EvId, bool /*is_undo*/)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::MutationUndone { tx, .. } => Some((*tx, true)),
            EventKind::MutationRedone { tx, .. } => Some((*tx, false)),
            _ => None,
        })
        .collect()
}

/// Undo the newest not-currently-undone mutating transaction.
pub fn undo(
    journal: &Journal,
    ws: &Workspace,
    events: &[Event],
) -> Result<RestoreReport, RewindError> {
    let muts = mutations(events);
    if muts.is_empty() {
        return Err(RewindError::NothingToUndo);
    }
    let mut state: std::collections::HashMap<EvId, bool> = std::collections::HashMap::new(); // tx -> undone
    for (tx, is_undo) in undo_redo_marks(events) {
        state.insert(tx, is_undo);
    }
    // newest first
    let mut grouped: Vec<(EvId, Vec<&Mutation>)> = Vec::new();
    for m in muts.iter().rev() {
        if let Some(entry) = grouped.iter_mut().find(|(tx, _)| *tx == m.tx) {
            entry.1.push(m);
        } else {
            grouped.push((m.tx, vec![m]));
        }
    }
    let (tx, files) = grouped
        .into_iter()
        .find(|(tx, _)| !state.get(tx).copied().unwrap_or(false))
        .ok_or(RewindError::NothingToUndo)?;
    let report = restore_files(journal, ws, &files, /*use_before*/ true)?;
    journal.append(
        None,
        EventKind::MutationUndone {
            tx,
            files: report.files_restored,
            conflicts: report.conflicts.clone(),
        },
    )?;
    Ok(report)
}

/// Redo the newest undone transaction (restore after-content).
pub fn redo(
    journal: &Journal,
    ws: &Workspace,
    events: &[Event],
) -> Result<RestoreReport, RewindError> {
    let muts = mutations(events);
    let mut state: std::collections::HashMap<EvId, bool> = std::collections::HashMap::new();
    for (tx, is_undo) in undo_redo_marks(events) {
        state.insert(tx, is_undo);
    }
    let mut grouped: Vec<(EvId, Vec<&Mutation>)> = Vec::new();
    for m in muts.iter().rev() {
        if let Some(entry) = grouped.iter_mut().find(|(tx, _)| *tx == m.tx) {
            entry.1.push(m);
        } else {
            grouped.push((m.tx, vec![m]));
        }
    }
    let (tx, files) = grouped
        .into_iter()
        .find(|(tx, _)| state.get(tx).copied().unwrap_or(false))
        .ok_or(RewindError::NothingToRedo)?;
    let report = restore_files(journal, ws, &files, /*use_before*/ false)?;
    journal.append(
        None,
        EventKind::MutationRedone {
            tx,
            files: report.files_restored,
            conflicts: report.conflicts.clone(),
        },
    )?;
    Ok(report)
}

/// Restore files of one transaction. `use_before` selects the direction.
fn restore_files(
    journal: &Journal,
    ws: &Workspace,
    files: &[&Mutation],
    use_before: bool,
) -> Result<RestoreReport, RewindError> {
    let mut restored = 0usize;
    let mut conflicts = Vec::new();
    for m in files {
        let target_sha = if use_before {
            &m.before_sha
        } else {
            &m.after_sha
        };
        #[cfg(feature = "rewind_debug")]
        eprintln!(
            "[rewind-debug] file={} use_before={} before={} after={} target={}",
            m.path,
            use_before,
            &m.before_sha[..8],
            &m.after_sha[..8],
            &target_sha[..8]
        );
        let blob_sha = if use_before {
            m.before_blob.as_deref()
        } else {
            m.after_blob.as_deref()
        };
        let path = PathBuf::from(&m.path);
        // Conflict check: the file must currently hold the opposite side's content.
        let expected_now = if use_before {
            &m.after_sha
        } else {
            &m.before_sha
        };
        match ws.hash_of(&path) {
            Ok(Some(current)) if &current == expected_now => {}
            Ok(Some(current)) if &current == target_sha => {
                // already in target state (e.g. repeated undo): count as restored no-op
                restored += 1;
                continue;
            }
            // File absent: correct only if the target state is "did not exist".
            Ok(None) if target_sha == loon_workspace::EMPTY_HASH => {
                restored += 1;
                continue;
            }
            _ => {
                conflicts.push(format!(
                    "{} (changed by someone else since; not overwritten)",
                    m.path
                ));
                continue;
            }
        }
        // A creation undone = the file did not exist before: delete it.
        let is_creation = use_before && m.before_sha == loon_workspace::EMPTY_HASH;
        let Some(sha) = blob_sha else {
            if is_creation {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        restored += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        restored += 1; // already absent = target state
                    }
                    Err(e) => conflicts.push(format!("{} (delete failed: {e})", m.path)),
                }
            } else {
                conflicts.push(format!("{} (snapshot blob missing)", m.path));
            }
            continue;
        };
        let Some(bytes) = journal.blob_by_sha(sha)? else {
            conflicts.push(format!("{} (snapshot content missing)", m.path));
            continue;
        };
        if sha256_hex(&bytes) != *target_sha {
            conflicts.push(format!("{} (snapshot hash mismatch)", m.path));
            continue;
        }
        loon_workspace::write_atomic(&path, &bytes)
            .map_err(|e| RewindError::Events(loon_events::EventsError::Io(e)))?;
        restored += 1;
    }
    Ok(RestoreReport {
        files_restored: restored,
        conflicts,
    })
}

/// A request boundary usable as a rewind target.
#[derive(Debug, Clone)]
pub struct RequestBoundary {
    pub request: EvId,
    pub seq: u64,
    /// Seq of the user message (or turn start) that led to this request — everything from
    /// here on is what conversation rewind drops.
    pub truncate_to_seq: u64,
    pub member_note: String,
}

pub fn visible_user_prompt(text: &str) -> bool {
    ![
        "[ultraloop ",
        "[ultra loop]",
        "[Local lifecycle hook]",
        "[Earlier conversation summary]",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

pub fn request_boundaries(events: &[Event]) -> Vec<RequestBoundary> {
    let mut out = Vec::new();
    let mut last_user_seq: Option<u64> = None;
    for e in crate::replay::active_events(events, None) {
        match &e.kind {
            EventKind::UserMessage { text } if visible_user_prompt(text) => {
                last_user_seq = Some(e.seq)
            }
            EventKind::RequestStarted { request, .. } => {
                // Conversation rewind drops the triggering user message too, so the
                // replay bound is the seq *before* it (replay is inclusive).
                let truncate = last_user_seq
                    .map(|s| s.saturating_sub(1))
                    .unwrap_or_else(|| e.seq.saturating_sub(1));
                out.push(RequestBoundary {
                    request: *request,
                    seq: e.seq,
                    truncate_to_seq: truncate,
                    member_note: String::new(),
                });
            }
            _ => {}
        }
    }
    out
}

/// One row in the rewind picker: what the turn was, and what it changed.
#[derive(Debug, Clone)]
pub struct RewindEntry {
    /// 1 = the latest boundary, matching `/rewind N`.
    pub n: usize,
    pub boundary: RequestBoundary,
    /// The user message that opened the turn, flattened to one line.
    pub prompt: String,
    pub files_changed: usize,
    pub added: usize,
    pub removed: usize,
}

/// Boundaries with the context a person needs to choose one: the prompt that opened
/// the turn, and how much it changed. Line counts come from the journal's blobs; when a
/// snapshot is missing the counts are simply left at zero rather than guessed.
pub fn rewind_entries(journal: &Journal, events: &[Event]) -> Vec<RewindEntry> {
    let bounds = request_boundaries(events);
    let total = bounds.len();
    let active = crate::replay::active_events(events, None);
    let grouped: Vec<_> = bounds
        .iter()
        .enumerate()
        .filter(|(i, b)| *i == 0 || bounds[*i - 1].truncate_to_seq != b.truncate_to_seq)
        .collect();
    let mut out = Vec::with_capacity(grouped.len());
    for (position, (i, boundary)) in grouped.iter().enumerate() {
        let boundary = (*boundary).clone();
        // Everything between this boundary and the next belongs to this turn.
        let end_seq = grouped
            .get(position + 1)
            .map(|(_, b)| b.seq)
            .unwrap_or(u64::MAX);
        let mut prompt = String::new();
        let mut files = std::collections::BTreeMap::<String, (String, String)>::new();
        for e in active.iter().filter(|e| e.seq <= boundary.seq) {
            if let EventKind::UserMessage { text } = &e.kind {
                if visible_user_prompt(text) {
                    prompt = flatten(text);
                }
            }
        }
        for e in active
            .iter()
            .filter(|e| e.seq >= boundary.seq && e.seq < end_seq)
        {
            if let EventKind::FileMutated {
                path,
                before_sha256,
                after_sha256,
                ..
            } = &e.kind
            {
                files.insert(path.clone(), (before_sha256.clone(), after_sha256.clone()));
            }
        }
        let (mut added, mut removed) = (0, 0);
        for (before, after) in files.values() {
            let (a, r) = line_delta(journal, before, after);
            added += a;
            removed += r;
        }
        out.push(RewindEntry {
            n: total - i,
            boundary,
            prompt,
            files_changed: files.len(),
            added,
            removed,
        });
    }
    out.reverse();
    out
}

/// Added/removed line counts between two blob hashes, or (0, 0) when either is gone.
fn line_delta(journal: &Journal, before: &str, after: &str) -> (usize, usize) {
    let read = |sha: &str| {
        journal
            .blob_by_sha(sha)
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok())
    };
    let (Some(old), Some(new)) = (read(before), read(after)) else {
        return (0, 0);
    };
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    // A cheap LCS would be nicer; this reports the net change per file, which is what
    // the picker shows and is honest about direction.
    let common = old.iter().filter(|l| new.contains(l)).count();
    (
        new.len().saturating_sub(common),
        old.len().saturating_sub(common),
    )
}

/// One line, trimmed for a menu row.
fn flatten(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 90 {
        flat
    } else {
        flat.chars().take(89).collect::<String>() + "…"
    }
}

/// Rewind to before the n-th-from-last request boundary (1 = latest request).
/// Conversation mode truncates in-memory history; workspace mode restores file state.
/// Everything is journaled so resume agrees.
pub struct RewindOutcome {
    pub boundary: RequestBoundary,
    pub mode: RewindMode,
    pub history_len_after: usize,
    pub restore: Option<RestoreReport>,
    /// Original multiline user prompt, for editing and resubmission after rewind.
    pub prompt: String,
}

pub fn rewind_n(
    journal: &Journal,
    ws: &Workspace,
    events: &[Event],
    history: &mut Vec<loon_providers_types::ChatMessage>,
    n: usize,
    mode: RewindMode,
) -> Result<RewindOutcome, RewindError> {
    let bounds = request_boundaries(events);
    let boundary = bounds
        .len()
        .checked_sub(n)
        .and_then(|i| bounds.get(i))
        .cloned()
        .ok_or(RewindError::NoTarget)?;

    // Conversation: drop history entries recorded after the boundary's user message.
    // History entries carry no seq, so we replay the event stream counting messages the
    // same way resume() does, up to truncate_to_seq.
    let mut restore = None;
    if matches!(
        mode,
        RewindMode::WorkspaceOnly | RewindMode::ConversationAndWorkspace
    ) {
        let all_muts = mutations(events);
        #[cfg(feature = "rewind_debug")]
        for m in &all_muts {
            eprintln!("[muts-debug] seq={} path={} tx={}", m.seq, m.path, m.tx);
        }
        #[cfg(feature = "rewind_debug")]
        eprintln!(
            "[muts-debug] boundary.seq={} truncate_to_seq={}",
            boundary.seq, boundary.truncate_to_seq
        );
        let muts: Vec<Mutation> = all_muts
            .into_iter()
            .filter(|m| m.seq > boundary.truncate_to_seq)
            .collect();
        // restore in reverse order of application
        let mut refs: Vec<&Mutation> = muts.iter().rev().collect();
        // group-wise reverse: unique txs newest first
        let mut grouped: Vec<(EvId, Vec<&Mutation>)> = Vec::new();
        for m in refs.drain(..) {
            if let Some(entry) = grouped.iter_mut().find(|(tx, _)| *tx == m.tx) {
                entry.1.push(m);
            } else {
                grouped.push((m.tx, vec![m]));
            }
        }
        let mut total = RestoreReport {
            files_restored: 0,
            conflicts: vec![],
        };
        for (_, files) in grouped {
            let r = restore_files(journal, ws, &files, true)?;
            total.files_restored += r.files_restored;
            total.conflicts.extend(r.conflicts);
        }
        restore = Some(total);
    }

    let mut history_len_after = history.len();
    if matches!(
        mode,
        RewindMode::ConversationOnly | RewindMode::ConversationAndWorkspace
    ) {
        // Exact: replay the event stream up to the boundary with the same rules used by
        // resume, so the truncated history is exactly what a fresh session would have.
        let truncated = crate::replay::replay_history(events, Some(boundary.truncate_to_seq));
        *history = truncated;
        history_len_after = history.len();
    }

    journal.append(
        None,
        EventKind::Rewound {
            target_request: boundary.request,
            mode: mode.as_str().to_string(),
            truncate_to_seq: boundary.truncate_to_seq,
            files_restored: restore.as_ref().map(|r| r.files_restored).unwrap_or(0),
            files_conflicted: restore.as_ref().map(|r| r.conflicts.len()).unwrap_or(0),
        },
    )?;

    let prompt = events
        .iter()
        .find_map(|e| {
            if e.seq > boundary.truncate_to_seq && e.seq <= boundary.seq {
                if let EventKind::UserMessage { text } = &e.kind {
                    return Some(text.clone());
                }
            }
            None
        })
        .unwrap_or_default();
    Ok(RewindOutcome {
        boundary,
        mode,
        history_len_after,
        restore,
        prompt,
    })
}

/// Fork: record a named branch point without touching anything.
pub fn fork(journal: &Journal, events: &[Event], name: &str) -> Result<u64, RewindError> {
    let at_seq = events.last().map(|e| e.seq).unwrap_or(0);
    journal.append(
        None,
        EventKind::ForkCreated {
            name: name.to_string(),
            at_seq,
        },
    )?;
    Ok(at_seq)
}

pub fn checkpoint(journal: &Journal, events: &[Event], name: &str) -> Result<u64, RewindError> {
    let at_seq = events.last().map(|e| e.seq).unwrap_or(0);
    journal.append(
        None,
        EventKind::CheckpointCreated {
            name: name.to_string(),
            at_seq,
        },
    )?;
    Ok(at_seq)
}

/// Restore workspace to a named checkpoint's seq (reverse-applying later mutations).
pub fn restore_checkpoint(
    journal: &Journal,
    ws: &Workspace,
    events: &[Event],
    name: &str,
) -> Result<RestoreReport, RewindError> {
    let cp = events.iter().rev().find_map(|e| match &e.kind {
        EventKind::CheckpointCreated { name: n, at_seq } if n == name => Some(*at_seq),
        _ => None,
    });
    let Some(at_seq) = cp else {
        return Err(RewindError::NoTarget);
    };
    let muts: Vec<Mutation> = mutations(events)
        .into_iter()
        .filter(|m| m.seq > at_seq)
        .collect();
    let mut grouped: Vec<(EvId, Vec<&Mutation>)> = Vec::new();
    for m in muts.iter().rev() {
        if let Some(entry) = grouped.iter_mut().find(|(tx, _)| *tx == m.tx) {
            entry.1.push(m);
        } else {
            grouped.push((m.tx, vec![m]));
        }
    }
    let mut total = RestoreReport {
        files_restored: 0,
        conflicts: vec![],
    };
    for (_, files) in grouped {
        let r = restore_files(journal, ws, &files, true)?;
        total.files_restored += r.files_restored;
        total.conflicts.extend(r.conflicts);
    }
    Ok(total)
}

/// Helper for callers holding only paths.
pub fn open_journal(paths: &LoonPaths, id: EvId) -> Result<Arc<Journal>, RewindError> {
    let store = crate::open_store(paths)?;
    let (j, _) = Journal::open_default(&store, &paths.sessions_dir(), id)?;
    Ok(Arc::new(j))
}

/// BlobRef sizes are resolved lazily: rebuild exact BlobRefs from the store.
pub fn blob_ref(journal: &Journal, sha: &str) -> Option<BlobRef> {
    journal.blob_by_sha(sha).ok().flatten().map(|data| BlobRef {
        sha256: sha.to_string(),
        bytes: data.len() as u64,
    })
}

/// Type-only re-export so this module can name history messages without leaking a
/// direct providers dependency into signatures.
pub mod loon_providers_types {
    pub use loon_providers::ChatMessage;
}
