//! Guard controls whose semantics cannot be implemented by non-OpenAI backends.

use serde_json::Value;

use crate::ValidationError;

fn unsupported(field: impl Into<String>) -> ValidationError {
    ValidationError::InvalidField {
        field: field.into(),
        message:
            "requires a native OpenAI upstream; this backend cannot guarantee this cache strategy"
                .into(),
    }
}

pub(crate) fn validate_options(options: Option<&Value>) -> Result<(), ValidationError> {
    let Some(options) = options.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let Some(options) = options.as_object() else {
        return Err(unsupported("prompt_cache_options"));
    };
    // In particular, a prewarm must never turn into an ordinary, billable
    // generation. An explicit minimum lifetime cannot be promised either.
    for field in ["prewarm", "ttl", "mode"] {
        let Some(value) = options.get(field).filter(|value| !value.is_null()) else {
            continue;
        };
        let compatible = match field {
            "prewarm" => value == &Value::Bool(false),
            "mode" => value.as_str() == Some("implicit"),
            _ => false,
        };
        if !compatible {
            return Err(unsupported(format!("prompt_cache_options.{field}")));
        }
    }
    Ok(())
}

pub(crate) fn validate_content(content: &Value, field: &str) -> Result<(), ValidationError> {
    for (index, part) in content.as_array().into_iter().flatten().enumerate() {
        validate_part(part, &format!("{field}.{index}"))?;
    }
    Ok(())
}

pub(crate) fn validate_part(part: &Value, field: &str) -> Result<(), ValidationError> {
    if part
        .get("prompt_cache_breakpoint")
        .is_some_and(|value| !value.is_null())
    {
        return Err(unsupported(format!("{field}.prompt_cache_breakpoint")));
    }
    Ok(())
}
