use crosslane_translate::{
    responses_to_openai, validate_claude, validate_openai, ClaudeRequest, OpenAiRequest,
    ResponsesRequest,
};
use serde_json::json;

#[test]
fn all_protocols_reject_pasted_or_unbounded_models_without_echoing_them() {
    for model in [
        "pasted_terminal_text\nSELECT private_data".to_owned(),
        "pasted_terminal_text\tdata".to_owned(),
        "pasted_terminal_text\u{001b}[31m".to_owned(),
        "pasted_terminal_text more".to_owned(),
        format!("pasted_terminal_text{}", "x".repeat(256)),
    ] {
        let body =
            json!({"model":model, "max_tokens":16, "messages":[{"role":"user","content":"hello"}]});
        let claude: ClaudeRequest = serde_json::from_value(body.clone()).unwrap();
        let openai: OpenAiRequest = serde_json::from_value(body).unwrap();
        let responses: ResponsesRequest =
            serde_json::from_value(json!({"model":model,"input":"hello"})).unwrap();
        for error in [
            validate_claude(&claude).unwrap_err(),
            validate_openai(&openai).unwrap_err(),
            responses_to_openai(&responses).err().unwrap(),
        ] {
            let message = error.to_string();
            assert!(message.contains("model"));
            assert!(!message.contains("pasted_terminal_text"));
        }
    }
}

#[test]
fn model_safety_limits_preserve_custom_aliases_and_namespaced_ids() {
    for model in [
        "claude-opus-5[1m]".to_owned(),
        "provider/claude:latest+thinking".to_owned(),
        "自定义模型".to_owned(),
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/claude".to_owned(),
        "x".repeat(256),
    ] {
        crosslane_translate::validate::validate_model(&model).unwrap();
    }
}
