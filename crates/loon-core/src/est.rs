//! Token estimation. Deliberately honest: this is a heuristic (~4 chars/token) used for
//! budgets and context display. Where the provider reports real usage, the provider
//! number is authoritative and replaces the estimate on the request record.

use loon_providers::ChatMessage;

pub fn estimate_text(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(4)
}

pub fn estimate_messages(msgs: &[ChatMessage]) -> u64 {
    let mut n: u64 = 8; // per-message framing overhead, roughly
    for m in msgs {
        n += estimate_text(&m.text) + 8;
        for c in &m.tool_calls {
            n += estimate_text(&c.arguments_json) + estimate_text(&c.name) + 16;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_is_monotonic_and_sane() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("abcd"), 1);
        assert_eq!(estimate_text("abcde"), 2, "rounds up");
        let msgs = vec![ChatMessage::user("x".repeat(400))];
        let est = estimate_messages(&msgs);
        assert!((100..=130).contains(&est), "sane ballpark, got {est}");
    }
}
