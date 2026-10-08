//! Compatibility baseline: jwadow a5292ca, hj01857655 c5c4776 and chaogei 447adcd.
//! Format and additive hints are accepted without enforcing structured output.
//! Compare actual serialized Kiro input, including non-null Claude formats.
use crosslane_translate::{
    claude_to_kiro, openai_to_kiro, responses_to_openai, validate_claude, validate_openai,
    ClaudeRequest, OpenAiRequest, ResponsesRequest, TranslationOptions,
};
use serde_json::{json, Value};

fn output_format() -> Value {
    json!({"type":"json_schema","schema":{
        "type":"object","properties":{"output_format_sentinel":{"type":"string"}},
        "required":["output_format_sentinel"],"additionalProperties":false
    }})
}

fn assert_kiro_projection(wire: Value) {
    assert!(wire.get("additionalModelRequestFields").is_none(), "{wire}");
    let serialized = wire.to_string();
    for ignored in [
        "output_format_sentinel",
        "strict",
        "future_hint",
        "output_config",
        "response_format",
    ] {
        assert!(
            !serialized.contains(ignored),
            "unexpected {ignored}: {wire}"
        );
    }
    assert!(
        serialized.contains("tool_argument"),
        "tool input schema was lost: {wire}"
    );
    assert!(
        serialized.contains("Reply pong"),
        "user content was lost: {wire}"
    );
}

#[test]
fn claude_format_and_additive_hints_do_not_reject_a_kiro_request() {
    for (stream, format) in [false, true].into_iter().flat_map(|stream| {
        [
            output_format(),
            json!({"type":"text"}),
            json!({"type":"json_object"}),
            Value::Null,
        ]
        .into_iter()
        .map(move |format| (stream, format))
    }) {
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-haiku-4-5-20251001","max_tokens":4096,"stream":stream,
            "messages":[{"role":"user","content":"Reply pong","future_hint":true}],
            "output_config":{"format":format,"task_budget":{"type":"tokens","total":64000},"future_hint":true},
            "service_tier":"standard_only","future_hint":true,
            "tools":[{"name":"lookup","description":"Find a record","strict":true,
                "eager_input_streaming":true,"future_hint":true,
                "input_schema":{"type":"object","properties":{"tool_argument":{"type":"string"}}}}]
        }))
        .unwrap();
        validate_claude(&request).expect("reference-compatible Claude controls");
        let wire = serde_json::to_value(claude_to_kiro(
            &request,
            &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
        ))
        .unwrap();
        assert_kiro_projection(wire);
    }
}

#[test]
fn openai_format_and_additive_hints_do_not_reject_a_kiro_request() {
    for stream in [false, true] {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-haiku-4.5","max_tokens":4096,"stream":stream,
            "messages":[{"role":"user","content":"Reply pong","future_hint":true}],
            "response_format":{"type":"json_schema","json_schema":{
                "name":"answer","strict":true,"schema":output_format()["schema"]}},
            "stream_options":{"include_usage":true,"future_hint":true},
            "service_tier":"priority","future_hint":true,
            "tools":[{"type":"function","function":{"name":"lookup","strict":true,
                "parameters":{"type":"object","properties":{"tool_argument":{"type":"string"}}}}}]
        }))
        .expect("additive OpenAI request fields");
        validate_openai(&request).expect("reference-compatible OpenAI controls");
        assert_kiro_projection(
            serde_json::to_value(openai_to_kiro(
                &request,
                &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
            ))
            .unwrap(),
        );
    }
}

#[test]
fn responses_format_and_additive_hints_use_the_same_kiro_projection() {
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"claude-haiku-4.5","input":"Reply pong","max_output_tokens":4096,
        "text":{"format":output_format(),"verbosity":"future_verbosity","future_hint":true},
        "reasoning":{"summary":"future_summary","context":"future_context","future_hint":true},
        "include":["file_search_call.results"],"prompt_cache_retention":"24h",
        "service_tier":"priority","future_hint":true,
        "stream_options":{"include_obfuscation":true,"future_hint":true},
        "tools":[{"type":"function","name":"lookup","strict":true,
            "parameters":{"type":"object","properties":{"tool_argument":{"type":"string"}}}}]
    }))
    .expect("additive Responses request fields");
    let normalized =
        responses_to_openai(&request).expect("reference-compatible Responses controls");
    assert_kiro_projection(
        serde_json::to_value(openai_to_kiro(
            &normalized.request,
            &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
        ))
        .unwrap(),
    );
}

