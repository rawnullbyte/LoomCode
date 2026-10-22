//! One-shot, tools-free model requests: the single place that turns a semantic
//! request into a provider call.
//!
//! Supervisor reviews and compaction summaries used to carry their own copy of the
//! streaming loop, and the copies drifted: one had no cancellation, another had no
//! truncation check, a third reported "no answer text" without saying which member
//! served or what stopped it. Everything here shares one retry policy, one attribution
//! rule and one vocabulary for failure, so a request that cannot be used is reported
//! with the reason instead of surfacing as an unexplained parse error upstream.
//!
//! Contract of this module:
//! - requests are tools-free (a one-shot answer cannot execute anything);
//! - nothing here touches conversation history, the turn journal or the work state —
//!   callers own their side effects and validate the returned text;
//! - a returned [`Answer`] is a completed, non-empty, non-truncated answer, because a
//!   truncated answer is a failure for every caller that parses it;
//! - [`OneShot::request_parsed`] additionally guarantees a *validated* value: prose,
//!   malformed JSON and truncated answers are repaired by re-asking, with the output
//!   budget raised when the cut was a token limit.

use loon_events::Usage;
use loon_providers::{
    AttemptEvent, ChatMessage, ChatRequest, ComboRunner, ProviderEvent, ProviderRegistry,
    RotationState,
};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

/// Called once per provider attempt event, for callers that journal attribution.
pub type AttemptSink = Arc<dyn Fn(&AttemptEvent) + Send + Sync>;

/// A completed answer with the attribution needed to explain it.
#[derive(Debug, Clone)]
pub struct Answer {
    pub text: String,
    /// `provider/model` that actually served, or the requested reference.
    pub served: String,
    /// Finish reason as reported by the provider (`Stop`, `Length`, …).
    pub finish: String,
    pub usage: Usage,
    pub usage_reported: bool,
    /// Characters spent on reasoning: distinguishes a reasoning-only answer from an
    /// empty one, which look identical on the wire.
    pub reasoning_chars: usize,
    /// Attempts abandoned and restarted by the retry policy.
    pub retries: u32,
    /// Members skipped or failed, formatted for a message (empty when none).
    pub trail: String,
    /// The provider stopped at the output limit: the text is a prefix, not an answer.
    pub truncated: bool,
}

impl Answer {
    /// Why this answer has no usable text, or `None` when it does.
    fn unusable(&self) -> Option<String> {
        if self.truncated {
            return Some(format!(
                "{} stopped at the output limit after {} chars; the answer is incomplete{}{}",
                self.served,
                self.text.chars().count(),
                retry_note(self.retries),
                self.trail
            ));
        }
        if self.text.trim().is_empty() {
            return Some(if self.reasoning_chars > 0 {
                format!(
                    "{} spent {} chars on reasoning and returned no answer text (finish: {}){}",
                    self.served, self.reasoning_chars, self.finish, self.trail
                )
            } else {
                format!(
                    "{} returned no answer text (finish: {}){}",
                    self.served, self.finish, self.trail
                )
            });
        }
        None
    }
}

fn retry_note(retries: u32) -> String {
    if retries == 0 {
        String::new()
    } else {
        format!(
            " (after {} retr{})",
            retries,
            if retries == 1 { "y" } else { "ies" }
        )
    }
}

/// A validated answer: `value` is the caller's own parse of `answer.text`.
#[derive(Debug, Clone)]
pub struct Parsed<T> {
    pub value: T,
    pub answer: Answer,
    /// Requests issued, including repairs (1 means the first answer was accepted).
    pub attempts: u32,
}

impl<T> Parsed<T> {
    /// True when the answer needed at least one repair round.
    pub fn repaired(&self) -> bool {
        self.attempts > 1
    }
}

