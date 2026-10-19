//! Context compaction: when the conversation approaches the model's context window,
//! older exchanges are replaced by a summary plus a retained recent tail.
//!
//! Honesty rules:
//! - the window comes from config (`models.*.context_window` / combo minimums); when it
//!   is unknown a conservative 128k default is used and the summary request itself is an
//!   ordinary journaled model call,
//! - the compaction event carries the kept tail verbatim, so resume replays exactly
//!   what the live session held (see `replay.rs`),
//! - token counts are estimates (~4 chars/token) and are labeled as estimates wherever
//!   shown.

use crate::permissions::futures_future::BoxFuture;
use crate::Agent;
use loon_events::model::{EventKind, JournalMessage, JournalToolCall};
use loon_events::EvId;
use loon_providers::{AttemptEvent, ChatMessage, ProviderEvent, Role};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Compaction starts at this fraction of the context window.
pub const COMPACT_THRESHOLD: f64 = 0.8;
/// Recent exchanges (user → final assistant/tool messages) kept verbatim.
pub const KEEP_RECENT_EXCHANGES: usize = 2;

/// Produce a summary for one transcript chunk. The default implementation asks the
/// serving model; tests install deterministic closures.
pub type Summarizer = Arc<dyn Fn(String) -> BoxFuture<String> + Send + Sync>;

/// Render history into the plain-text transcript handed to the summarizer.
pub fn render_transcript(msgs: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m.role {
            Role::User => out.push_str(&format!("[user] {}\n", m.text)),
            Role::Assistant => {
                if !m.text.is_empty() {
                    out.push_str(&format!("[assistant] {}\n", m.text));
                }
                for c in &m.tool_calls {
                    out.push_str(&format!(
                        "[assistant tool call] {}({})\n",
                        c.name, c.arguments_json
                    ));
                }
            }
            Role::Tool => out.push_str(&format!("[tool result] {}\n", m.text)),
            Role::System => out.push_str(&format!("[system] {}\n", m.text)),
        }
    }
    out
}

/// Index in `history` where the summarized prefix ends: everything before the user
/// message that opens the `keep`-th-from-last exchange. None when there is nothing
/// worth summarizing (too few exchanges, or the prefix is empty).
pub fn split_point(history: &[ChatMessage], keep: usize) -> Option<usize> {
    let user_positions: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.role == Role::User && !m.text.starts_with("[Earlier conversation summary]")
        })
        .map(|(i, _)| i)
        .collect();
    if user_positions.len() <= keep {
        return None;
    }
    // Summarize up to (but not including) the user message that begins the window.
    let split = user_positions[user_positions.len() - keep];
    (split > 0).then_some(split)
}

/// Default summarizer: one no-tools model call over the transcript.
pub fn model_summarizer(
    registry: Arc<loon_providers::ProviderRegistry>,
    model_ref: String,
) -> Summarizer {
    Arc::new(move |transcript: String| {
        let registry = registry.clone();
        let model_ref = model_ref.clone();
        Box::pin(async move {
            let req = loon_providers::ChatRequest {
                model: model_ref,
                messages: vec![
                    ChatMessage::user(
                        "Summarize the following working-session transcript for continued \
                         coding work. Keep: the task and its current state, every file path \
                         mentioned, decisions made, and any unfinished steps. Be concise; \
                         plain text only.\n\n--- TRANSCRIPT ---\n",
                    ),
                    ChatMessage::user(transcript),
                ],
                tools: vec![],
                max_output_tokens: Some(2048),
                temperature: None,
            };
            let (tx, mut rx) = mpsc::channel::<AttemptEvent>(256);
            let mut runner = loon_providers::ComboRunner::new(registry);
            runner.max_retries = 1;
            let handle = tokio::spawn(async move { runner.run(&req.model, &req, tx).await });
            let mut summary = String::new();
            while let Some(ev) = rx.recv().await {
                if let ProviderEvent::TextDelta { text } = ev.event {
                    summary.push_str(&text);
                }
            }
            let _ = handle.await;
            summary
        })
    })
}

