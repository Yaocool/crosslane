//! Last-mile normalization for Kiro's strict tool-use history contract.
//!
//! Client histories can become incomplete after context editing, compaction,
//! or retries. Kiro rejects the entire request when a structured tool call is
//! undefined, malformed, duplicated, or not followed by exactly one matching
//! result. This module repairs valid calls locally and archives only the calls
//! that cannot be represented safely as structured Kiro history.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::json;

use crate::{
    KiroAssistantMessage, KiroHistoryMessage, KiroMessageContext, KiroPayload, KiroText,
    KiroToolResult, KiroUserInputMessage,
};

const MISSING_RESULT_TEXT: &str =
    "The historical tool execution result is unavailable because the conversation history was incomplete.";

/// Counts repairs made before a Kiro generation request is sent upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KiroToolHistoryStats {
    pub flattened_tool_uses: usize,
    pub flattened_tool_results: usize,
    pub normalized_tool_uses: usize,
    pub removed_invalid_tool_uses: usize,
    pub relocated_tool_results: usize,
    pub synthesized_tool_results: usize,
    pub normalized_tool_results: usize,
    pub removed_historical_tool_definitions: usize,
    pub removed_duplicate_tool_definitions: usize,
    pub inserted_messages: usize,
}

impl KiroToolHistoryStats {
    pub fn repaired(&self) -> bool {
        self.has_structured_tool_repair() || self.inserted_messages > 0
    }

    /// Whether normalization changed structured tool data rather than only
    /// adding a role placeholder to meet Kiro's alternating-turn contract.
    pub fn has_structured_tool_repair(&self) -> bool {
        self.flattened_tool_uses > 0
            || self.flattened_tool_results > 0
            || self.normalized_tool_uses > 0
            || self.removed_invalid_tool_uses > 0
            || self.relocated_tool_results > 0
            || self.synthesized_tool_results > 0
            || self.normalized_tool_results > 0
            || self.removed_historical_tool_definitions > 0
            || self.removed_duplicate_tool_definitions > 0
    }
}

#[derive(Debug)]
enum ConversationMessage {
    User(KiroUserInputMessage),
    Assistant(KiroAssistantMessage),
}

impl ConversationMessage {
    fn user_mut(&mut self) -> Option<&mut KiroUserInputMessage> {
        match self {
            Self::User(user) => Some(user),
            Self::Assistant(_) => None,
        }
    }
}

#[derive(Debug)]
struct SourcedResult {
    source_index: usize,
    result: Option<KiroToolResult>,
}

/// Repairs a Kiro payload so ordinary and proxy-executed tool calls form
/// strict, adjacent, one-to-one call/result pairs.
///
/// Calls whose definitions are still active remain structured. Undefined,
/// malformed, or ambiguous calls are archived as neutral JSON records while
/// the rest of the history remains structured, so one bad call cannot flatten
/// unrelated tool calls or make model-visible text resemble an instruction.
pub fn sanitize_kiro_tool_history(payload: &mut KiroPayload) -> KiroToolHistoryStats {
    let mut stats = KiroToolHistoryStats::default();
    let state = &mut payload.conversation_state;

    let active_tool_names = sanitize_current_tool_definitions(
        &mut state
            .current_message
            .user_input_message
            .user_input_message_context,
        &mut stats,
    );

    let current = state.current_message.user_input_message.clone();
    let mut messages = Vec::with_capacity(state.history.len().saturating_add(1));
    for mut item in std::mem::take(&mut state.history) {
        if let Some(mut user) = item.user_input_message.take() {
            if let Some(context) = user.user_input_message_context.as_mut() {
                stats.removed_historical_tool_definitions = stats
                    .removed_historical_tool_definitions
                    .saturating_add(context.tools.len());
                context.tools.clear();
                context.env_state = None;
                if context.tool_results.is_empty() {
                    user.user_input_message_context = None;
                }
            }
            messages.push(ConversationMessage::User(user));
        }
        if let Some(assistant) = item.assistant_response_message.take() {
            messages.push(ConversationMessage::Assistant(assistant));
        }
    }
    messages.push(ConversationMessage::User(current));

    let scheduled = repair_tool_pairs(&mut messages, &active_tool_names, &mut stats);
    messages = normalize_roles(messages, scheduled, &mut stats);

    let current = match messages.pop() {
        Some(ConversationMessage::User(user)) => user,
        Some(ConversationMessage::Assistant(_)) | None => {
            // normalize_roles always leaves a user message last. Keep this
            // defensive branch so a future refactor cannot emit an invalid
            // currentMessage shape.
            stats.inserted_messages = stats.inserted_messages.saturating_add(1);
            synthetic_user(
                &state.current_message.user_input_message,
                "Continue.",
                Vec::new(),
            )
        }
    };
    state.current_message.user_input_message = current;
    state.history = messages
        .into_iter()
        .map(|message| match message {
            ConversationMessage::User(user) => KiroHistoryMessage {
                user_input_message: Some(user),
                assistant_response_message: None,
            },
            ConversationMessage::Assistant(assistant) => KiroHistoryMessage {
                user_input_message: None,
                assistant_response_message: Some(assistant),
            },
        })
        .collect();
    stats
}