/// Builder for one request. Defaults mirror the session defaults; callers override
/// what they need and keep the rest shared.
pub struct OneShot {
    registry: Arc<ProviderRegistry>,
    cancel: Option<watch::Receiver<bool>>,
    rotation: Option<Arc<RotationState>>,
    max_retries: u32,
    allow_retry_after_partial_stream: bool,
    max_output_tokens: u64,
    /// Ceiling the output budget may be raised to when an answer is truncated.
    token_ceiling: u64,
    /// Total requests a parsed call may issue, including repairs.
    max_attempts: u32,
    on_attempt: Option<AttemptSink>,
    label: String,
}

impl OneShot {
    pub fn new(registry: Arc<ProviderRegistry>) -> OneShot {
        OneShot {
            registry,
            cancel: None,
            rotation: None,
            max_retries: 2,
            allow_retry_after_partial_stream: false,
            max_output_tokens: 2048,
            token_ceiling: 8192,
            max_attempts: 3,
            on_attempt: None,
            label: "model request".into(),
        }
    }

    /// How the request is named in error messages, so a failure says which subsystem
    /// could not get an answer.
    pub fn label(mut self, label: &str) -> OneShot {
        self.label = label.to_string();
        self
    }

    pub fn cancel(mut self, cancel: watch::Receiver<bool>) -> OneShot {
        self.cancel = Some(cancel);
        self
    }

    pub fn rotation(mut self, rotation: Arc<RotationState>) -> OneShot {
        self.rotation = Some(rotation);
        self
    }

    /// Per-member transport retry budget, forwarded to the combo runner.
    pub fn retries(mut self, retries: u32) -> OneShot {
        self.max_retries = retries;
        self
    }

    pub fn allow_retry_after_partial_stream(mut self, allow: bool) -> OneShot {
        self.allow_retry_after_partial_stream = allow;
        self
    }

    pub fn max_output_tokens(mut self, tokens: u64) -> OneShot {
        self.max_output_tokens = tokens;
        self
    }

    /// Highest output budget a truncated answer may be escalated to.
    pub fn token_ceiling(mut self, tokens: u64) -> OneShot {
        self.token_ceiling = tokens.max(self.max_output_tokens);
        self
    }

    /// Total requests a parsed call may issue, including repairs (minimum 1).
    pub fn max_attempts(mut self, attempts: u32) -> OneShot {
        self.max_attempts = attempts.max(1);
        self
    }

    pub fn on_attempt(mut self, sink: AttemptSink) -> OneShot {
        self.on_attempt = Some(sink);
        self
    }