impl Agent {
    /// Estimated context size of the next request, in tokens (heuristic, ~4 chars/token).
    pub fn estimated_context_tokens(&self) -> u64 {
        crate::est::estimate_messages(&self.assemble_messages())
    }

    /// True when the next request would cross the compaction threshold.
    pub fn should_compact(&self) -> bool {
        self.cfg.context_window > 0
            && (self.estimated_context_tokens() as f64)
                > self.cfg.context_window as f64 * COMPACT_THRESHOLD
    }

    /// Compact now: summarize the older prefix through the summarizer, journal the
    /// result, and reshape live history. Returns false when there was nothing to
    /// summarize (kept exchanges only).
    pub async fn compact_now(&mut self) -> bool {
        let Some(split) = split_point(&self.history, KEEP_RECENT_EXCHANGES) else {
            return false;
        };
        let before_tokens = self.estimated_context_tokens();
        let summarized = self.history[..split].to_vec();
        let kept = self.history[split..].to_vec();
        let transcript = render_transcript(&summarized);
        let summarizer = match &self.summarizer {
            Some(s) => s.clone(),
            None => model_summarizer(self.registry.clone(), self.cfg.model_ref.clone()),
        };
        let summary = summarizer(transcript).await;
        let after_tokens = {
            let synthetic: Vec<ChatMessage> = std::iter::once(ChatMessage::user(format!(
                "[Earlier conversation summary]\n{summary}"
            )))
            .chain(kept.iter().cloned())
            .collect();
            crate::est::estimate_messages(&synthetic)
        };
        let _ = self.journal.append(
            None,
            EventKind::ContextCompacted {
                turn: EvId::now_v7(),
                before_tokens,
                after_tokens,
                summary: summary.clone(),
                kept: to_journal_messages(&kept),
            },
        );
        self.history = vec![ChatMessage::user(format!(
            "[Earlier conversation summary]\n{summary}"
        ))];
        self.history.extend(kept);
        self.emit(crate::AgentEvent::MemberNote(format!(
            "context compacted: ~{before_tokens} → ~{after_tokens} est. tokens (estimate)"
        )));
        true
    }

    /// Compact if the threshold is crossed. Called before each provider request.
    pub async fn maybe_compact(&mut self) {
        if self.should_compact() {
            let _ = self.compact_now().await;
        }
    }
}

pub fn to_journal_messages(msgs: &[ChatMessage]) -> Vec<JournalMessage> {
    msgs.iter()
        .map(|m| JournalMessage {
            role: match m.role {
                Role::User => "user".into(),
                Role::Assistant => "assistant".into(),
                Role::Tool => "tool".into(),
                Role::System => "system".into(),
            },
            text: m.text.clone(),
            tool_calls: m
                .tool_calls
                .iter()
                .map(|c| JournalToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments_json: c.arguments_json.clone(),
                })
                .collect(),
            tool_call_id: m.tool_call_id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> ChatMessage {
        ChatMessage::user(t)
    }

    #[test]
    fn split_point_keeps_recent_exchanges() {
        let h = vec![
            user("e1"),
            ChatMessage::assistant("a1"),
            user("e2"),
            ChatMessage::assistant("a2"),
            user("e3"),
            ChatMessage::assistant("a3"),
        ];
        // keep 2 recent exchanges (e2, e3): summarize [e1, a1] → split at index 2
        assert_eq!(split_point(&h, 2), Some(2));
        let h2 = vec![user("e1"), user("e2"), user("e3")];
        assert_eq!(split_point(&h2, 2), Some(1));
        // nothing to summarize
        assert_eq!(split_point(&h[..2], 2), None);
        assert_eq!(split_point(&[], 2), None);
    }

    #[test]
    fn transcript_renders_roles() {
        let t = render_transcript(&[user("do it"), ChatMessage::assistant("reading"), user("ok")]);
        assert!(t.contains("[user] do it"));
        assert!(t.contains("[assistant] reading"));
        assert!(t.contains("[user] ok"));
    }
}