/// Verifies the exact Kiro tool history contract after all payload mutations.
pub fn validate_kiro_tool_history(payload: &KiroPayload) -> Result<(), String> {
    let current = &payload
        .conversation_state
        .current_message
        .user_input_message;
    let active_names = current
        .user_input_message_context
        .as_ref()
        .map(|context| {
            context
                .tools
                .iter()
                .filter_map(|tool| tool.specification().map(|tool| tool.name.as_str()))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    if active_names.iter().any(|name| name.trim().is_empty()) {
        return Err("current tool definitions contain an empty name".into());
    }
    if current
        .user_input_message_context
        .as_ref()
        .is_some_and(|context| {
            context
                .tools
                .iter()
                .filter(|tool| tool.specification().is_some())
                .count()
                != active_names.len()
        })
    {
        return Err("current tool definitions contain duplicate names".into());
    }

    let mut previous_was_user = false;
    let mut previous_uses = Vec::<String>::new();
    let mut all_use_ids = HashSet::new();
    let mut all_result_ids = HashSet::new();

    for (index, item) in payload.conversation_state.history.iter().enumerate() {
        match (
            item.user_input_message.as_ref(),
            item.assistant_response_message.as_ref(),
        ) {
            (Some(user), None) => {
                if previous_was_user {
                    return Err(format!("history message {index} repeats the user role"));
                }
                if user
                    .user_input_message_context
                    .as_ref()
                    .is_some_and(|context| !context.tools.is_empty())
                {
                    return Err(format!(
                        "history message {index} contains tool definitions; definitions belong on the current message"
                    ));
                }
                validate_user_results(
                    user,
                    &previous_uses,
                    &mut all_result_ids,
                    &format!("history message {index}"),
                )?;
                previous_uses.clear();
                previous_was_user = true;
            }
            (None, Some(assistant)) => {
                if index == 0 || !previous_was_user {
                    return Err(format!(
                        "history message {index} repeats or starts with assistant"
                    ));
                }
                previous_uses = validate_assistant_uses(
                    assistant,
                    &active_names,
                    &mut all_use_ids,
                    &format!("history message {index}"),
                )?;
                previous_was_user = false;
            }
            _ => {
                return Err(format!(
                    "history message {index} must contain exactly one role"
                ))
            }
        }
    }

    if !previous_was_user && payload.conversation_state.history.is_empty() {
        previous_uses.clear();
    }
    if previous_was_user && !previous_uses.is_empty() {
        return Err("internal tool-pair validation state is inconsistent".into());
    }
    if !payload.conversation_state.history.is_empty() && previous_was_user {
        return Err("current user message would repeat the user role".into());
    }
    validate_user_results(
        current,
        &previous_uses,
        &mut all_result_ids,
        "current message",
    )?;
    Ok(())
}

fn sanitize_current_tool_definitions(
    context: &mut Option<KiroMessageContext>,
    stats: &mut KiroToolHistoryStats,
) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(context) = context {
        context.tools.retain_mut(|tool| {
            let Some(specification) = tool.specification_mut() else {
                return true;
            };
            let name = specification.name.trim();
            let keep = !name.is_empty() && names.insert(name.to_owned());
            if !keep {
                stats.removed_duplicate_tool_definitions =
                    stats.removed_duplicate_tool_definitions.saturating_add(1);
            } else if name != specification.name {
                specification.name = name.to_owned();
            }
            keep
        });
    }
    names
}

fn repair_tool_pairs(
    messages: &mut [ConversationMessage],
    active_tool_names: &HashSet<String>,
    stats: &mut KiroToolHistoryStats,
) -> HashMap<usize, Vec<KiroToolResult>> {
    let mut results = Vec::<SourcedResult>::new();
    let mut result_indexes = HashMap::<String, VecDeque<usize>>::new();
    for (source_index, message) in messages.iter_mut().enumerate() {
        let Some(user) = message.user_mut() else {
            continue;
        };
        for result in take_tool_results(user) {
            let index = results.len();
            result_indexes
                .entry(result.tool_use_id.trim().to_owned())
                .or_default()
                .push_back(index);
            results.push(SourcedResult {
                source_index,
                result: Some(result),
            });
        }
    }

    let tool_use_id_counts = messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::Assistant(assistant) => Some(&assistant.tool_uses),
            ConversationMessage::User(_) => None,
        })
        .flatten()
        .map(|tool_use| tool_use.tool_use_id.trim())
        .filter(|id| !id.is_empty())
        .fold(HashMap::<String, usize>::new(), |mut counts, id| {
            *counts.entry(id.to_owned()).or_default() += 1;
            counts
        });
    let mut scheduled = HashMap::<usize, Vec<KiroToolResult>>::new();
    for (assistant_index, message) in messages.iter_mut().enumerate() {
        let ConversationMessage::Assistant(assistant) = message else {
            continue;
        };
        let original_uses = std::mem::take(&mut assistant.tool_uses);
        let mut repaired_uses = Vec::with_capacity(original_uses.len());
        for mut tool_use in original_uses {
            let original_id = tool_use.tool_use_id.clone();
            let mut normalized = false;

            let trimmed_name = tool_use.name.trim();
            let trimmed_id = original_id.trim();
            let duplicate_id = !trimmed_id.is_empty()
                && tool_use_id_counts
                    .get(trimmed_id)
                    .copied()
                    .unwrap_or_default()
                    > 1;
            let malformed = trimmed_name.is_empty()
                || trimmed_id.is_empty()
                || duplicate_id
                || !tool_use.input.is_object();
            let undefined = !trimmed_name.is_empty() && !active_tool_names.contains(trimmed_name);
            if malformed || undefined {
                append_text(&mut assistant.content, &render_tool_use(&tool_use));
                stats.flattened_tool_uses = stats.flattened_tool_uses.saturating_add(1);
                if malformed {
                    stats.removed_invalid_tool_uses =
                        stats.removed_invalid_tool_uses.saturating_add(1);
                }
                continue;
            }

            if trimmed_name != tool_use.name {
                tool_use.name = trimmed_name.to_owned();
                normalized = true;
            }
            if trimmed_id != original_id {
                tool_use.tool_use_id = trimmed_id.to_owned();
                normalized = true;
            }
            if normalized {
                stats.normalized_tool_uses = stats.normalized_tool_uses.saturating_add(1);
            }

            let matching = result_indexes
                .get_mut(original_id.trim())
                .and_then(|indexes| {
                    while indexes
                        .front()
                        .is_some_and(|index| results[*index].source_index <= assistant_index)
                    {
                        indexes.pop_front();
                    }
                    indexes.pop_front()
                });
            let result = matching
                .and_then(|index| {
                    let candidate = &mut results[index];
                    let source_index = candidate.source_index;
                    candidate.result.take().map(|result| (source_index, result))
                })
                .map(|(source_index, mut result)| {
                    if source_index != assistant_index.saturating_add(1) {
                        stats.relocated_tool_results =
                            stats.relocated_tool_results.saturating_add(1);
                    }
                    let mut normalized_result = normalize_result(&mut result);
                    if result.tool_use_id != tool_use.tool_use_id {
                        result.tool_use_id.clone_from(&tool_use.tool_use_id);
                        normalized_result = true;
                    }
                    if normalized_result {
                        stats.normalized_tool_results =
                            stats.normalized_tool_results.saturating_add(1);
                    }
                    result
                })
                .unwrap_or_else(|| {
                    stats.synthesized_tool_results =
                        stats.synthesized_tool_results.saturating_add(1);
                    missing_result(&tool_use.tool_use_id)
                });
            scheduled.entry(assistant_index).or_default().push(result);
            repaired_uses.push(tool_use);
        }
        assistant.tool_uses = repaired_uses;
    }

    for candidate in results {
        let Some(result) = candidate.result else {
            continue;
        };
        if let Some(user) = messages
            .get_mut(candidate.source_index)
            .and_then(ConversationMessage::user_mut)
        {
            append_text(&mut user.content, &render_tool_result(&result));
            stats.flattened_tool_results = stats.flattened_tool_results.saturating_add(1);
        }
    }

    scheduled
}

