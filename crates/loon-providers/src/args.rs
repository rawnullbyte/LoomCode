//! Lenient tool-argument recovery.
//!
//! Compatible endpoints routinely emit arguments that are not strict JSON: Markdown
//! fences, trailing commas, single quotes, unquoted keys, or a sentence wrapped around
//! the object. One bad call must not kill a turn, so [`normalize_tool_arguments`] returns
//! the best usable container string (or `None` when nothing is recoverable); callers
//! substitute `{}` in the `None` case and let the model retry.

use serde_json::Value;

/// Wire `arguments` may arrive as a string or, from some gateways, as an already-decoded
/// object/array. Normalize both to the string form [`crate::ToolCallReq`] carries.
pub(crate) fn arguments_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) | Value::Array(_) => serde_json::to_string(v).ok(),
        _ => None,
    }
}

/// Return a parseable JSON object/array for `raw`, repairing common model slop.
pub(crate) fn normalize_tool_arguments(raw: &str) -> Option<String> {
    let trimmed = strip_fence(raw).trim();
    if trimmed.is_empty() {
        return Some("{}".into());
    }
    if parses_as_container(trimmed) {
        return Some(trimmed.to_string());
    }
    // Most prose-wrapped arguments contain exactly one balanced object.
    if let Some(obj) = first_balanced_object(trimmed) {
        if parses_as_container(obj) {
            return Some(obj.to_string());
        }
        if obj.len() != trimmed.len() {
            let repaired = repair(obj);
            if parses_as_container(&repaired) {
                return Some(canonical(&repaired));
            }
        }
    }
    let repaired = repair(trimmed);
    parses_as_container(&repaired).then(|| canonical(&repaired))
}

fn parses_as_container(s: &str) -> bool {
    matches!(
        serde_json::from_str::<Value>(s),
        Ok(Value::Object(_)) | Ok(Value::Array(_))
    )
}

fn canonical(s: &str) -> String {
    serde_json::to_string(&serde_json::from_str::<Value>(s).expect("validated JSON"))
        .unwrap_or_else(|_| s.to_string())
}

/// Strip a surrounding Markdown code fence (with or without a language tag).
fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = match rest.split_once('\n') {
        Some((_, body)) => body,
        None => {
            let r = rest.trim_start();
            r.strip_prefix("json")
                .or_else(|| r.strip_prefix("JSON"))
                .unwrap_or(r)
        }
    };
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
}

/// First balanced `{...}` substring, ignoring braces inside quoted strings.
fn first_balanced_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut quote = b'"';
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == quote {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => {
                in_str = true;
                quote = b;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Repair single-quoted strings, unquoted keys, and trailing commas.
fn repair(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    let mut in_str = false;
    let mut quote = '"';
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    if quote == '\'' && next == '\'' {
                        out.push('\'');
                    } else {
                        out.push('\\');
                        out.push(next);
                    }
                    i += 2;
                    continue;
                }
                out.push('\\');
                i += 1;
                continue;
            }
            if c == quote {
                out.push('"');
                in_str = false;
                i += 1;
                continue;
            }
            if quote == '\'' && c == '"' {
                out.push_str("\\\"");
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                quote = '"';
                out.push('"');
            }
            '\'' => {
                in_str = true;
                quote = '\'';
                out.push('"');
            }
            '}' | ']' => {
                strip_trailing_comma(&mut out);
                out.push(c);
            }
            _ => out.push(c),
        }
        i += 1;
    }
    quote_keys(&out)
}

fn strip_trailing_comma(out: &mut String) {
    out.truncate(out.trim_end().len());
    if out.ends_with(',') {
        out.pop();
    }
}

/// Quote bare identifiers that are followed by `:`, outside strings.
fn quote_keys(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 2;
                    continue;
                }
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            let start = i;
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || matches!(chars[j], '_' | '-' | '$' | '.'))
            {
                j += 1;
            }
            let ident: String = chars[start..j].iter().collect();
            let mut k = j;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            if k < chars.len() && chars[k] == ':' {
                out.push('"');
                out.push_str(&ident);
                out.push('"');
            } else {
                out.push_str(&ident);
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(raw: &str) -> Value {
        serde_json::from_str(&normalize_tool_arguments(raw).expect("recoverable")).unwrap()
    }

    #[test]
    fn strict_json_is_preserved() {
        assert_eq!(
            normalize_tool_arguments(r#"{"path": "a"}"#).as_deref(),
            Some(r#"{"path": "a"}"#)
        );
    }

    #[test]
    fn empty_becomes_object() {
        assert_eq!(normalize_tool_arguments("   ").as_deref(), Some("{}"));
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(
            parsed("```json\n{\"a\":1}\n```"),
            serde_json::json!({"a":1})
        );
        assert_eq!(parsed("```json {\"a\":1}```"), serde_json::json!({"a":1}));
    }

    #[test]
    fn trailing_commas_are_removed() {
        assert_eq!(
            parsed("{\"a\":[1,2,],\"b\":3,}"),
            serde_json::json!({"a":[1,2],"b":3})
        );
    }

    #[test]
    fn single_quotes_and_bare_keys() {
        assert_eq!(
            parsed("{path: 'src/a.rs'}"),
            serde_json::json!({"path":"src/a.rs"})
        );
        assert_eq!(
            parsed("{'path': 'a\\'b'}"),
            serde_json::json!({"path":"a'b"})
        );
    }

    #[test]
    fn prose_around_object_is_ignored() {
        assert_eq!(
            parsed("Sure, here you go:\n{\"path\":\"a\"}\nAnything else?"),
            serde_json::json!({"path":"a"})
        );
        assert_eq!(
            parsed("Use {\"path\":\"a}\"} please"),
            serde_json::json!({"path":"a}"})
        );
    }

    #[test]
    fn unrecoverable_is_none() {
        assert_eq!(normalize_tool_arguments("{not json"), None);
        assert_eq!(normalize_tool_arguments("hello"), None);
    }
}
