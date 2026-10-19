//! Deterministic mock provider — a real `Provider` implementation driven by scripted
//! steps. It powers the offline test harness and lets Loon run with no network at all.
//! It is explicitly *not* a live provider and is never reported as one.

use crate::{FinishReason, ProviderError, ProviderEvent, ToolCallReq};
use loon_events::Usage;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum MockStep {
    Text(String),
    Reasoning(String),
    /// Emitted as a display fragment, then as a completed call (mock guarantees valid args).
    ToolCall {
        id: String,
        name: String,
        args_json: String,
    },
    Usage(Usage),
    /// Stream some text, then fail — exercises partial-stream handling.
    FailMidStream {
        text: String,
        error: ProviderError,
    },
    /// Fail before emitting anything.
    FailImmediate(ProviderError),
    /// Terminate without any completion event (dropped connection).
    Drop,
}

impl MockStep {
    pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> MockStep {
        MockStep::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args_json: args.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MockScript {
    pub steps: Vec<MockStep>,
    pub finish: FinishReason,
}

impl MockScript {
    pub fn new(steps: Vec<MockStep>) -> MockScript {
        MockScript {
            steps,
            finish: FinishReason::Stop,
        }
    }
    pub fn with_finish(steps: Vec<MockStep>, finish: FinishReason) -> MockScript {
        MockScript { steps, finish }
    }
    pub fn text(s: impl Into<String>) -> MockScript {
        MockScript::new(vec![MockStep::Text(s.into())])
    }
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<MockScript>,
    fallback: Option<MockScript>,
}

#[derive(Default)]
pub struct MockProvider {
    inner: Mutex<Inner>,
}

impl MockProvider {
    pub fn new() -> MockProvider {
        MockProvider::default()
    }

    pub fn with_scripts(scripts: Vec<MockScript>) -> Arc<Self> {
        let p = MockProvider::default();
        {
            let mut q = p.inner.lock().expect("mock lock");
            for s in scripts {
                q.queue.push_back(s);
            }
        }
        Arc::new(p)
    }

    pub fn push_script(&self, s: MockScript) {
        self.inner.lock().expect("mock lock").queue.push_back(s);
    }

    pub fn set_fallback(&self, s: MockScript) {
        self.inner.lock().expect("mock lock").fallback = Some(s);
    }

    fn take_script(&self) -> MockScript {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.queue.pop_front().unwrap_or_else(|| {
            inner
                .fallback
                .clone()
                .unwrap_or_else(|| MockScript::text("Hello from the Loon mock provider."))
        })
    }

    /// Stream the next scripted response as real provider events.
    pub async fn stream(
        &self,
        _req: &crate::ChatRequest,
        tx: &mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        let script = self.take_script();
        for step in script.steps {
            match step {
                MockStep::Text(t) => {
                    let _ = tx.send(ProviderEvent::TextDelta { text: t }).await;
                }
                MockStep::Reasoning(t) => {
                    let _ = tx.send(ProviderEvent::ReasoningDelta { text: t }).await;
                }
                MockStep::ToolCall {
                    id,
                    name,
                    args_json,
                } => {
                    let _ = tx
                        .send(ProviderEvent::ToolCallDelta {
                            index: 0,
                            name: Some(name.clone()),
                            args_delta: String::new(),
                        })
                        .await;
                    // Mock guarantees the args parse; emit as complete.
                    let _ = tx
                        .send(ProviderEvent::ToolCallComplete {
                            call: ToolCallReq {
                                id,
                                name,
                                arguments_json: args_json,
                            },
                        })
                        .await;
                }
                MockStep::Usage(u) => {
                    let _ = tx.send(ProviderEvent::Usage(u)).await;
                }
                MockStep::FailMidStream { text, error } => {
                    let _ = tx.send(ProviderEvent::TextDelta { text }).await;
                    let _ = tx.send(ProviderEvent::Failure { error }).await;
                    return Ok(());
                }
                MockStep::FailImmediate(error) => {
                    let _ = tx.send(ProviderEvent::Failure { error }).await;
                    return Ok(());
                }
                MockStep::Drop => {
                    return Ok(()); // no completion, no failure: dropped stream
                }
            }
        }
        let _ = tx
            .send(ProviderEvent::Finished {
                finish: script.finish,
            })
            .await;
        Ok(())
    }
}

/// Convenience constructors used across the test harness.
pub fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cached_input_tokens: None,
        reasoning_tokens: None,
    }
}

pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> MockStep {
    MockStep::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args_json: args.to_string(),
    }
}

pub fn read_tool_call(path: &str) -> MockStep {
    tool_call("call_read_1", "read", json!({"path": path}))
}