fn normalize_roles(
    messages: Vec<ConversationMessage>,
    mut scheduled: HashMap<usize, Vec<KiroToolResult>>,
    stats: &mut KiroToolHistoryStats,
) -> Vec<ConversationMessage> {
    let template = messages.iter().rev().find_map(|message| match message {
        ConversationMessage::User(user) => Some(user.clone()),
        ConversationMessage::Assistant(_) => None,
    });
    let template = template.unwrap_or_else(|| KiroUserInputMessage {
        content: "Continue.".into(),
        model_id: String::new(),
        origin: String::new(),
        images: Vec::new(),
        documents: Vec::new(),
        cache_point: None,
        client_cache_config: None,
        user_input_message_context: None,
    });
    let mut output = Vec::with_capacity(messages.len().saturating_add(2));
    let mut pending_results = Vec::new();

    for (original_index, message) in messages.into_iter().enumerate() {
        match message {
            ConversationMessage::User(mut user) => {
                if matches!(output.last(), Some(ConversationMessage::User(_))) {
                    output.push(ConversationMessage::Assistant(synthetic_assistant()));
                    stats.inserted_messages = stats.inserted_messages.saturating_add(1);
                }
                if !pending_results.is_empty() {
                    add_tool_results(&mut user, std::mem::take(&mut pending_results));
                }
                output.push(ConversationMessage::User(user));
            }
            ConversationMessage::Assistant(assistant) => {
                if output.is_empty()
                    || matches!(output.last(), Some(ConversationMessage::Assistant(_)))
                {
                    let content = if output.is_empty() {
                        "Begin conversation."
                    } else {
                        "Continue after the previous tool call."
                    };
                    output.push(ConversationMessage::User(synthetic_user(
                        &template,
                        content,
                        std::mem::take(&mut pending_results),
                    )));
                    stats.inserted_messages = stats.inserted_messages.saturating_add(1);
                }
                pending_results = scheduled.remove(&original_index).unwrap_or_default();
                output.push(ConversationMessage::Assistant(assistant));
            }
        }
    }
    if output.is_empty() || matches!(output.last(), Some(ConversationMessage::Assistant(_))) {
        output.push(ConversationMessage::User(synthetic_user(
            &template,
            "Continue.",
            pending_results,
        )));
        stats.inserted_messages = stats.inserted_messages.saturating_add(1);
    }
    output
}

