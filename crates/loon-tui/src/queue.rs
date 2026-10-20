//! Prompt queue: prompts typed while a turn runs are queued (with visible position)
//! and submitted automatically when the turn finishes, instead of being dropped.

#[derive(Debug, Default)]
pub struct PromptQueue {
    items: Vec<String>,
    capacity: usize,
}

impl PromptQueue {
    pub fn new(capacity: usize) -> PromptQueue {
        PromptQueue {
            items: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    /// Enqueue a prompt. Returns Err(text) with a reason when the queue is full —
    /// the caller shows it, nothing is silently dropped.
    pub fn push(&mut self, text: String) -> Result<usize, String> {
        if self.items.len() >= self.capacity {
            return Err(format!(
                "prompt queue is full ({}) — wait for the current turn to finish",
                self.capacity
            ));
        }
        self.items.push(text);
        Ok(self.items.len())
    }

    /// Next prompt to run, or None when idle-queued work remains none.
    pub fn pop(&mut self) -> Option<String> {
        if self.items.is_empty() {
            None
        } else {
            Some(self.items.remove(0))
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_is_fifo() {
        let mut q = PromptQueue::new(4);
        assert_eq!(q.push("one".into()).unwrap(), 1);
        assert_eq!(q.push("two".into()).unwrap(), 2);
        assert_eq!(q.pop().as_deref(), Some("one"));
        assert_eq!(q.pop().as_deref(), Some("two"));
        assert!(q.pop().is_none());
    }

    #[test]
    fn full_queue_refuses_with_reason_not_silence() {
        let mut q = PromptQueue::new(1);
        q.push("first".into()).unwrap();
        let err = q.push("second".into()).unwrap_err();
        assert!(err.contains("full"));
        assert_eq!(q.len(), 1);
    }
}