/// Codex replays the reasoning item it received on the previous turn, including
/// an opaque `encrypted_content` this proxy never issued. Kiro cannot decrypt
/// it, but failing the request strands the conversation on turn two, so the
/// blob is dropped and any plaintext summary is preserved.
#[test]
fn responses_accepts_replayed_reasoning_and_unsupported_input_items() {
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"claude-haiku-4.5","max_output_tokens":4096,
        "reasoning":{"effort":"medium","summary":"auto","context":"future_context"},
        "include":["reasoning.encrypted_content"],
        "input":[
            {"type":"message","role":"user","content":"Reply pong"},
            {"type":"reasoning","id":"rs_1","encrypted_content":"gAAAAAB_opaque_blob",
                "summary":[{"type":"summary_text","text":"weighing the options"}]},
            {"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":"pong"}]},
            // Hosted-tool and reference items carry nothing Kiro needs.
            {"type":"web_search_call","id":"ws_1","status":"completed"},
            {"type":"item_reference","id":"msg_1"},
            {"type":"message","role":"user","content":"Reply pong again"}
        ]
    }))
    .expect("replayed Responses history");
    let normalized = responses_to_openai(&request).expect("replayed reasoning is not fatal");
    let wire = serde_json::to_value(openai_to_kiro(
        &normalized.request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    ))
    .unwrap();
    let serialized = wire.to_string();
    // The opaque blob and the skipped item types must not reach Kiro.
    for leaked in [
        "gAAAAAB_opaque_blob",
        "encrypted_content",
        "web_search_call",
        "item_reference",
        "future_context",
    ] {
        assert!(!serialized.contains(leaked), "{leaked} leaked into {wire}");
    }
    // The plaintext summary the client did send is still carried.
    assert!(serialized.contains("weighing the options"), "{wire}");
}

/// Relaxing additive fields must not weaken the checks that keep a Kiro payload
/// well-formed. Stateful controls are accepted here and resolved after
/// authentication by the HTTP layer.
#[test]
fn responses_still_rejects_malformed_history_and_unimplemented_controls() {
    for (label, body) in [
        (
            "tool output without a matching call",
            json!({"model":"claude-haiku-4.5","input":[
                {"type":"message","role":"user","content":"hi"},
                {"type":"function_call_output","call_id":"c1","output":"r"}]}),
        ),
        (
            "tool call left unanswered",
            json!({"model":"claude-haiku-4.5","input":[
                {"type":"message","role":"user","content":"hi"},
                {"type":"function_call","call_id":"c1","name":"f","arguments":"{}"}]}),
        ),
        (
            "invalid truncation mode",
            json!({"model":"claude-haiku-4.5","input":"hi","truncation":"invalid"}),
        ),
        (
            "hosted tool the proxy cannot execute",
            json!({"model":"claude-haiku-4.5","input":"hi","tools":[{"type":"web_search"}]}),
        ),
    ] {
        let request: ResponsesRequest =
            serde_json::from_value(body).expect("request should deserialize");
        assert!(
            responses_to_openai(&request).is_err(),
            "{label} must still be rejected"
        );
    }
}

#[test]
fn responses_accepts_stateful_controls_for_authenticated_http_handling() {
    for body in [
        json!({"model":"claude-haiku-4.5","input":"hi","store":true}),
        json!({"model":"claude-haiku-4.5","input":"hi","previous_response_id":"resp_1"}),
        json!({"model":"claude-haiku-4.5","input":"hi","truncation":"auto","max_tool_calls":1}),
    ] {
        let request: ResponsesRequest =
            serde_json::from_value(body).expect("request should deserialize");
        responses_to_openai(&request).expect("HTTP layer resolves stateful controls");
    }
}
