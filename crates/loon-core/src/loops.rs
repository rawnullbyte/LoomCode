//! Loop tasks: recurring prompts (`/loop 1m check the deploy`).
//!
//! The registry lives here so both sides see the same state: the TUI driver polls
//! [`LoopRegistry::take_due`] each tick and submits due prompts through the normal
//! queue, and the `loop_cancel` tool lets the AGENT cancel tasks the user created.
//! Scheduling is wall-clock-naive (interval from last fire), which is the honest
//! behavior for an interactive session: pauses while a turn runs, catches up once.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct LoopJob {
    pub id: u64,
    pub interval: Duration,
    pub prompt: String,
    pub next_due: Instant,
}

#[derive(Default)]
pub struct LoopRegistry {
    jobs: Mutex<Vec<LoopJob>>,
    next_id: Mutex<u64>,
}

/// Parse `30s` / `5m` / `2h` / bare seconds.
pub fn parse_interval(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let secs = match unit {
        "s" => num.parse::<u64>().map_err(|_| "bad seconds")?,
        "m" => num
            .parse::<u64>()
            .map_err(|_| "bad minutes")?
            .checked_mul(60)
            .ok_or("interval overflow")?,
        "h" => num
            .parse::<u64>()
            .map_err(|_| "bad hours")?
            .checked_mul(3600)
            .ok_or("interval overflow")?,
        _ => s.parse::<u64>().map_err(|_| "bad seconds")?,
    };
    if secs == 0 {
        return Err("interval must be at least 1s".into());
    }
    Ok(Duration::from_secs(secs))
}

impl LoopRegistry {
    pub fn new() -> LoopRegistry {
        LoopRegistry::default()
    }

    pub fn create(&self, interval: Duration, prompt: String) -> u64 {
        let mut next_id = self.next_id.lock().unwrap();
        *next_id += 1;
        let id = *next_id;
        self.jobs.lock().unwrap().push(LoopJob {
            id,
            interval,
            prompt,
            next_due: Instant::now() + interval,
        });
        id
    }

    /// (id, interval, seconds until next fire, prompt), id order.
    pub fn list(&self) -> Vec<(u64, Duration, u64, String)> {
        let now = Instant::now();
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .map(|j| {
                let remaining = if j.next_due > now {
                    (j.next_due - now).as_secs()
                } else {
                    0
                };
                (j.id, j.interval, remaining, j.prompt.clone())
            })
            .collect()
    }

    /// Pop prompts whose time has come, rescheduling each for its next interval.
    pub fn take_due(&self) -> Vec<(u64, String)> {
        let mut jobs = self.jobs.lock().unwrap();
        let now = Instant::now();
        let mut due = Vec::new();
        for j in jobs.iter_mut() {
            if j.next_due <= now {
                due.push((j.id, j.prompt.clone()));
                j.next_due = now + j.interval;
            }
        }
        due
    }

    /// Cancel by id. True when a live job was removed.
    pub fn cancel(&self, id: u64) -> bool {
        let mut jobs = self.jobs.lock().unwrap();
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        jobs.len() != before
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_parses_units_and_bare_seconds() {
        assert_eq!(parse_interval("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_interval("1m"), Ok(Duration::from_secs(60)));
        assert_eq!(parse_interval("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_interval("90"), Ok(Duration::from_secs(90)));
        assert!(parse_interval("xm").is_err());
        assert!(parse_interval("0s").is_err());
    }

    #[test]
    fn create_take_due_cancel_round_trip() {
        let r = LoopRegistry::new();
        let id = r.create(Duration::from_millis(1), "check".into());
        std::thread::sleep(Duration::from_millis(5));
        let due = r.take_due();
        assert_eq!(due, vec![(id, "check".to_string())]);
        assert!(r.cancel(id));
        assert!(!r.cancel(id), "second cancel reports absence honestly");
        assert!(r.take_due().is_empty());
        assert!(r.is_empty());
    }
}
