//! Normalize legacy OpenAI shapes before validating the shared Kiro input.

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{OpenAiRequest, ValidationError};

fn invalid(field: impl Into<String>, message: impl Into<String>) -> ValidationError {
    ValidationError::InvalidField {
        field: field.into(),
        message: message.into(),
    }
}

pub fn parse_openai_request(mut value: Value) -> Result<OpenAiRequest, ValidationError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("request", "expected an object"))?;
    let legacy = ["functions", "function_call"]
        .iter()
        .any(|key| object.get(*key).is_some_and(|value| !value.is_null()))
        && !object
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
    // Nullable modern fields are omission, including when an older client
    // supplies the deprecated fields alongside its default modern values.
    for field in ["tools", "tool_choice"] {
        if object.get(field).is_some_and(Value::is_null) {
            object.remove(field);
        }
    }
    if let Some(functions) = object.remove("functions").filter(|value| !value.is_null()) {
        let functions = functions
            .as_array()
            .ok_or_else(|| invalid("functions", "expected an array"))?;
        let tools = object
            .entry("tools")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| invalid("tools", "expected an array"))?;
        tools.extend(
            functions
                .iter()
                .map(|function| json!({"type":"function","function":function})),
        );
    }
    if let Some(choice) = object
        .remove("function_call")
        .filter(|value| !value.is_null())
    {
        if !object.contains_key("tool_choice") {
            let choice = match choice {
                Value::String(_) => choice,
                Value::Object(ref object) if object.get("name").is_some_and(Value::is_string) => {
                    json!({"type":"function","function":{"name":object["name"]}})
                }
                _ => {
                    return Err(invalid(
                        "function_call",
                        "expected a mode or a named function",
                    ))
                }
            };
            object.insert("tool_choice".into(), choice);
        }
    }
    let mut pending = Vec::<(String, String)>::new();
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for (index, message) in messages.iter_mut().enumerate() {
            let field = format!("messages.{index}");
            if message["role"] == "assistant" {
                if message.get("tool_calls").is_some_and(Value::is_null) {
                    message.as_object_mut().unwrap().remove("tool_calls");
                }
                if let Some(call) = message
                    .as_object_mut()
                    .and_then(|message| message.remove("function_call"))
                    .filter(|value| !value.is_null())
                {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            invalid(format!("{field}.function_call.name"), "is required")
                        })?;
                    let mut digest = Sha256::new();
                    digest.update(index.to_be_bytes());
                    digest.update(call.to_string().as_bytes());
                    let id = format!(
                        "tooluse_{}",
                        base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(&digest.finalize()[..16])
                    );
                    let calls = message
                        .as_object_mut()
                        .unwrap()
                        .entry("tool_calls")
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .ok_or_else(|| {
                            invalid(format!("{field}.tool_calls"), "expected an array")
                        })?;
                    calls.push(json!({"type":"function","id":id,"function":call}));
                    pending.push((name.to_owned(), id));
                }
            } else if message["role"] == "function" {
                let name = message
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let position = pending
                    .iter()
                    .position(|(function, _)| function == name)
                    .ok_or_else(|| {
                        invalid(
                            format!("{field}.name"),
                            "function result has no matching function_call",
                        )
                    })?;
                let (_, id) = pending.remove(position);
                message["role"] = json!("tool");
                message["tool_call_id"] = json!(id);
            }
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for (part_index, part) in parts.iter_mut().enumerate() {
                    if part["type"] == "file" {
                        *part =
                            openai_file_document(part, &format!("{field}.content.{part_index}"))?;
                    }
                }
            }
        }
    }
    let mut request: OpenAiRequest =
        serde_json::from_value(value).map_err(|error| invalid("request", error.to_string()))?;
    request.legacy_functions = legacy;
    if legacy {
        request.parallel_tool_calls = false;
    }
    Ok(request)
}

/// Inline data and public URLs use the same validation and bounded hydration
/// path as Claude documents. Foreign hosted file IDs cannot be resolved with a
/// Kiro credential and must never be mistaken for document contents.
pub fn openai_file_document(part: &Value, field: &str) -> Result<Value, ValidationError> {
    let file = part.get("file").unwrap_or(part);
    if file.get("file_id").is_some_and(|value| !value.is_null()) {
        return Err(invalid(
            format!("{field}.file_id"),
            "hosted file IDs cannot be resolved by Kiro; provide file_data or file_url",
        ));
    }
    let mut document = json!({"type":"document"});
    if let Some(filename) = file.get("filename").filter(|value| !value.is_null()) {
        if !filename.is_string() {
            return Err(invalid(format!("{field}.filename"), "expected a string"));
        }
        document["title"] = filename.clone();
    }
    if let Some(url) = file.get("file_url").filter(|value| !value.is_null()) {
        if !url.is_string() {
            return Err(invalid(format!("{field}.file_url"), "expected a string"));
        }
        document["source"] = json!({"type":"url","url":url});
    } else {
        let data = file
            .get("file_data")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid(
                    format!("{field}.file_data"),
                    "expected base64 data or a base64 data URL",
                )
            })?;
        let (media, encoded) = if let Some(data) = data.strip_prefix("data:") {
            let (media, encoded) = data.split_once(";base64,").ok_or_else(|| {
                invalid(format!("{field}.file_data"), "expected a base64 data URL")
            })?;
            (Some(media), encoded)
        } else {
            (None, data)
        };
        document["source"] = json!({"type":"base64","data":encoded});
        if let Some(media) = media {
            document["source"]["media_type"] = json!(media);
        }
        if crate::translate::common::claude_document_format(&document).is_none() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| {
                    invalid(format!("{field}.file_data"), "invalid base64 document data")
                })?;
            document["source"]["media_type"] = if bytes.starts_with(b"%PDF-") {
                json!("application/pdf")
            } else if std::str::from_utf8(&bytes).is_ok() {
                json!("text/plain")
            } else {
                return Err(invalid(
                    format!("{field}.filename"),
                    "provide a filename or a data URL media type for binary documents",
                ));
            };
        }
    }
    if let Some(control) = part.get("cache_control") {
        document["cache_control"] = control.clone();
    }
    Ok(document)
}