    /// Run one request to completion. Every failure mode is named: transport errors,
    /// cancellation, an empty answer, and a truncated answer.
    pub async fn request(
        &self,
        model_ref: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<Answer, String> {
        let answer = self
            .run(model_ref, messages, self.max_output_tokens)
            .await?;
        match answer.unusable() {
            Some(reason) => Err(format!("{}: {reason}", self.label)),
            None => Ok(answer),
        }
    }

    /// Run a request whose answer must satisfy `validate` (typically a JSON parse).
    ///
    /// Retry policy, cheapest first:
    /// 1. a truncated answer is re-asked with a larger output budget — the answer was
    ///    cut, not wrong, so the same question deserves more room;
    /// 2. an answer `validate` rejects is followed by a repair request that shows the
    ///    model what it actually sent and what was required;
    /// 3. transport failures are retried inside the combo runner.
    ///
    /// When `max_attempts` is spent the error lists every reason, so the caller can say
    /// what actually went wrong instead of "the answer was unusable".
    pub async fn request_parsed<T, F>(
        &self,
        model_ref: &str,
        messages: Vec<ChatMessage>,
        validate: F,
    ) -> Result<Parsed<T>, String>
    where
        F: Fn(&str) -> Result<T, String>,
    {
        let mut conversation = messages;
        let mut budget = self.max_output_tokens;
        let mut failures: Vec<String> = Vec::new();
        let mut attempts = 0u32;
        while attempts < self.max_attempts {
            attempts += 1;
            let answer = match self.run(model_ref, conversation.clone(), budget).await {
                Ok(answer) => answer,
                // The combo runner already spent its transport retry budget; repeating
                // an identical request here would only add latency.
                Err(error) => return Err(format!("{}: {error}", self.label)),
            };
            if let Some(reason) = answer.unusable() {
                // A cut answer gets more room; an empty one gets a nudge to answer.
                if answer.truncated && budget < self.token_ceiling {
                    budget = (budget * 2).min(self.token_ceiling);
                } else {
                    conversation.push(ChatMessage::assistant(answer.text.clone()));
                    conversation.push(ChatMessage::user(
                        "Your previous reply carried no usable text. Send the requested \
                         answer now.",
                    ));
                }
                failures.push(reason);
                continue;
            }
            match validate(&answer.text) {
                Ok(value) => {
                    return Ok(Parsed {
                        value,
                        answer,
                        attempts,
                    })
                }
                Err(reason) => {
                    conversation.push(ChatMessage::assistant(answer.text.clone()));
                    conversation.push(ChatMessage::user(format!(
                        "That reply was rejected: {reason}\nReply with ONLY the required \
                         JSON object — no prose, no code fence, no commentary."
                    )));
                    failures.push(reason);
                }
            }
        }
        Err(format!(
            "{}: no usable answer after {} attempt{} ({})",
            self.label,
            attempts,
            if attempts == 1 { "" } else { "s" },
            failures.join("; ")
        ))
    }

    /// One provider round trip. Returns the raw answer even when it is truncated or
    /// empty: the callers above decide whether that is usable.
    async fn run(
        &self,
        model_ref: &str,
        messages: Vec<ChatMessage>,
        max_output_tokens: u64,
    ) -> Result<Answer, String> {
        if messages.is_empty() {
            return Err("nothing to send".into());
        }
        let req = ChatRequest {
            model: model_ref.to_string(),
            messages,
            // Tools-free by contract: a one-shot answer is text, never a tool call.
            tools: Vec::new(),
            max_output_tokens: Some(max_output_tokens),
            temperature: None,
            reasoning_effort: None,
        };
        let (tx, mut rx) = mpsc::channel::<AttemptEvent>(256);
        let mut runner = ComboRunner::new(self.registry.clone());
        runner.max_retries = self.max_retries;
        runner.allow_retry_after_partial_stream = self.allow_retry_after_partial_stream;
        if let Some(rotation) = &self.rotation {
            runner.rotation = rotation.clone();
        }
        let model = req.model.clone();
        let req_for_task = req.clone();
        // A dropped JoinHandle detaches: without the AbortTask wrapper below, a
        // cancelled or abandoned request would keep streaming in the background.
        let mut handle = crate::supervisor::AbortTask(tokio::spawn(async move {
            runner.run(&model, &req_for_task, tx).await
        }));
        let mut text = String::new();
        let mut reasoning_chars = 0usize;
        let mut retries = 0u32;
        let mut cancel_rx = self.cancel.clone();
        loop {
            let next = match &mut cancel_rx {
                Some(cancel) => tokio::select! {
                    event = rx.recv() => event,
                    _ = crate::cancel_changed(cancel) => return Err("cancelled".into()),
                },
                None => rx.recv().await,
            };
            let Some(event) = next else { break };
            if let Some(sink) = &self.on_attempt {
                sink(&event);
            }
            match event.event {
                ProviderEvent::TextDelta { text: delta } => text.push_str(&delta),
                ProviderEvent::ReasoningDelta { text: delta } => {
                    reasoning_chars += delta.chars().count();
                }
                // The retry produces a fresh answer: keep only the last attempt.
                ProviderEvent::Retrying { .. } => {
                    text.clear();
                    reasoning_chars = 0;
                    retries += 1;
                }
                _ => {}
            }
        }
        let outcome = match (&mut handle.0).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(format!("request task failed: {error}")),
        };
        let served = outcome
            .served
            .as_ref()
            .map(|m| format!("{}/{}", m.provider_id, m.model_id))
            .unwrap_or_else(|| model_ref.to_string());
        let trail = if outcome.trail.is_empty() {
            String::new()
        } else {
            let reasons = outcome
                .trail
                .iter()
                .map(|(member, reason)| format!("{member}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            format!(" [skipped: {reasons}]")
        };
        let finish = match &outcome.outcome {
            loon_events::RequestOutcome::Completed { finish } => finish.clone(),
            loon_events::RequestOutcome::Errored { error, .. } => {
                return Err(format!("{served}: {error}{trail}"));
            }
            loon_events::RequestOutcome::Cancelled => {
                return Err(format!("{served}: the request was cancelled"));
            }
            loon_events::RequestOutcome::Unknown { reason } => {
                return Err(format!(
                    "{served}: the request ended without a result ({reason}){trail}"
                ));
            }
        };
        Ok(Answer {
            truncated: finish.eq_ignore_ascii_case("length"),
            text,
            served,
            finish,
            usage: outcome.usage,
            usage_reported: outcome.usage_reported,
            reasoning_chars,
            retries,
            trail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loon_providers::{FinishReason, MockProvider, MockScript, MockStep, ProviderRegistry};
    use std::sync::Mutex;

    fn registry(scripts: Vec<MockScript>) -> Arc<ProviderRegistry> {
        Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            scripts,
        )))
    }

    fn user(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::user(text)]
    }

    fn accept_json(text: &str) -> Result<String, String> {
        if text.trim_start().starts_with('{') {
            Ok(text.to_string())
        } else {
            Err("expected a JSON object".to_string())
        }
    }

    #[tokio::test]
    async fn a_completed_answer_reports_what_served() {
        let answer = OneShot::new(registry(vec![MockScript::text("hello")]))
            .label("test")
            .request("mock", user("hi"))
            .await
            .expect("a text answer is usable");
        assert_eq!(answer.text, "hello");
        assert!(answer.served.contains("mock"), "got {}", answer.served);
        assert!(!answer.truncated);
    }

    #[tokio::test]
    async fn an_empty_answer_names_the_cause_instead_of_being_returned() {
        let err = OneShot::new(registry(vec![MockScript::new(vec![])]))
            .label("supervisor review")
            .request("mock", user("hi"))
            .await
            .unwrap_err();
        assert!(err.starts_with("supervisor review:"), "got: {err}");
        assert!(err.contains("no answer text"), "got: {err}");
    }

    #[tokio::test]
    async fn a_truncated_answer_is_a_failure_not_a_prefix() {
        let script = MockScript::with_finish(
            vec![MockStep::Text("{\"status\":\"cont".into())],
            FinishReason::Length,
        );
        let err = OneShot::new(registry(vec![script]))
            .label("supervisor review")
            .request("mock", user("hi"))
            .await
            .unwrap_err();
        assert!(err.contains("output limit"), "got: {err}");
        assert!(err.contains("incomplete"), "got: {err}");
    }

    #[tokio::test]
    async fn cancellation_ends_the_request_and_names_it() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let err = OneShot::new(registry(vec![MockScript::text("ignored")]))
            .label("supervisor review")
            .cancel(rx)
            .request("mock", user("hi"))
            .await
            .unwrap_err();
        assert!(err.contains("cancelled"), "got: {err}");
    }

