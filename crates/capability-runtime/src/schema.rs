//! Minimal JSON Schema validation.
//!
//! We validate the subset the runtime actually publishes: type, required, properties,
//! additionalProperties, enum, items, minimum/maximum, minLength/maxLength, minItems/maxItems.
//! Anything outside the subset is ignored, which is the safe direction for a v1: it never
//! accepts malformed data for the constructs we do declare.

use agentos_core::error::{Result, RuntimeError};
use serde_json::Value;

pub fn validate(schema: &Value, instance: &Value, path: &str) -> Result<()> {
    if schema.is_null() {
        return Ok(());
    }
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };

    // type
    if let Some(t) = obj.get("type") {
        let ok = match t {
            Value::String(s) => type_matches(s, instance),
            Value::Array(arr) => arr.iter().any(|v| v.as_str().map(|s| type_matches(s, instance)).unwrap_or(false)),
            _ => true,
        };
        if !ok {
            return Err(fail(path, format!("expected type {}, got {}", t, kind_of(instance))));
        }
    }

    // enum
    if let Some(Value::Array(options)) = obj.get("enum") {
        if !options.iter().any(|o| o == instance) {
            return Err(fail(path, format!("value {instance} is not one of the allowed options")));
        }
    }

    match instance {
        Value::Object(map) => {
            if let Some(Value::Array(required)) = obj.get("required") {
                for r in required.iter().filter_map(|v| v.as_str()) {
                    if !map.contains_key(r) {
                        return Err(fail(format!("{path}.{r}"), "required property is missing"));
                    }
                }
            }
            if let Some(Value::Object(props)) = obj.get("properties") {
                for (k, sub) in props {
                    if let Some(v) = map.get(k) {
                        validate(sub, v, &format!("{path}.{k}"))?;
                    }
                }
                if obj.get("additionalProperties").and_then(|v| v.as_bool()) == Some(false) {
                    for k in map.keys() {
                        if !props.contains_key(k) {
                            return Err(fail(format!("{path}.{k}"), "additional properties are not allowed"));
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = obj.get("minItems").and_then(|v| v.as_u64()) {
                if (items.len() as u64) < min {
                    return Err(fail(path, format!("expected at least {min} items")));
                }
            }
            if let Some(max) = obj.get("maxItems").and_then(|v| v.as_u64()) {
                if (items.len() as u64) > max {
                    return Err(fail(path, format!("expected at most {max} items")));
                }
            }
            if let Some(sub) = obj.get("items") {
                for (i, v) in items.iter().enumerate() {
                    validate(sub, v, &format!("{path}[{i}]"))?;
                }
            }
        }
        Value::String(s) => {
            if let Some(min) = obj.get("minLength").and_then(|v| v.as_u64()) {
                if (s.chars().count() as u64) < min {
                    return Err(fail(path, format!("string is shorter than {min}")));
                }
            }
            if let Some(max) = obj.get("maxLength").and_then(|v| v.as_u64()) {
                if (s.chars().count() as u64) > max {
                    return Err(fail(path, format!("string is longer than {max}")));
                }
            }
        }
        Value::Number(n) => {
            if let Some(min) = obj.get("minimum").and_then(|v| v.as_f64()) {
                if n.as_f64().unwrap_or(f64::NAN) < min {
                    return Err(fail(path, format!("number is below the minimum {min}")));
                }
            }
            if let Some(max) = obj.get("maximum").and_then(|v| v.as_f64()) {
                if n.as_f64().unwrap_or(f64::NAN) > max {
                    return Err(fail(path, format!("number is above the maximum {max}")));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn fail(path: impl Into<String>, message: impl Into<String>) -> RuntimeError {
    let path = path.into();
    let message = message.into();
    RuntimeError::invalid_input(format!("schema violation at {path}: {message}"))
        .with_detail("violation", message)
}

fn type_matches(expected: &str, instance: &Value) -> bool {
    match expected {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => true,
    }
}

fn kind_of(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_required_property_is_rejected() {
        let schema = json!({"type":"object","required":["text"],"properties":{"text":{"type":"string"}}});
        assert!(validate(&schema, &json!({}), "input").is_err());
        assert!(validate(&schema, &json!({"text":"hi"}), "input").is_ok());
    }

    #[test]
    fn wrong_type_is_rejected() {
        let schema = json!({"type":"object","properties":{"n":{"type":"number"}}});
        assert!(validate(&schema, &json!({"n":"x"}), "input").is_err());
    }

    #[test]
    fn additional_properties_can_be_forbidden() {
        let schema = json!({"type":"object","additionalProperties":false,"properties":{"a":{"type":"string"}}});
        assert!(validate(&schema, &json!({"a":"1","b":"2"}), "input").is_err());
        assert!(validate(&schema, &json!({"a":"1"}), "input").is_ok());
    }

    #[test]
    fn numeric_bounds_are_enforced() {
        let schema = json!({"type":"object","properties":{"n":{"type":"number","minimum":0,"maximum":10}}});
        assert!(validate(&schema, &json!({"n":11}), "input").is_err());
        assert!(validate(&schema, &json!({"n":-1}), "input").is_err());
    }

    #[test]
    fn nested_arrays_are_validated() {
        let schema = json!({"type":"object","properties":{"xs":{"type":"array","items":{"type":"number"}}}});
        assert!(validate(&schema, &json!({"xs":[1,"two"]}), "input").is_err());
        assert!(validate(&schema, &json!({"xs":[1,2]}), "input").is_ok());
    }
}