fn validate_assistant_uses(
    assistant: &KiroAssistantMessage,
    active_names: &HashSet<&str>,
    all_ids: &mut HashSet<String>,
    location: &str,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(assistant.tool_uses.len());
    for tool_use in &assistant.tool_uses {
        if tool_use.tool_use_id.trim().is_empty() || tool_use.name.trim().is_empty() {
            return Err(format!("{location} contains an unnamed tool call"));
        }
        if !tool_use.input.is_object() {
            return Err(format!(
                "{location} tool call '{}' has a non-object input",
                tool_use.tool_use_id
            ));
        }
        if !active_names.contains(tool_use.name.as_str()) {
            return Err(format!(
                "{location} tool call '{}' references undefined tool '{}'",
                tool_use.tool_use_id, tool_use.name
            ));
        }
        if !all_ids.insert(tool_use.tool_use_id.clone()) {
            return Err(format!(
                "{location} duplicates tool-use id '{}'",
                tool_use.tool_use_id
            ));
        }
        ids.push(tool_use.tool_use_id.clone());
    }
    Ok(ids)
}

fn validate_user_results(
    user: &KiroUserInputMessage,
    expected: &[String],
    all_ids: &mut HashSet<String>,
    location: &str,
) -> Result<(), String> {
    let results = user
        .user_input_message_context
        .as_ref()
        .map(|context| context.tool_results.as_slice())
        .unwrap_or_default();
    if results.len() != expected.len() {
        return Err(format!(
            "{location} has {} tool results but the preceding assistant has {} tool calls",
            results.len(),
            expected.len()
        ));
    }
    let expected = expected.iter().map(String::as_str).collect::<HashSet<_>>();
    for result in results {
        if result.tool_use_id.trim().is_empty() {
            return Err(format!("{location} contains a tool result without an id"));
        }
        if !matches!(result.status.as_str(), "success" | "error") {
            return Err(format!(
                "{location} tool result '{}' has invalid status '{}'",
                result.tool_use_id, result.status
            ));
        }
        if result.content.is_empty() {
            return Err(format!(
                "{location} tool result '{}' has no content",
                result.tool_use_id
            ));
        }
        if !expected.contains(result.tool_use_id.as_str()) {
            return Err(format!(
                "{location} contains orphan tool result '{}'",
                result.tool_use_id
            ));
        }
        if !all_ids.insert(result.tool_use_id.clone()) {
            return Err(format!(
                "{location} duplicates tool result '{}'",
                result.tool_use_id
            ));
        }
    }
    Ok(())
}