    #[tokio::test]
    async fn an_empty_message_list_is_refused_before_sending() {
        let err = OneShot::new(registry(vec![]))
            .label("compaction summary")
            .request("mock", vec![])
            .await
            .unwrap_err();
        assert!(err.contains("nothing to send"), "got: {err}");
    }

    // ── parsed requests: the path behind "supervisor returned no JSON object" ─────

    #[tokio::test]
    async fn prose_is_repaired_into_a_valid_answer() {
        // The real-world failure: the reviewer answers in prose instead of JSON.
        let got = OneShot::new(registry(vec![
            MockScript::text("Looks fine to me, ship it."),
            MockScript::text(r#"{"status":"continue","reason":"more"}"#),
        ]))
        .label("supervisor review")
        .request_parsed("mock", user("review this"), accept_json)
        .await
        .expect("the repair round must recover a usable answer");
        assert_eq!(got.attempts, 2, "one rejection plus one repair");
        assert!(got.repaired());
        assert!(got.value.starts_with('{'));
    }

    #[tokio::test]
    async fn a_truncated_answer_escalates_the_budget_instead_of_being_parsed() {
        let got = OneShot::new(registry(vec![
            MockScript::with_finish(
                vec![MockStep::Text("{\"status\":\"con".into())],
                FinishReason::Length,
            ),
            MockScript::text(r#"{"status":"completed","reason":"done"}"#),
        ]))
        .label("supervisor review")
        .max_output_tokens(64)
        .token_ceiling(512)
        .request_parsed("mock", user("review this"), accept_json)
        .await
        .expect("the escalated attempt must succeed");
        assert_eq!(got.attempts, 2);
        assert!(got.value.contains("completed"));
    }

    #[tokio::test]
    async fn the_repair_round_shows_the_model_what_it_sent() {
        // Capture the requests the provider actually receives: the repair must carry
        // the rejected answer and the complaint, or the model just repeats itself.
        let mock = MockProvider::with_scripts(vec![
            MockScript::text("not json"),
            MockScript::text(r#"{"status":"continue","reason":"more"}"#),
        ]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock.clone()));
        OneShot::new(registry)
            .label("supervisor review")
            .request_parsed("mock", user("review"), accept_json)
            .await
            .expect("repaired");
        let sent = mock.requests();
        assert_eq!(sent.len(), 2, "two requests: the review and its repair");
        let repair = &sent[1].messages;
        assert!(
            repair
                .iter()
                .any(|m| m.role == loon_providers::Role::Assistant && m.text == "not json"),
            "the repair must quote the rejected answer: {repair:?}"
        );
        assert!(
            repair
                .iter()
                .any(|m| m.text.contains("rejected") && m.text.contains("JSON")),
            "the repair must state the requirement: {repair:?}"
        );
    }

    #[tokio::test]
    async fn an_exhausted_repair_budget_reports_every_reason() {
        let err = OneShot::new(registry(vec![
            MockScript::text("prose one"),
            MockScript::text("prose two"),
            MockScript::text("prose three"),
        ]))
        .label("supervisor review")
        .max_attempts(3)
        .request_parsed("mock", user("review"), accept_json)
        .await
        .unwrap_err();
        assert!(
            err.contains("no usable answer after 3 attempts"),
            "got: {err}"
        );
        assert_eq!(
            err.matches("expected a JSON object").count(),
            3,
            "each rejection must be reported, got: {err}"
        );
    }

    #[tokio::test]
    async fn a_parsed_answer_reports_its_usage() {
        let got = OneShot::new(registry(vec![MockScript::text("{}")]))
            .label("supervisor review")
            .request_parsed("mock", user("review"), accept_json)
            .await
            .unwrap();
        assert_eq!(got.attempts, 1);
        assert!(!got.repaired());
        assert!(got.answer.served.contains("mock"));
    }

    #[tokio::test]
    async fn a_validator_sees_only_usable_text() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        // Every attempt answers with nothing at all.
        let mock = MockProvider::new();
        mock.set_fallback(MockScript::new(vec![]));
        let err = OneShot::new(Arc::new(ProviderRegistry::with_mock(Arc::new(mock))))
            .label("supervisor review")
            .max_attempts(2)
            .request_parsed("mock", user("review"), move |text: &str| {
                sink.lock().expect("lock").push(text.to_string());
                accept_json(text)
            })
            .await
            .unwrap_err();
        assert!(
            seen.lock().expect("lock").is_empty(),
            "an empty answer must never reach the validator"
        );
        assert_eq!(
            err.matches("no answer text").count(),
            2,
            "both empty answers must be reported, got: {err}"
        );
    }
}
