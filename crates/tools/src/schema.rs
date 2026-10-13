//! Validation for the simple, flat tool schemas the runtime uses: one object
//! whose properties are strings, integers, numbers or booleans. Calls are
//! checked strictly so a malformed call never reaches a tool.

use serde_json::Value;

pub fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let args = args
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !args.contains_key(name) {
                return Err(format!("missing required argument `{name}`"));
            }
        }
    }

    for (name, value) in args {
        let Some(prop) = properties.get(name) else {
            return Err(format!("unknown argument `{name}`"));
        };
        let expected = prop.get("type").and_then(Value::as_str).unwrap_or("string");
        let ok = match expected {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            other => return Err(format!("schema for `{name}` uses unsupported type {other}")),
        };
        if !ok {
            return Err(format!("argument `{name}` must be a {expected}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "minutes": {"type": "integer"},
                "reason": {"type": "string"}
            },
            "required": ["minutes"]
        })
    }

    #[test]
    fn accepts_a_well_formed_call() {
        assert!(validate(&schema(), &json!({"minutes": 30, "reason": "later"})).is_ok());
    }

    #[test]
    fn rejects_missing_unknown_and_mistyped_arguments() {
        assert!(
            validate(&schema(), &json!({}))
                .unwrap_err()
                .contains("missing")
        );
        assert!(
            validate(&schema(), &json!({"minutes": 1, "extra": true}))
                .unwrap_err()
                .contains("unknown")
        );
        assert!(
            validate(&schema(), &json!({"minutes": "soon"}))
                .unwrap_err()
                .contains("integer")
        );
        assert!(validate(&schema(), &json!("minutes")).is_err());
    }
}
