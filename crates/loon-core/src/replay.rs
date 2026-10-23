//! Event-sourced conversation replay.
//!
//! One function rebuilds the wire-format conversation history from journal events, so
//! live sessions, resume, and rewind cannot drift apart: the same rules that appended
//! messages while running are applied to the same events afterward.

use loon_events::model::{Event, EventKind};
use loon_providers::{ChatMessage, Role, ToolCallReq};
use std::collections::HashMap;

fn push_result_ordered(history: &mut Vec<ChatMessage>, wire: String, text: String) {
    if let Some(index) = history
        .iter()
        .rposition(|m| m.tool_calls.iter().any(|c| c.id == wire))
    {
        let calls = &history[index].tool_calls;
        let rank = calls.iter().position(|c| c.id == wire).unwrap_or(0);
        let before = history[index + 1..]
            .iter()
            .take_while(|m| m.role == Role::Tool)
            .filter(|m| {
                m.tool_call_id
                    .as_ref()
                    .and_then(|id| calls.iter().position(|c| &c.id == id))
                    .map(|r| r < rank)
                    .unwrap_or(false)
            })
            .count();
        history.insert(index + 1 + before, ChatMessage::tool_result(wire, text));
    } else {
        history.push(ChatMessage::tool_result(wire, text));
    }
}

/// Resolve conversation rewinds in journal order, retaining work created after a
/// rewind. Workspace-only rewinds leave the conversation intact.
pub(crate) fn active_events(events: &[Event], upto_seq: Option<u64>) -> Vec<&Event> {
    let mut active: Vec<&Event> = Vec::new();
    for event in events
        .iter()
        .take_while(|e| upto_seq.map(|n| e.seq <= n).unwrap_or(true))
    {
        if let EventKind::Rewound {
            truncate_to_seq,
            mode,
            ..
        } = &event.kind
        {
            if mode != "workspace" {
                active.retain(|e| e.seq <= *truncate_to_seq);
            }
        } else {
            active.push(event);
        }
    }
    active
}