fn take_tool_results(user: &mut KiroUserInputMessage) -> Vec<KiroToolResult> {
    let Some(context) = user.user_input_message_context.as_mut() else {
        return Vec::new();
    };
    let results = std::mem::take(&mut context.tool_results);
    if context.tools.is_empty() && context.env_state.is_none() {
        user.user_input_message_context = None;
    }
    results
}

fn add_tool_results(user: &mut KiroUserInputMessage, results: Vec<KiroToolResult>) {
    if results.is_empty() {
        return;
    }
    user.user_input_message_context
        .get_or_insert_with(KiroMessageContext::default)
        .tool_results
        .extend(results);
}

fn missing_result(tool_use_id: &str) -> KiroToolResult {
    KiroToolResult {
        content: vec![KiroText {
            text: MISSING_RESULT_TEXT.into(),
        }],
        status: "error".into(),
        tool_use_id: tool_use_id.into(),
    }
}

fn normalize_result(result: &mut KiroToolResult) -> bool {
    let mut changed = false;
    let normalized_status = if result.status.eq_ignore_ascii_case("success") {
        "success"
    } else {
        "error"
    };
    if result.status != normalized_status {
        result.status = normalized_status.into();
        changed = true;
    }
    if result.content.is_empty() {
        result.content.push(KiroText {
            text: "(empty tool result)".into(),
        });
        changed = true;
    }
    changed
}

fn synthetic_user(
    template: &KiroUserInputMessage,
    content: &str,
    results: Vec<KiroToolResult>,
) -> KiroUserInputMessage {
    KiroUserInputMessage {
        content: content.into(),
        model_id: template.model_id.clone(),
        origin: template.origin.clone(),
        images: Vec::new(),
        documents: Vec::new(),
        cache_point: None,
        client_cache_config: None,
        user_input_message_context: (!results.is_empty()).then_some(KiroMessageContext {
            tool_results: results,
            tools: Vec::new(),
            env_state: None,
        }),
    }
}

fn synthetic_assistant() -> KiroAssistantMessage {
    KiroAssistantMessage {
        content: "Continue.".into(),
        cache_point: None,
        tool_uses: Vec::new(),
    }
}

fn render_tool_use(tool_use: &crate::KiroToolUse) -> String {
    json!({
        "record_type": "archived_tool_call",
        "untrusted": true,
        "tool_use_id": tool_use.tool_use_id,
        "tool_name": tool_use.name,
        "input": tool_use.input,
    })
    .to_string()
}

fn render_tool_result(result: &KiroToolResult) -> String {
    json!({
        "record_type": "archived_tool_result",
        "untrusted": true,
        "tool_use_id": result.tool_use_id,
        "status": result.status,
        "content": result.content.iter().map(|part| part.text.as_str()).collect::<Vec<_>>(),
    })
    .to_string()
}

