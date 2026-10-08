//! Claude cache-control schema and prompt-order constraints.

use serde_json::Value;

use super::{invalid, ValidationError, MAX_CACHE_BREAKPOINTS};
use crate::ClaudeRequest;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheTtl {
    FiveMinutes,
    OneHour,
}

fn cache_control_ttl(
    value: Option<&Value>,
    field: &str,
) -> Result<Option<CacheTtl>, ValidationError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(control) = value.as_object() else {
        return invalid(field, "expected an object");
    };
    let Some(kind) = control
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.trim().is_empty())
    else {
        return invalid(format!("{field}.type"), "expected a non-empty string");
    };
    if kind != "ephemeral" {
        return invalid(format!("{field}.type"), "expected ephemeral");
    }
    match control.get("ttl") {
        None => Ok(Some(CacheTtl::FiveMinutes)),
        Some(Value::String(ttl)) if ttl == "5m" => Ok(Some(CacheTtl::FiveMinutes)),
        Some(Value::String(ttl)) if ttl == "1h" => Ok(Some(CacheTtl::OneHour)),
        Some(_) => invalid(format!("{field}.ttl"), "expected 5m or 1h"),
    }
}

/// The OpenAI extension shares Claude's marker schema, not its prompt rules.
pub(super) fn validate_cache_control(
    value: Option<&Value>,
    field: &str,
) -> Result<usize, ValidationError> {
    Ok(usize::from(cache_control_ttl(value, field)?.is_some()))
}

struct Breakpoint {
    ttl: CacheTtl,
    field: String,
}

struct AutomaticTarget {
    /// Insert an automatic marker here to retain prompt order.
    position: usize,
    explicit_ttl: Option<CacheTtl>,
    field: String,
}

#[derive(Default)]
struct CacheControls {
    breakpoints: Vec<Breakpoint>,
    last_target: Option<AutomaticTarget>,
}

impl CacheControls {
    fn visit(
        &mut self,
        value: Option<&Value>,
        field: String,
        eligible: bool,
    ) -> Result<(), ValidationError> {
        let ttl = cache_control_ttl(value, &field)?;
        let position = self.breakpoints.len();
        if let Some(ttl) = ttl {
            self.breakpoints.push(Breakpoint {
                ttl,
                field: field.clone(),
            });
        }
        if eligible {
            self.last_target = Some(AutomaticTarget {
                position,
                explicit_ttl: ttl,
                field,
            });
        }
        Ok(())
    }

    fn content(
        &mut self,
        content: &Value,
        field: &str,
        top_level: bool,
    ) -> Result<(), ValidationError> {
        if let Some(text) = content.as_str() {
            if top_level && !text.is_empty() {
                self.visit(None, format!("{field}.cache_control"), true)?;
            }
            return Ok(());
        }
        let Some(blocks) = content.as_array() else {
            return Ok(());
        };
        for (index, block) in blocks.iter().enumerate() {
            let block_field = format!("{field}.{index}");
            let kind = block.get("type").and_then(Value::as_str);
            match kind {
                Some("tool_result" | "search_result") => {
                    if let Some(nested) = block.get("content") {
                        self.content(nested, &format!("{block_field}.content"), false)?;
                    }
                }
                Some("document")
                    if block.pointer("/source/type").and_then(Value::as_str) == Some("content") =>
                {
                    if let Some(nested) = block.pointer("/source/content") {
                        self.content(nested, &format!("{block_field}.source.content"), false)?;
                    }
                }
                _ => {}
            }
            let control = block.get("cache_control");
            let cache_field = format!("{block_field}.cache_control");
            let cannot_cache = matches!(kind, Some("thinking" | "redacted_thinking"))
                || (kind == Some("text") && block.get("text").and_then(Value::as_str) == Some(""));
            if cannot_cache && control.is_some_and(|control| !control.is_null()) {
                // Validate the marker first so malformed types retain precise errors.
                cache_control_ttl(control, &cache_field)?;
                return invalid(
                    cache_field,
                    "thinking and empty text blocks cannot define cache_control",
                );
            }
            let eligible = !cannot_cache
                && (matches!(
                    kind,
                    Some(
                        "text"
                            | "image"
                            | "document"
                            | "tool_use"
                            | "tool_result"
                            | "server_tool_use"
                            | "web_search_tool_result"
                            | "tool_search_tool_result"
                            | "search_result"
                    )
                ) || (kind == Some("compaction")
                    && block
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| !content.is_empty())));
            self.visit(control, cache_field, top_level && eligible)?;
        }
        Ok(())
    }

    fn finish(mut self, automatic: Option<CacheTtl>) -> Result<(), ValidationError> {
        if let (Some(ttl), Some(target)) = (automatic, self.last_target) {
            // The documented automatic-caching limit requires an available slot
            // even though a same-TTL marker is otherwise a no-op.
            if self.breakpoints.len() >= MAX_CACHE_BREAKPOINTS {
                return invalid(
                    "cache_control",
                    format!(
                        "must define at most {MAX_CACHE_BREAKPOINTS} cache breakpoints including automatic caching"
                    ),
                );
            }
            if let Some(explicit_ttl) = target.explicit_ttl {
                if ttl != explicit_ttl {
                    return invalid(
                        format!("{}.ttl", target.field),
                        "must match the top-level cache_control TTL",
                    );
                }
            } else {
                self.breakpoints.insert(
                    target.position,
                    Breakpoint {
                        ttl,
                        field: "cache_control".to_owned(),
                    },
                );
            }
        }
        if self.breakpoints.len() > MAX_CACHE_BREAKPOINTS {
            return invalid(
                "cache_control",
                format!("must define at most {MAX_CACHE_BREAKPOINTS} cache breakpoints"),
            );
        }
        let mut saw_five_minutes = false;
        for point in self.breakpoints {
            if point.ttl == CacheTtl::OneHour && saw_five_minutes {
                return invalid(
                    format!("{}.ttl", point.field),
                    "1h cache breakpoints must precede all 5m cache breakpoints",
                );
            }
            saw_five_minutes |= point.ttl == CacheTtl::FiveMinutes;
        }
        Ok(())
    }
}

pub(super) fn validate_claude_cache_controls(
    request: &ClaudeRequest,
) -> Result<(), ValidationError> {
    let automatic = cache_control_ttl(request.cache_control.as_ref(), "cache_control")?;
    let mut controls = CacheControls::default();
    // Claude builds the prompt prefix in this order, independently of JSON order.
    for (index, tool) in request.tools.iter().enumerate() {
        controls.visit(
            tool.cache_control.as_ref(),
            format!("tools.{index}.cache_control"),
            !tool.defer_loading,
        )?;
    }
    if let Some(system) = &request.system {
        controls.content(system, "system", true)?;
    }
    for (index, message) in request.messages.iter().enumerate() {
        controls.content(&message.content, &format!("messages.{index}.content"), true)?;
        // Preserve the existing message-level extension as an end-of-message
        // breakpoint, without treating it as an additional automatic target.
        controls.visit(
            message.cache_control.as_ref(),
            format!("messages.{index}.cache_control"),
            false,
        )?;
    }
    controls.finish(automatic)
}
