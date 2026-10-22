//! Structural JSON-Schema validation.
//!
//! Supports the subset our tool schemas (and most MCP servers) use: `type` (single or
//! list), `properties`/`required`/`additionalProperties: false`, `items`, `enum`,
//! `minimum`/`maximum`, `minLength`/`maxLength`, `minItems`. This is deliberately *not*
//! full JSON Schema; unsupported keywords are ignored rather than enforced, and that
//! limitation is stated wherever validation is advertised.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaIssue {
    pub pointer: String,
    pub message: String,
}

impl std::fmt::Display for SchemaIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Root-level issues have no pointer; printing one yields a bare ": message".
        if self.pointer.is_empty() {
            return write!(f, "{}", self.message);
        }
        // `/command` reads better as `command` in a message the model must act on.
        write!(
            f,
            "{}: {}",
            self.pointer.trim_start_matches('/'),
            self.message
        )
    }
}

pub fn validate(instance: &Value, schema: &Value) -> Vec<SchemaIssue> {
    let mut issues = Vec::new();
    check(instance, schema, String::new(), &mut issues);
    issues
}

fn pointer_join(base: &str, seg: &str) -> String {
    if base.is_empty() {
        format!("/{seg}")
    } else {
        format!("{base}/{seg}")
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn matches_type(v: &Value, want: &str) -> bool {
    match want {
        "number" => matches!(type_name(v), "number" | "integer"),
        "integer" => matches!(type_name(v), "integer"),
        other => type_name(v) == other,
    }
}

fn check(instance: &Value, schema: &Value, ptr: String, issues: &mut Vec<SchemaIssue>) {
    // enum first: it constrains the whole value
    if let Some(enu) = schema.get("enum").and_then(Value::as_array) {
        if !enu.iter().any(|e| e == instance) {
            issues.push(SchemaIssue {
                message: format!(
                    "value {} not in enum [{}]",
                    serde_json::to_string(instance).unwrap_or_else(|_| "?".into()),
                    enu.iter()
                        .filter_map(|e| serde_json::to_string(e).ok())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                pointer: ptr.clone(),
            });
            return;
        }
    }
    match schema.get("type") {
        Some(Value::String(t)) => {
            if !matches_type(instance, t) {
                issues.push(SchemaIssue {
                    message: format!("expected {t}, got {}", type_name(instance)),
                    pointer: ptr.clone(),
                });
                return;
            }
        }
        Some(Value::Array(ts)) => {
            let ok = ts
                .iter()
                .filter_map(Value::as_str)
                .any(|t| matches_type(instance, t));
            if !ok {
                issues.push(SchemaIssue {
                    message: format!(
                        "type {} matches none of the allowed types",
                        type_name(instance)
                    ),
                    pointer: ptr.clone(),
                });
                return;
            }
        }
        _ => {}
    }
    match instance {
        Value::String(s) => {
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
                if (s.chars().count() as u64) < min {
                    issues.push(SchemaIssue {
                        message: format!("shorter than minLength {min}"),
                        pointer: ptr.clone(),
                    });
                }
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
                if (s.chars().count() as u64) > max {
                    issues.push(SchemaIssue {
                        message: format!("longer than maxLength {max}"),
                        pointer: ptr.clone(),
                    });
                }
            }
        }
        Value::Number(_) => {
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                if instance.as_f64().map(|v| v < min).unwrap_or(false) {
                    issues.push(SchemaIssue {
                        message: format!("less than minimum {min}"),
                        pointer: ptr.clone(),
                    });
                }
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                if instance.as_f64().map(|v| v > max).unwrap_or(false) {
                    issues.push(SchemaIssue {
                        message: format!("greater than maximum {max}"),
                        pointer: ptr,
                    });
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    issues.push(SchemaIssue {
                        message: format!("fewer than minItems {min}"),
                        pointer: ptr.clone(),
                    });
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(
                        item,
                        item_schema,
                        pointer_join(&ptr, &i.to_string()),
                        issues,
                    );
                }
            }
        }
        Value::Object(map) => {
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (k, sub) in props {
                    if let Some(v) = map.get(k) {
                        check(v, sub, pointer_join(&ptr, k), issues);
                    }
                }
                if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
                    for k in map.keys() {
                        if !props.contains_key(k) {
                            issues.push(SchemaIssue {
                                message: format!(
                                    "unknown field `{k}` (this tool accepts: {})",
                                    if props.is_empty() {
                                        "no parameters".to_string()
                                    } else {
                                        props.keys().cloned().collect::<Vec<_>>().join(", ")
                                    }
                                ),
                                pointer: ptr.clone(),
                            });
                        }
                    }
                }
            }
            if let Some(req) = schema.get("required").and_then(Value::as_array) {
                for r in req.iter().filter_map(Value::as_str) {
                    if !map.contains_key(r) {
                        issues.push(SchemaIssue {
                            message: format!("missing required field `{r}`"),
                            pointer: ptr.clone(),
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod message_tests {
    use super::*;
    use serde_json::json;

    fn messages(instance: Value, schema: Value) -> Vec<String> {
        validate(&instance, &schema)
            .iter()
            .map(|i| i.to_string())
            .collect()
    }

    #[test]
    fn root_level_issues_have_no_stray_colon() {
        let schema = json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false
        });
        let msgs = messages(json!({}), schema.clone());
        assert_eq!(msgs, vec!["missing required field `command`"]);
        assert!(
            !msgs[0].starts_with(':'),
            "a message the model must act on cannot start with a colon"
        );
    }

    #[test]
    fn an_unknown_field_names_the_accepted_ones() {
        let schema = json!({
            "type": "object",
            "properties": {"command": {"type": "string"}, "timeout_secs": {"type": "integer"}},
            "required": ["command"],
            "additionalProperties": false
        });
        let msgs = messages(json!({"wd": "/tmp", "command": "ls"}), schema);
        assert!(msgs[0].contains("unknown field `wd`"), "got {msgs:?}");
        assert!(
            msgs[0].contains("command") && msgs[0].contains("timeout_secs"),
            "the accepted fields are listed: {msgs:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_missing_required_and_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "n": {"type": "integer", "minimum": 1}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        let issues = validate(&json!({"n": "x", "extra": 1}), &schema);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("missing required field `path`")));
        assert!(issues
            .iter()
            .any(|i| i.pointer == "/n" && i.message.contains("expected integer")));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("unknown field `extra`")));
    }

    #[test]
    fn number_accepts_integer_and_integers_reject_floats() {
        assert!(validate(&json!(1.5), &json!({"type": "number"})).is_empty());
        assert!(!validate(&json!(1.5), &json!({"type": "integer"})).is_empty());
        assert!(validate(&json!(3), &json!({"type": "integer"})).is_empty());
    }

    #[test]
    fn array_items_are_checked() {
        let schema = json!({"type": "array", "minItems": 2, "items": {"type": "string"}});
        assert_eq!(validate(&json!(["a"]), &schema).len(), 1);
        assert_eq!(validate(&json!(["a", 1]), &schema).len(), 1);
        assert!(validate(&json!(["a", "b"]), &schema).is_empty());
    }

    #[test]
    fn enum_is_enforced() {
        assert!(validate(&json!("a"), &json!({"enum": ["a", "b"]})).is_empty());
        assert!(!validate(&json!("c"), &json!({"enum": ["a", "b"]})).is_empty());
    }
}