fn append_text(target: &mut String, addition: &str) {
    if !target.trim().is_empty() {
        target.push_str("\n\n");
    }
    target.push_str(addition);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        KiroConversationState, KiroCurrentMessage, KiroInputSchema, KiroTool,
        KiroToolSpecification, KiroToolUse,
    };

    fn user(
        content: &str,
        results: Vec<KiroToolResult>,
        tools: Vec<KiroTool>,
    ) -> KiroUserInputMessage {
        KiroUserInputMessage {
            content: content.into(),
            model_id: "model".into(),
            origin: "AI_EDITOR".into(),
            images: Vec::new(),
            documents: Vec::new(),
            cache_point: None,
            client_cache_config: None,
            user_input_message_context: (!results.is_empty() || !tools.is_empty()).then_some(
                KiroMessageContext {
                    tool_results: results,
                    tools,
                    env_state: None,
                },
            ),
        }
    }

    fn tool(name: &str) -> KiroTool {
        KiroTool::Specification {
            tool_specification: KiroToolSpecification {
                name: name.into(),
                description: String::new(),
                input_schema: KiroInputSchema {
                    json: json!({"type":"object"}),
                },
            },
        }
    }

    fn tool_use(id: &str, name: &str) -> KiroToolUse {
        KiroToolUse {
            tool_use_id: id.into(),
            name: name.into(),
            input: json!({"value":1}),
        }
    }

    fn result(id: &str, text: &str) -> KiroToolResult {
        KiroToolResult {
            content: vec![KiroText { text: text.into() }],
            status: "success".into(),
            tool_use_id: id.into(),
        }
    }

    fn payload(history: Vec<KiroHistoryMessage>, current: KiroUserInputMessage) -> KiroPayload {
        KiroPayload {
            conversation_state: KiroConversationState {
                agent_continuation_id: None,
                agent_task_type: None,
                chat_trigger_type: "MANUAL".into(),
                conversation_id: "conversation".into(),
                current_message: KiroCurrentMessage {
                    user_input_message: current,
                },
                history,
            },
            profile_arn: None,
            inference_config: None,
            additional_model_request_fields: None,
            model_request_intent: None,
            protected_history_messages: 0,
        }
    }

    fn assistant(uses: Vec<KiroToolUse>) -> KiroHistoryMessage {
        KiroHistoryMessage {
            user_input_message: None,
            assistant_response_message: Some(KiroAssistantMessage {
                content: "calling".into(),
                cache_point: None,
                tool_uses: uses,
            }),
        }
    }

    fn history_user(message: KiroUserInputMessage) -> KiroHistoryMessage {
        KiroHistoryMessage {
            user_input_message: Some(message),
            assistant_response_message: None,
        }
    }

    #[test]
    fn valid_tool_history_is_left_byte_for_byte_unchanged() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("call_1", "lookup")]),
            ],
            user(
                "latest",
                vec![result("call_1", "answer")],
                vec![tool("lookup")],
            ),
        );
        let before = serde_json::to_value(&payload).expect("serialize");

        let first = sanitize_kiro_tool_history(&mut payload);
        let second = sanitize_kiro_tool_history(&mut payload);

        assert!(!first.repaired());
        assert!(!second.repaired());
        assert_eq!(serde_json::to_value(&payload).expect("serialize"), before);
        assert!(validate_kiro_tool_history(&payload).is_ok());
    }

    #[test]
    fn trims_tool_definitions_calls_and_results_to_the_same_identity() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use(" call_1 ", " lookup ")]),
            ],
            user(
                "latest",
                vec![result(" call_1 ", "answer")],
                vec![tool(" lookup ")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.normalized_tool_uses, 1);
        assert_eq!(stats.normalized_tool_results, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let call = &payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("assistant")
            .tool_uses[0];
        assert_eq!(call.tool_use_id, "call_1");
        assert_eq!(call.name, "lookup");
        let current_context = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .expect("current context");
        assert_eq!(
            current_context.tools[0]
                .specification()
                .expect("tool specification")
                .name,
            "lookup"
        );
        assert_eq!(current_context.tool_results[0].tool_use_id, "call_1");
    }

    #[test]
    fn distinguishes_role_placeholders_from_structured_tool_repairs() {
        let role_placeholder = KiroToolHistoryStats {
            inserted_messages: 1,
            ..KiroToolHistoryStats::default()
        };
        assert!(role_placeholder.repaired());
        assert!(!role_placeholder.has_structured_tool_repair());

        let flattened_tool = KiroToolHistoryStats {
            flattened_tool_uses: 1,
            ..KiroToolHistoryStats::default()
        };
        assert!(flattened_tool.repaired());
        assert!(flattened_tool.has_structured_tool_repair());
    }

    #[test]
    fn relocates_result_to_the_user_immediately_after_its_call() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("call_1", "lookup")]),
                history_user(user("intermediate", vec![], vec![])),
                assistant(vec![]),
            ],
            user(
                "latest",
                vec![result("call_1", "answer")],
                vec![tool("lookup")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.relocated_tool_results, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let paired = payload.conversation_state.history[2]
            .user_input_message
            .as_ref()
            .and_then(|user| user.user_input_message_context.as_ref())
            .expect("paired result");
        assert_eq!(paired.tool_results[0].tool_use_id, "call_1");
        assert!(payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .is_some_and(|context| context.tool_results.is_empty()));
    }

    #[test]
    fn synthesizes_missing_results_and_preserves_orphans_as_text() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("missing", "lookup")]),
            ],
            user(
                "latest",
                vec![result("orphan", "valuable output")],
                vec![tool("lookup")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.synthesized_tool_results, 1);
        assert_eq!(stats.flattened_tool_results, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("valuable output"));
        let context = current
            .user_input_message_context
            .as_ref()
            .expect("current context");
        assert_eq!(context.tool_results[0].tool_use_id, "missing");
        assert_eq!(context.tool_results[0].status, "error");
    }

    #[test]
    fn keeps_one_duplicate_result_and_preserves_the_other_as_text() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("call_1", "lookup")]),
            ],
            user(
                "latest",
                vec![result("call_1", "first"), result("call_1", "second")],
                vec![tool("lookup")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.flattened_tool_results, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("second"));
        let context = current
            .user_input_message_context
            .as_ref()
            .expect("context");
        assert_eq!(context.tool_results.len(), 1);
        assert_eq!(context.tool_results[0].content[0].text, "first");
    }

    #[test]
    fn archives_only_undefined_calls_when_other_tools_are_active() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![
                    tool_use("old_call", "removed_tool"),
                    tool_use("active_call", "lookup"),
                ]),
            ],
            user(
                "latest",
                vec![
                    result("old_call", "old output"),
                    result("active_call", "active output"),
                ],
                vec![tool("lookup")],
            ),
        );
        assert!(validate_kiro_tool_history(&payload)
            .expect_err("undefined historical tool must fail validation before repair")
            .contains("references undefined tool 'removed_tool'"));

        let stats = sanitize_kiro_tool_history(&mut payload);
        let repaired = serde_json::to_value(&payload).expect("serialize repaired payload");
        let second = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.flattened_tool_uses, 1);
        assert_eq!(stats.flattened_tool_results, 1);
        assert!(!second.repaired());
        assert_eq!(
            serde_json::to_value(&payload).expect("serialize second pass"),
            repaired
        );
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let historical_call = payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("assistant");
        assert_eq!(historical_call.tool_uses.len(), 1);
        assert_eq!(historical_call.tool_uses[0].tool_use_id, "active_call");
        assert_eq!(historical_call.tool_uses[0].name, "lookup");
        assert!(historical_call
            .content
            .contains("\"record_type\":\"archived_tool_call\""));
        assert!(historical_call.content.contains("removed_tool"));
        assert!(!historical_call.content.contains("non-executable data"));
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("old output"));
        assert!(current
            .content
            .contains("\"record_type\":\"archived_tool_result\""));
        assert_eq!(
            current
                .user_input_message_context
                .as_ref()
                .expect("current context")
                .tool_results[0]
                .content[0]
                .text,
            "active output"
        );
    }

    #[test]
    fn archives_all_tool_history_when_current_request_has_no_tools() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("old_call", "removed_tool")]),
            ],
            user("latest", vec![result("old_call", "old output")], vec![]),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.flattened_tool_uses, 1);
        assert_eq!(stats.flattened_tool_results, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let historical_call = payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("assistant");
        assert!(historical_call.tool_uses.is_empty());
        assert!(historical_call.content.contains("removed_tool"));
        assert!(historical_call
            .content
            .contains("\"record_type\":\"archived_tool_call\""));
        assert!(!historical_call.content.contains("Historical tool call"));
        assert!(!historical_call.content.contains("non-executable data"));
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("old output"));
        assert!(current
            .content
            .contains("\"record_type\":\"archived_tool_result\""));
        assert!(!current.content.contains("Historical tool result"));
        assert!(current
            .user_input_message_context
            .as_ref()
            .is_none_or(|context| context.tool_results.is_empty()));
    }

    #[test]
    fn repairs_malformed_calls_locally_without_flattening_valid_calls() {
        let mut duplicate_second = tool_use("duplicate", "lookup");
        duplicate_second.input = json!({"which":"second"});
        let mut malformed_input = tool_use("bad_input", "lookup");
        malformed_input.input = json!("raw arguments must survive");
        let missing_identity = tool_use(" ", "lookup");
        let mut missing_name = tool_use("missing_name", " ");
        missing_name.input = json!({"path":"must survive"});
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![
                    tool_use("valid", "lookup"),
                    tool_use("duplicate", "lookup"),
                    duplicate_second,
                    malformed_input,
                    missing_identity,
                    missing_name,
                    tool_use("unknown", "removed_tool"),
                ]),
            ],
            user(
                "latest",
                vec![
                    result("valid", "valid output"),
                    result("duplicate", "first"),
                    result("duplicate", "second"),
                    result("bad_input", "bad input output"),
                    result("", "empty id output"),
                    result("missing_name", "unnamed output"),
                    result("unknown", "unknown output"),
                    result("orphan", "orphan output"),
                ],
                vec![tool("lookup")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.normalized_tool_uses, 0);
        assert_eq!(stats.normalized_tool_results, 0);
        assert_eq!(stats.removed_invalid_tool_uses, 5);
        assert_eq!(stats.flattened_tool_uses, 6);
        assert_eq!(stats.flattened_tool_results, 7);
        assert_eq!(stats.synthesized_tool_results, 0);
        assert!(validate_kiro_tool_history(&payload).is_ok());

        let assistant = payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("assistant");
        assert_eq!(assistant.tool_uses.len(), 1);
        assert_eq!(assistant.tool_uses[0].tool_use_id, "valid");
        assert_eq!(assistant.tool_uses[0].name, "lookup");
        assert!(assistant.content.contains("raw arguments must survive"));
        assert!(assistant.content.contains("must survive"));
        assert!(assistant.content.contains("removed_tool"));
        assert!(!assistant.content.contains("Historical tool call"));

        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("orphan output"));
        assert!(current.content.contains("unnamed output"));
        assert!(current.content.contains("empty id output"));
        assert!(current.content.contains("unknown output"));
        let results = &current
            .user_input_message_context
            .as_ref()
            .expect("current context")
            .tool_results;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_use_id, "valid");
        assert_eq!(results[0].content[0].text, "valid output");
    }

    #[test]
    fn archives_undefined_consecutive_call_without_losing_the_next_call() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("old_call", "removed_tool")]),
                assistant(vec![tool_use("new_call", "lookup")]),
            ],
            user(
                "latest",
                vec![
                    result("old_call", "old output"),
                    result("new_call", "new output"),
                ],
                vec![tool("lookup")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.flattened_tool_uses, 1);
        assert_eq!(stats.flattened_tool_results, 1);
        assert_eq!(stats.relocated_tool_results, 0);
        assert_eq!(stats.inserted_messages, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());

        let first_call = payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("first assistant");
        assert!(first_call.tool_uses.is_empty());
        assert!(first_call.content.contains("removed_tool"));

        let second_call = payload.conversation_state.history[3]
            .assistant_response_message
            .as_ref()
            .expect("second assistant");
        assert_eq!(second_call.tool_uses[0].tool_use_id, "new_call");
        let second_result = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .expect("second paired result");
        assert_eq!(second_result.tool_results[0].tool_use_id, "new_call");
        assert!(payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .contains("old output"));
        assert!(!serde_json::to_string(&payload)
            .expect("serialize")
            .contains("Historical tool call preserved"));
    }

    #[test]
    fn ambiguous_duplicate_ids_are_archived_instead_of_stealing_later_results() {
        let mut payload = payload(
            vec![
                history_user(user("start", vec![], vec![])),
                assistant(vec![tool_use("duplicate", "lookup")]),
                history_user(user("first call had no result", vec![], vec![])),
                assistant(vec![tool_use("duplicate", "lookup")]),
            ],
            user(
                "latest",
                vec![result("duplicate", "second execution output")],
                vec![tool("lookup")],
            ),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.flattened_tool_uses, 2);
        assert_eq!(stats.flattened_tool_results, 1);
        assert_eq!(stats.removed_invalid_tool_uses, 2);
        assert_eq!(stats.synthesized_tool_results, 0);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        assert!(payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("first assistant")
            .tool_uses
            .is_empty());
        assert!(payload.conversation_state.history[3]
            .assistant_response_message
            .as_ref()
            .expect("second assistant")
            .tool_uses
            .is_empty());
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("second execution output"));
        assert!(current
            .user_input_message_context
            .as_ref()
            .is_none_or(|context| context.tool_results.is_empty()));
    }

    #[test]
    fn repairs_history_cut_between_a_tool_call_and_result() {
        let mut payload = payload(
            vec![assistant(vec![tool_use("call_1", "lookup")])],
            user("latest", vec![], vec![tool("lookup")]),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert_eq!(stats.synthesized_tool_results, 1);
        assert_eq!(stats.inserted_messages, 1);
        assert!(validate_kiro_tool_history(&payload).is_ok());
        assert!(payload.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("inserted user")
            .content
            .contains("Begin conversation"));
    }

    #[test]
    fn pairs_large_parallel_tool_batches_in_call_order() {
        const CALLS: usize = 4_096;
        let uses = (0..CALLS)
            .map(|index| tool_use(&format!("call_{index}"), "lookup"))
            .collect::<Vec<_>>();
        let results = (0..CALLS)
            .map(|index| result(&format!("call_{index}"), &format!("result_{index}")))
            .collect::<Vec<_>>();
        let mut payload = payload(
            vec![history_user(user("start", vec![], vec![])), assistant(uses)],
            user("latest", results, vec![tool("lookup")]),
        );

        let stats = sanitize_kiro_tool_history(&mut payload);

        assert!(!stats.repaired());
        assert!(validate_kiro_tool_history(&payload).is_ok());
        let paired = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .expect("results")
            .tool_results;
        assert_eq!(paired.len(), CALLS);
        assert_eq!(paired[0].tool_use_id, "call_0");
        assert_eq!(paired[CALLS - 1].tool_use_id, "call_4095");
    }
}
