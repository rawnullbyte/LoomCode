//! One generation-throughput meter per provider request. Reasoning, answer text
//! and streamed tool arguments share the estimate; usage replaces it, never adds.
use std::time::{Duration, Instant};
#[derive(Default)]
pub struct RequestRate {
    started: Option<Instant>,
    finished: Option<Instant>,
    chars: u64,
    reported_tokens: Option<u64>,
}
impl RequestRate {
    pub fn start(&mut self) {
        self.start_at(Instant::now());
    }
    pub fn start_at(&mut self, now: Instant) {
        *self = Self {
            started: Some(now),
            ..Default::default()
        };
    }
    pub fn add_chars(&mut self, chars: u64) {
        if self.started.is_some() && self.finished.is_none() {
            self.chars = self.chars.saturating_add(chars);
        }
    }
    /// OpenAI-compatible output/completion usage includes reasoning tokens.
    pub fn report(&mut self, tokens: u64) {
        self.reported_tokens = Some(tokens);
    }
    pub fn finish(&mut self) {
        self.finish_at(Instant::now());
    }
    pub fn finish_at(&mut self, now: Instant) {
        if self.started.is_some() && self.finished.is_none() {
            self.finished = Some(now);
        }
    }
    pub fn tokens(&self) -> u64 {
        self.reported_tokens.unwrap_or(self.chars.div_ceil(4))
    }
    pub fn is_estimate(&self) -> bool {
        self.reported_tokens.is_none()
    }
    pub fn elapsed(&self) -> Duration {
        self.elapsed_at(Instant::now())
    }
    fn elapsed_at(&self, now: Instant) -> Duration {
        self.started
            .map(|t| self.finished.unwrap_or(now).saturating_duration_since(t))
            .unwrap_or_default()
    }
    pub fn per_sec(&self) -> f64 {
        self.tokens() as f64 / self.elapsed().as_secs_f64().max(0.001)
    }
    pub fn label(&self) -> String {
        format!(
            "{}{:.1} t/s",
            if self.is_estimate() { "~" } else { "" },
            self.per_sec()
        )
    }
    pub fn active(&self) -> bool {
        self.started.is_some()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combines_phases_without_double_counting_usage_and_freezes_between_requests() {
        let now = Instant::now();
        let mut meter = RequestRate::default();
        meter.start_at(now);
        meter.add_chars(800);
        meter.add_chars(400); // thinking + writing
        assert_eq!(meter.tokens(), 300);
        assert!(meter.is_estimate());
        meter.report(360);
        assert_eq!(meter.tokens(), 360);
        assert!(!meter.is_estimate());
        meter.finish_at(now + Duration::from_secs(3));
        assert_eq!(meter.per_sec(), 120.0);
        assert_eq!(
            meter.elapsed_at(now + Duration::from_secs(90)),
            Duration::from_secs(3)
        );
        meter.start_at(now + Duration::from_secs(90));
        assert_eq!(meter.tokens(), 0);
        assert!(meter.is_estimate());
    }
}
