//! Editable FIFO shared by the UI driver and the inference request loop.

#[derive(Debug, Clone)]
pub struct PromptQueue {
    items: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    capacity: usize,
}

impl Default for PromptQueue {
    fn default() -> Self {
        Self::new(16)
    }
}

impl PromptQueue {
    pub fn new(capacity: usize) -> PromptQueue {
        PromptQueue {
            items: Default::default(),
            capacity: capacity.max(1),
        }
    }

    /// Enqueue a prompt. Returns Err(text) with a reason when the queue is full —
    /// the caller shows it, nothing is silently dropped.
    pub fn push(&self, text: String) -> Result<usize, String> {
        let mut items = self.items.lock().unwrap();
        if items.len() >= self.capacity {
            return Err(format!(
                "prompt queue is full ({}) — wait for the current turn to finish",
                self.capacity
            ));
        }
        items.push(text);
        Ok(items.len())
    }

    /// Next prompt to run, or None when idle-queued work remains none.
    pub fn pop(&self) -> Option<String> {
        let mut items = self.items.lock().unwrap();
        if items.is_empty() {
            None
        } else {
            Some(items.remove(0))
        }
    }

    /// Take everything queued, in order. Used to deliver all pending prompts with the
    /// next request instead of one per turn boundary.
    pub fn drain_all(&self) -> Vec<String> {
        std::mem::take(&mut *self.items.lock().unwrap())
    }

    /// Most recently queued prompt, for "Up to edit" — the rest stay FIFO.
    pub fn remove_last(&self) -> Option<String> {
        self.items.lock().unwrap().pop()
    }

    /// A copy for the UI's queue mirror.
    pub fn snapshot(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_is_fifo() {
        let q = PromptQueue::new(4);
        assert_eq!(q.push("one".into()).unwrap(), 1);
        assert_eq!(q.push("two".into()).unwrap(), 2);
        assert_eq!(q.pop().as_deref(), Some("one"));
        assert_eq!(q.pop().as_deref(), Some("two"));
        assert!(q.pop().is_none());
    }

    #[test]
    fn full_queue_refuses_with_reason_not_silence() {
        let q = PromptQueue::new(1);
        q.push("first".into()).unwrap();
        let err = q.push("second".into()).unwrap_err();
        assert!(err.contains("full"));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn remove_last_pops_only_the_newest_and_keeps_fifo_order() {
        let q = PromptQueue::new(4);
        q.push("one".into()).unwrap();
        q.push("two".into()).unwrap();
        q.push("three".into()).unwrap();
        assert_eq!(q.remove_last().as_deref(), Some("three"));
        assert_eq!(q.snapshot(), vec!["one", "two"]);
        assert_eq!(q.pop().as_deref(), Some("one"));
        assert_eq!(q.pop().as_deref(), Some("two"));
        assert_eq!(q.remove_last(), None);
        assert!(q.is_empty());
    }
}
