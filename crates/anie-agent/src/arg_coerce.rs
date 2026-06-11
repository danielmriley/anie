//! Schema-guided coercion of tool-call arguments.
//!
//! Small local models frequently emit tool calls whose
//! arguments are *semantically* right but *structurally* wrong
//! in mechanically fixable ways: the whole argument object
//! JSON-encoded as a string, numbers or booleans as strings, a
//! bare scalar where a one-element array is expected. Strict
//! validation turns each of those into a failed turn; fixing
//! them in code is free.
//!
//! [`coerce_arguments`] applies only coercions that are
//! **unambiguous** (the target schema declares exactly one
//! type) and **lossless** (the value round-trips exactly —
//! `"4.5"` never becomes the integer 4). Anything it can't fix
//! that way is left untouched for the normal validation error
//! path.
//!
//! Plan 01 PR 1 of `docs/local_model_augmentation/`.

use serde_json::Value;

/// Attempt safe, schema-guided coercions on tool-call
/// arguments. Returns the (possibly rewritten) value and a
/// list of human-readable coercion notes; an empty list means
/// the arguments were returned untouched.
///
/// The caller is expected to invoke this only after strict
/// validation has already failed, and to re-validate the
/// returned value — coercion itself makes no promise the
/// result satisfies the schema.
#[must_use]
pub fn coerce_arguments(schema: &Value, args: Value) -> (Value, Vec<String>) {
    let mut notes = Vec::new();

    // 1. Whole-args string that parses as a JSON object, where
    //    the schema root expects an object.
    let mut args = if let (Some("object"), Value::String(raw)) = (single_type(schema), &args) {
        match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => {
                notes.push("parsed string-encoded arguments into a JSON object".to_string());
                parsed
            }
            _ => args,
        }
    } else {
        args
    };

    // 2.–4. Per-property coercions, one level deep.
    if let (Value::Object(map), Some(properties)) = (
        &mut args,
        schema.get("properties").and_then(Value::as_object),
    ) {
        for (key, value) in map.iter_mut() {
            let Some(prop_schema) = properties.get(key) else {
                continue;
            };
            if let Some(note) = coerce_property(prop_schema, value) {
                notes.push(format!("{key}: {note}"));
            }
        }
    }

    (args, notes)
}

/// Coerce a single property value toward its schema type.
/// Returns a note describing the coercion, or `None` if the
/// value was left untouched.
fn coerce_property(prop_schema: &Value, value: &mut Value) -> Option<String> {
    let target = single_type(prop_schema)?;
    match (target, &*value) {
        // String -> integer, only when the parse is exact
        // (i64 parsing rejects "4.5", "1e3", and overflow).
        ("integer", Value::String(s)) => {
            let parsed: i64 = s.trim().parse().ok()?;
            let note = format!("coerced string {s:?} to integer {parsed}");
            *value = Value::from(parsed);
            Some(note)
        }
        // String -> number, any finite f64.
        ("number", Value::String(s)) => {
            let parsed: f64 = s.trim().parse().ok()?;
            let number = serde_json::Number::from_f64(parsed)?;
            let note = format!("coerced string {s:?} to number {parsed}");
            *value = Value::Number(number);
            Some(note)
        }
        // String -> boolean. Case-insensitive: Python-trained
        // models emit "True"/"False"; still unambiguous.
        ("boolean", Value::String(s)) => {
            let parsed = match s.trim().to_ascii_lowercase().as_str() {
                "true" => true,
                "false" => false,
                _ => return None,
            };
            let note = format!("coerced string {s:?} to boolean {parsed}");
            *value = Value::Bool(parsed);
            Some(note)
        }
        // String-encoded object/array -> parsed, when the
        // parsed type matches the schema type.
        ("object", Value::String(s)) => {
            let parsed: Value = serde_json::from_str(s).ok()?;
            parsed.is_object().then(|| {
                *value = parsed;
                "parsed string-encoded value into an object".to_string()
            })
        }
        ("array", Value::String(s)) => {
            // Try a string-encoded array first; otherwise fall
            // through to scalar wrapping below.
            if let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(s) {
                *value = parsed;
                return Some("parsed string-encoded value into an array".to_string());
            }
            wrap_scalar_in_array(prop_schema, value)
        }
        // Scalar -> one-element array, when the scalar matches
        // the declared items type.
        ("array", _) if !value.is_array() => wrap_scalar_in_array(prop_schema, value),
        _ => None,
    }
}

/// Wrap a non-array value in a one-element array when the
/// schema's `items` type matches the value's type. Without a
/// declared items type the wrap would be a guess, so refuse.
fn wrap_scalar_in_array(prop_schema: &Value, value: &mut Value) -> Option<String> {
    let items_type = single_type(prop_schema.get("items")?)?;
    let matches = match items_type {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        _ => false,
    };
    matches.then(|| {
        *value = Value::Array(vec![value.take()]);
        "wrapped scalar into a one-element array".to_string()
    })
}