/// Replay journal events into conversation history. `upto_seq` (inclusive) bounds the
/// replay — used by rewind to drop everything after a boundary.
pub fn replay_history(events: &[Event], upto_seq: Option<u64>) -> Vec<ChatMessage> {
    let mut history: Vec<ChatMessage> = Vec::new();
    let mut pending_calls: std::collections::BTreeMap<String, Vec<ToolCallReq>> =
        std::collections::BTreeMap::new();
    let mut call_requests: HashMap<loon_events::EvId, String> = HashMap::new();
    let mut call_wires: HashMap<loon_events::EvId, String> = HashMap::new();
    let mut unanswered: std::collections::VecDeque<(String, String)> =
        std::collections::VecDeque::new();
    let mut assistant_text = String::new();

    let flush_text = |history: &mut Vec<ChatMessage>, text: &mut String| {
        if !text.is_empty() {
            history.push(ChatMessage::assistant(std::mem::take(text)));
        }
    };

    for e in active_events(events, upto_seq) {
        match &e.kind {
            EventKind::UserMessage { text } => {
                flush_text(&mut history, &mut assistant_text);
                history.push(ChatMessage::user(text.clone()));
            }
            EventKind::TextDelta { text, .. } => assistant_text.push_str(text),
            EventKind::ToolCallObserved {
                request,
                wire_id,
                name,
                arguments_json,
                call,
                ..
            } => {
                call_requests.insert(*call, request.to_string());
                call_wires.insert(*call, wire_id.clone());
                unanswered.push_back((wire_id.clone(), name.clone()));
                pending_calls
                    .entry(request.to_string())
                    .or_default()
                    .push(ToolCallReq {
                        id: wire_id.clone(),
                        name: name.clone(),
                        arguments_json: arguments_json.clone(),
                    });
            }
            EventKind::ToolExecutionFinished {
                call,
                excerpt,
                blob,
                name,
                ..
            } => {
                flush_text(&mut history, &mut assistant_text);
                let req = call_requests
                    .get(call)
                    .cloned()
                    .or_else(|| pending_calls.keys().next().cloned());
                if let Some(req) = req {
                    if let Some(batch) = pending_calls.remove(&req) {
                        if !batch.is_empty() {
                            history.push(ChatMessage {
                                role: Role::Assistant,
                                text: String::new(),
                                tool_calls: batch,
                                tool_call_id: None,
                            });
                        }
                    }
                }
                let result_text = match blob {
                    Some(b) => format!(
                        "{excerpt}\n[full output in blob {}]",
                        &b.sha256[..b.sha256.len().min(12)]
                    ),
                    None => excerpt.clone(),
                };
                let wire = call_wires
                    .remove(call)
                    .or_else(|| {
                        unanswered
                            .iter()
                            .find(|(_, n)| n == name)
                            .map(|(id, _)| id.clone())
                    })
                    .unwrap_or_else(|| call.to_string());
                unanswered.retain(|(id, _)| id != &wire);
                push_result_ordered(&mut history, wire, result_text);
            }
            EventKind::ToolCallRejected {
                request,
                name,
                reason,
            } => {
                let req = call_requests.get(request).cloned();
                if let Some(req) = req {
                    if let Some(batch) = pending_calls.remove(&req) {
                        history.push(ChatMessage {
                            role: Role::Assistant,
                            text: String::new(),
                            tool_calls: batch,
                            tool_call_id: None,
                        });
                    }
                }
                if let Some(wire) = call_wires.remove(request).or_else(|| {
                    unanswered
                        .iter()
                        .find(|(_, n)| n == name)
                        .map(|(id, _)| id.clone())
                }) {
                    unanswered.retain(|(id, _)| id != &wire);
                    push_result_ordered(&mut history, wire, format!("ERROR: {reason}"));
                }
            }

            EventKind::RequestFinished { request, .. } => {
                flush_text(&mut history, &mut assistant_text);
                if let Some(batch) = pending_calls.remove(&request.to_string()) {
                    if !batch.is_empty() {
                        history.push(ChatMessage {
                            role: Role::Assistant,
                            text: String::new(),
                            tool_calls: batch,
                            tool_call_id: None,
                        });
                    }
                }
            }
            EventKind::TurnFinished { .. } => {
                flush_text(&mut history, &mut assistant_text);
            }

            // Compaction replaces everything the model saw before this point with the
            // summary plus the verbatim kept tail — exactly what the live session held.
            EventKind::ContextCompacted { summary, kept, .. } => {
                flush_text(&mut history, &mut assistant_text);
                history.clear();
                pending_calls.clear();
                call_requests.clear();
                call_wires.clear();
                unanswered.clear();
                history.push(ChatMessage::user(format!(
                    "[Earlier conversation summary]\n{summary}"
                )));
                for m in kept {
                    history.push(ChatMessage {
                        role: match m.role.as_str() {
                            "assistant" => Role::Assistant,
                            "tool" => Role::Tool,
                            _ => Role::User,
                        },
                        text: m.text.clone(),
                        tool_calls: m
                            .tool_calls
                            .iter()
                            .map(|c| ToolCallReq {
                                id: c.id.clone(),
                                name: c.name.clone(),
                                arguments_json: c.arguments_json.clone(),
                            })
                            .collect(),
                        tool_call_id: m.tool_call_id.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    flush_text(&mut history, &mut assistant_text);
    for (_, batch) in pending_calls {
        if !batch.is_empty() {
            history.push(ChatMessage {
                role: Role::Assistant,
                text: String::new(),
                tool_calls: batch,
                tool_call_id: None,
            });
        }
    }
    history
}

/// One row of the user's chat view, rebuilt from the journal.
///
/// This is what a resumed session (and a rewind) hands to the UI, so it has to carry
/// everything the live transcript showed. It used to be `(role, text)` pairs, which
/// silently dropped every tool call: `replay_history` keeps them as `Role::Tool`
/// messages for the model, but the old filter only passed user/assistant *text*, so a
/// resumed conversation read as bare prose with all of its commands, reads and MCP
/// calls missing. Tool rows therefore have their own variant carrying the same fields
/// the live `Block::Tool` row renders.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum VisibleBlock {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Tool {
        name: String,
        /// Human-facing command/path (`$ cargo test`), as the live row shows it.
        detail: String,
        ok: bool,
        excerpt: String,
    },
}

/// The visible transcript retains original exchanges even after model compaction.
/// Compaction snapshots affect provider history only, never the user's chat view.
///
/// Rebuilt from the event stream rather than from `replay_history`, because the wire
/// history keeps a tool result's text but discards the `ok` flag and the call arguments
/// that make a tool row readable.
pub fn visible_blocks(events: &[Event], upto_seq: Option<u64>) -> Vec<VisibleBlock> {
    let mut out: Vec<VisibleBlock> = Vec::new();
    let mut assistant_text = String::new();
    // A call is announced by `ToolCallObserved` (which carries the arguments that make
    // `detail`) and completed by `ToolExecutionFinished` (which carries `ok`). Remember
    // the announcement so the finished row can be rendered the way the live one was.
    let mut announced: HashMap<loon_events::EvId, (String, String)> = HashMap::new();
    let mut finished: std::collections::HashSet<loon_events::EvId> =
        std::collections::HashSet::new();

    let flush_text = |out: &mut Vec<VisibleBlock>, text: &mut String| {
        if !text.is_empty() {
            out.push(VisibleBlock::Assistant {
                text: std::mem::take(text),
            });
        }
    };

    for e in active_events(events, upto_seq) {
        match &e.kind {
            // Compaction changes what the model sees, never the user's chat view.
            EventKind::ContextCompacted { .. } => {}
            EventKind::UserMessage { text } => {
                flush_text(&mut out, &mut assistant_text);
                // Harness prompts (`[ultraloop …]`, summaries, hook context) are not the
                // user's words and were never shown as user rows.
                if crate::rewind::visible_user_prompt(text) {
                    out.push(VisibleBlock::User { text: text.clone() });
                }
            }
            EventKind::TextDelta { text, .. } => assistant_text.push_str(text),
            EventKind::ToolCallObserved {
                call,
                name,
                arguments_json,
                ..
            } => {
                let detail = match serde_json::from_str::<serde_json::Value>(arguments_json) {
                    Ok(args) => crate::tool_detail(name, &args),
                    Err(_) => String::new(),
                };
                announced.insert(*call, (name.clone(), detail));
            }
            EventKind::ToolExecutionFinished {
                call,
                name,
                ok,
                excerpt,
                ..
            } => {
                flush_text(&mut out, &mut assistant_text);
                finished.insert(*call);
                let detail = announced
                    .remove(call)
                    .map(|(_, detail)| detail)
                    .unwrap_or_default();
                out.push(VisibleBlock::Tool {
                    name: name.clone(),
                    detail,
                    ok: *ok,
                    excerpt: excerpt.clone(),
                });
            }
            EventKind::ToolCallRejected { name, reason, .. } => {
                flush_text(&mut out, &mut assistant_text);
                out.push(VisibleBlock::Tool {
                    name: name.clone(),
                    detail: String::new(),
                    ok: false,
                    excerpt: format!("rejected: {reason}"),
                });
            }
            // A request that ended with text and no further deltas still needs flushing;
            // `TurnFinished` below covers the ordinary case.
            _ => {}
        }
    }
    flush_text(&mut out, &mut assistant_text);
    // A call that was announced but never finished (cancelled turn, dropped stream) was
    // shown as running and then as cancelled. Report it rather than losing it silently.
    for (call, (name, detail)) in announced {
        if !finished.contains(&call) {
            out.push(VisibleBlock::Tool {
                name,
                detail,
                ok: false,
                excerpt: "cancelled — no result arrived".into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use loon_events::model::{EvId, Usage};

    fn ev_seq(seq: u64, kind: EventKind) -> Event {
        Event {
            id: EvId::now_v7(),
            parent: None,
            seq,
            ts_ms: 0,
            kind,
        }
    }

    #[test]
    fn rebuilds_user_assistant_and_tool_exchange() {
        let events = vec![
            ev_seq(
                1,
                EventKind::UserMessage {
                    text: "fix it".into(),
                },
            ),
            ev_seq(
                2,
                EventKind::TextDelta {
                    request: EvId::nil(),
                    text: "Reading.".into(),
                },
            ),
            ev_seq(
                3,
                EventKind::ToolCallObserved {
                    request: EvId::nil(),
                    call: EvId::now_v7(),
                    wire_id: "call_9".into(),
                    name: "read".into(),
                    arguments_json: r#"{"path":"a.txt"}"#.into(),
                },
            ),
            ev_seq(
                4,
                EventKind::ToolExecutionFinished {
                    call: EvId::nil(),
                    name: "read".into(),
                    ok: true,
                    excerpt: "file body".into(),
                    blob: None,
                    duration_ms: 3,
                },
            ),
            ev_seq(
                5,
                EventKind::TextDelta {
                    request: EvId::nil(),
                    text: "Done.".into(),
                },
            ),
            ev_seq(
                6,
                EventKind::RequestFinished {
                    request: EvId::nil(),
                    outcome: loon_events::model::RequestOutcome::Completed {
                        finish: "stop".into(),
                    },
                    usage: Usage::default(),
                    usage_reported: false,
                },
            ),
        ];
        let h = replay_history(&events, None);
        // user, "Reading.", assistant-with-calls, tool-result, "Done." (flushed at
        // RequestFinished — exactly what the live session pushed)
        assert_eq!(h.len(), 5, "got {h:?}");
        assert_eq!(h[0].text, "fix it");
        assert_eq!(h[1].text, "Reading.");
        assert_eq!(h[2].tool_calls[0].id, "call_9");
        assert_eq!(
            h[3].tool_call_id.as_deref(),
            Some("call_9"),
            "result bound to its call"
        );
        assert_eq!(h[3].text, "file body");
        assert_eq!(h[4].text, "Done.");
    }

    #[test]
    fn upto_seq_truncates_deterministically() {
        let events = vec![
            ev_seq(
                1,
                EventKind::UserMessage {
                    text: "first".into(),
                },
            ),
            ev_seq(
                2,
                EventKind::UserMessage {
                    text: "second".into(),
                },
            ),
        ];
        let h = replay_history(&events, Some(1));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].text, "first");
    }
}
