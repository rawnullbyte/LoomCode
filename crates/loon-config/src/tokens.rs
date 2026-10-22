//! Human token sizes: parse `1m` / `100k` / `1.5k` / `90%` and render `1M` / `128k`.
//!
//! Context-window settings are typed by people, not scripts, so both directions are
//! shared: every place that accepts a token count (`/context`, `/autocompact`,
//! `/config`, the settings editor) parses through [`parse_tokens`], and every place that
//! shows one uses [`format_tokens`] so a 1,000,000-token window reads `1M` instead of
//! `1000k`.

/// A context setting: either an absolute token count or a percentage of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSpec {
    Tokens(u64),
    /// Percent of the context window (1–99); follows the window when it changes.
    Percent(u8),
}

/// Largest accepted window/threshold, in tokens.
pub const MAX_TOKENS: u64 = 10_000_000;

/// Parse a token count: `1000000`, `1m`, `1M`, `100k`, `1.5k`, `1_000_000`, `1,000,000`.
/// Fractional multipliers round to the nearest token. Returns None for junk or for values
/// outside `0..=MAX_TOKENS`.
pub fn parse_tokens(input: &str) -> Option<u64> {
    let cleaned: String = input
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, '_' | ',' | ' '))
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    // Tolerate "1m tokens" / "100ktok": the unit word is redundant with the suffix.
    let cleaned = cleaned
        .trim_end_matches("tokens")
        .trim_end_matches("token")
        .trim_end_matches("tok")
        .trim_end_matches('t');
    if cleaned.is_empty() {
        return None;
    }
    let (number, multiplier) = if let Some(n) = cleaned.strip_suffix('m') {
        (n, 1_000_000f64)
    } else if let Some(n) = cleaned.strip_suffix('k') {
        (n, 1_000f64)
    } else {
        (cleaned, 1f64)
    };
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let tokens = value * multiplier;
    if tokens > MAX_TOKENS as f64 {
        return None;
    }
    Some(tokens.round() as u64)
}

/// Parse a percentage of the window: `90%` or `90 %`. Accepted range is 1–99: a threshold
/// at or above 100% can never compact before the window is already full.
pub fn parse_percent(input: &str) -> Option<u8> {
    let raw = input.trim().strip_suffix('%')?;
    raw.trim()
        .parse::<u8>()
        .ok()
        .filter(|p| (1..=99).contains(p))
}

/// Parse either an absolute size or a percentage (see [`TokenSpec`]).
pub fn parse_token_spec(input: &str) -> Option<TokenSpec> {
    if input.trim().ends_with('%') {
        return parse_percent(input).map(TokenSpec::Percent);
    }
    parse_tokens(input).map(TokenSpec::Tokens)
}

/// Render a token count compactly: `1M`, `1.5M`, `128k`, `12.4k`, `1.024k`, `950`.
///
/// Integral multiples drop the decimal (`128k`, not `128.0k`), so a 1,000,000-token
/// window is `1M` rather than the old `1000k`. Values that are not exact multiples keep
/// up to three decimals so the displayed number still parses back to the same count
/// (`1024` → `1.024k`, never a misleading `1k`).
pub fn format_tokens(tokens: u64) -> String {
    fn scaled(value: f64, suffix: &str) -> String {
        let rounded = value.round();
        if (value - rounded).abs() < 0.0005 {
            return format!("{}{suffix}", rounded as u64);
        }
        let mut text = format!("{value:.3}");
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
        format!("{text}{suffix}")
    }
    if tokens >= 1_000_000 {
        scaled(tokens as f64 / 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        scaled(tokens as f64 / 1_000.0, "k")
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_suffixed_counts() {
        assert_eq!(parse_tokens("1000000"), Some(1_000_000));
        assert_eq!(parse_tokens("1m"), Some(1_000_000));
        assert_eq!(parse_tokens("1M"), Some(1_000_000));
        assert_eq!(parse_tokens("100k"), Some(100_000));
        assert_eq!(parse_tokens("1.5k"), Some(1_500));
        assert_eq!(parse_tokens("1_000_000"), Some(1_000_000));
        assert_eq!(parse_tokens("1,000,000"), Some(1_000_000));
        assert_eq!(parse_tokens("128 k"), Some(128_000));
        assert_eq!(parse_tokens("200ktokens"), Some(200_000));
        assert_eq!(parse_tokens("0"), Some(0));
        assert_eq!(parse_tokens("k"), None);
        assert_eq!(parse_tokens("1x"), None);
        assert_eq!(parse_tokens("11m"), None, "above the supported maximum");
        assert_eq!(parse_tokens("-5"), None);
    }

    #[test]
    fn parses_percentages_and_specs() {
        assert_eq!(parse_percent("90%"), Some(90));
        assert_eq!(parse_percent("90 %"), Some(90));
        assert_eq!(parse_percent("100%"), None);
        assert_eq!(parse_percent("0%"), None);
        assert_eq!(parse_token_spec("90%"), Some(TokenSpec::Percent(90)));
        assert_eq!(parse_token_spec("1m"), Some(TokenSpec::Tokens(1_000_000)));
        assert_eq!(parse_token_spec("nope"), None);
    }

    #[test]
    fn formats_with_m_and_k_units() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(1_024), "1.024k");
        assert_eq!(format_tokens(12_400), "12.4k");
        assert_eq!(format_tokens(100_000), "100k");
        assert_eq!(format_tokens(128_000), "128k");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(10_000_000), "10M");
    }

    #[test]
    fn format_and_parse_round_trip() {
        for n in [0, 1_024, 12_400, 128_000, 200_000, 1_000_000, 500] {
            assert_eq!(parse_tokens(&format_tokens(n)), Some(n), "round trip {n}");
        }
    }
}
