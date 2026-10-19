//! Event-sourced conversation replay.
//!
//! One function rebuilds the wire-format conversation history from journal events, so
//! live sessions, resume, and rewind cannot drift apart: the same rules that appended
//! messages while running are applied to the same events afterward.

use loon_events::model::{Event, EventKind};
use loon_providers::{ChatMessage, Role, ToolCallReq};
use std::collections::HashMap;

/// Replay journal events into conversation history. `upto_seq` (inclusive) bounds the
/// replay — used by rewind to drop everything after a boundary.
pub fn replay_history(events: &[Event], upto_seq: Option<u64>) -> Vec<ChatMessage> {
    let mut history: Vec<ChatMessage> = Vec::new();
    // Calls observed but not yet answered, keyed by request id (wire ids preserved).
    let mut pending_calls: HashMap<String, Vec<ToolCallReq>> = HashMap::new();
    let mut assistant_text = String::new();

    let flush_text = |history: &mut Vec<ChatMessage>, text: &mut String| {
        if !text.is_empty() {
            history.push(ChatMessage::assistant(std::mem::take(text)));
        }
    };

    for e in events {
        if let Some(upto) = upto_seq {
            if e.seq > upto {
                break;
            }
        }
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
                ..
            } => {
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
                ..
            } => {
                flush_text(&mut history, &mut assistant_text);
                // Emit the assistant message that carried these calls (once per request
                // is not trackable here; instead attach all still-pending calls of the
                // same turn by matching nothing — the journal orders observed calls
                // before their results, so drain per call in order).
                // Find the oldest request that still has pending calls and pop one.
                let mut answered: Option<String> = None;
                for (req, calls) in pending_calls.iter_mut() {
                    if !calls.is_empty() {
                        let call_req = calls.remove(0);
                        // push assistant-with-calls containing every call observed for
                        // this request up to now that has not been emitted
                        let all: Vec<ToolCallReq> = std::mem::take(calls);
                        let mut batch = vec![call_req];
                        batch.extend(all);
                        history.push(ChatMessage {
                            role: Role::Assistant,
                            text: String::new(),
                            tool_calls: batch,
                            tool_call_id: None,
                        });
                        answered = Some(req.clone());
                        break;
                    }
                }
                let _ = answered;
                let result_text = match blob {
                    Some(b) => format!(
                        "{excerpt}\n[full output in blob {}]",
                        &b.sha256[..b.sha256.len().min(12)]
                    ),
                    None => excerpt.clone(),
                };
                // tool result id: the first wire id of the last assistant message
                let call_id = history
                    .iter()
                    .rev()
                    .find_map(|m| m.tool_calls.first().map(|c| c.id.clone()))
                    .unwrap_or_else(|| call.to_string());
                history.push(ChatMessage::tool_result(call_id, result_text));
            }
            EventKind::RequestFinished { .. } | EventKind::TurnFinished { .. } => {
                flush_text(&mut history, &mut assistant_text);
            }
            _ => {}
        }
    }
    flush_text(&mut history, &mut assistant_text);
    history
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