/// The schema's declared type, but only when it is a single
/// unambiguous string. Union types (`["string", "null"]`) and
/// missing types return `None` — coercing toward them would be
/// a guess.
fn single_type(schema: &Value) -> Option<&str> {
    schema.get("type")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object_schema(properties: Value) -> Value {
        json!({ "type": "object", "properties": properties })
    }

    #[test]
    fn string_encoded_object_args_coerce_when_schema_expects_object() {
        let schema = object_schema(json!({ "path": { "type": "string" } }));
        let args = json!(r#"{"path": "src/main.rs"}"#);
        let (coerced, notes) = coerce_arguments(&schema, args);
        assert_eq!(coerced, json!({ "path": "src/main.rs" }));
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn numeric_string_coerces_to_integer_only_when_lossless() {
        let schema = object_schema(json!({ "limit": { "type": "integer" } }));
        let (coerced, notes) = coerce_arguments(&schema, json!({ "limit": "42" }));
        assert_eq!(coerced, json!({ "limit": 42 }));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn numeric_string_coerces_to_number() {
        let schema = object_schema(json!({ "ratio": { "type": "number" } }));
        let (coerced, notes) = coerce_arguments(&schema, json!({ "ratio": "4.5" }));
        assert_eq!(coerced, json!({ "ratio": 4.5 }));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn boolean_strings_coerce_case_insensitively() {
        let schema = object_schema(json!({ "force": { "type": "boolean" } }));
        let (coerced, notes) = coerce_arguments(&schema, json!({ "force": "True" }));
        assert_eq!(coerced, json!({ "force": true }));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn scalar_wraps_into_single_element_array_when_items_type_matches() {
        let schema = object_schema(json!({
            "paths": { "type": "array", "items": { "type": "string" } }
        }));
        let (coerced, notes) = coerce_arguments(&schema, json!({ "paths": "src/main.rs" }));
        assert_eq!(coerced, json!({ "paths": ["src/main.rs"] }));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn string_encoded_array_parses_before_scalar_wrapping() {
        let schema = object_schema(json!({
            "paths": { "type": "array", "items": { "type": "string" } }
        }));
        let (coerced, notes) = coerce_arguments(&schema, json!({ "paths": r#"["a", "b"]"# }));
        assert_eq!(coerced, json!({ "paths": ["a", "b"] }));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn valid_arguments_pass_through_byte_identical_with_no_notes() {
        let schema = object_schema(json!({
            "path": { "type": "string" },
            "limit": { "type": "integer" },
        }));
        let args = json!({ "path": "a.rs", "limit": 10 });
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn ambiguous_or_lossy_coercions_are_refused() {
        // Lossy: "4.5" must not become an integer.
        let schema = object_schema(json!({ "limit": { "type": "integer" } }));
        let args = json!({ "limit": "4.5" });
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty());

        // Ambiguous: union types are never coerced toward.
        let schema = object_schema(json!({ "limit": { "type": ["integer", "string"] } }));
        let args = json!({ "limit": "42" });
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty());

        // Ambiguous: array without an items type is not a
        // wrap target.
        let schema = object_schema(json!({ "paths": { "type": "array" } }));
        let args = json!({ "paths": "src/main.rs" });
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty());
    }

    #[test]
    fn non_boolean_words_are_not_coerced_to_booleans() {
        let schema = object_schema(json!({ "force": { "type": "boolean" } }));
        let args = json!({ "force": "yes" });
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty());
    }

    #[test]
    fn unknown_properties_are_left_untouched() {
        let schema = object_schema(json!({ "path": { "type": "string" } }));
        let args = json!({ "mystery": "42" });
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty());
    }

    #[test]
    fn whole_args_string_that_is_not_an_object_is_untouched() {
        let schema = object_schema(json!({ "path": { "type": "string" } }));
        // Parses as JSON, but to an array — not an object.
        let args = json!(r#"["src/main.rs"]"#);
        let (coerced, notes) = coerce_arguments(&schema, args.clone());
        assert_eq!(coerced, args);
        assert!(notes.is_empty());
    }

    #[test]
    fn multiple_properties_coerce_independently_with_one_note_each() {
        let schema = object_schema(json!({
            "limit": { "type": "integer" },
            "force": { "type": "boolean" },
            "path": { "type": "string" },
        }));
        let (coerced, notes) = coerce_arguments(
            &schema,
            json!({ "limit": "10", "force": "false", "path": "ok.rs" }),
        );
        assert_eq!(
            coerced,
            json!({ "limit": 10, "force": false, "path": "ok.rs" })
        );
        assert_eq!(notes.len(), 2, "{notes:?}");
    }
}
