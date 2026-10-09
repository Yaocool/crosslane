use super::{
    attempt_diagnostics, check_context_limit, compact_tool_search_continuation,
    empty_tool_result_disposition, estimated_credits, fallback_credits, fill_missing_usage,
    loaded_tool_count, loaded_tool_names, meter_error, now_secs, resolve_dynamic_model,
    resume_web_search_payload, sanitize_error_message, sanitize_kiro_tool_history,
    tool_search_budget_with_compaction, tool_search_continue_payload_batch, upstream_error,
    validate_kiro_tool_history, web_search_continue_payload_batch, AccountLease, ApiError,
    AppState, Arc, ClaudeContextEditStats, ClaudeRequest, ClaudeServerEvent,
    ClaudeToolSearchBudget, ClaudeToolSearchCatalog, ClaudeWebSearchTrace,
    CompactionIterationUsage, CreditReservation, DecodedResponse, DispatchFailure,
    EmptyToolResultDisposition, ErrorFormat, ExecuteError, HashSet, Instant, IntoResponse, Json,
    KiroError, KiroEvent, KiroResponse, OpenAiRequest, OpenAiToolIdentity, PoolError,
    PreparedUpstream, PromptCacheProfile, RequestDiagnostics, RequestLog, RequestLogContext,
    Response, Rng, StopSequenceFilter, ThinkingContentFilter, ToolLeakFilter, UpstreamAttemptLog,
    UpstreamExecution, UsageRecord, Uuid, Value,
};

mod dispatch;

pub(super) use dispatch::{execute_upstream, prepare_upstream};

pub(super) fn build_model_path(original: &str, mapped: &str, kiro: &str) -> Vec<String> {
    let mut path = Vec::new();
    push_model_path(&mut path, original);
    push_model_path(&mut path, mapped);
    push_model_path(&mut path, kiro);
    path
}

pub(super) fn push_model_path(path: &mut Vec<String>, model: &str) {
    if !model.is_empty() && path.last().is_none_or(|last| last != model) {
        path.push(model.to_owned());
    }
}

pub(super) fn upstream_attempt_log(
    attempt: u32,
    account: &crosslane_core::account::Account,
    model: &str,
    error: &KiroError,
) -> UpstreamAttemptLog {
    UpstreamAttemptLog {
        attempt,
        account_id: account.id.clone(),
        account_name: account.display_name().to_owned(),
        model: model.to_owned(),
        available_models: Vec::new(),
        endpoint: error.endpoint.clone(),
        status: error.status,
        error: sanitize_error_message(&error.message),
    }
}

pub(super) fn dispatch_error(
    error: KiroError,
    mapped_model: &str,
    kiro_model: &str,
    model_path: &[String],
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
) -> ExecuteError {
    let (account_id, account_name) = attempts
        .last()
        .map(|attempt| (attempt.account_id.clone(), attempt.account_name.clone()))
        .unwrap_or_default();
    ExecuteError::Dispatch(Box::new(DispatchFailure {
        context: RequestLogContext {
            provider_id: "kiro".into(),
            account_id,
            account_name,
            endpoint: error.endpoint.clone(),
            mapped_model: mapped_model.to_owned(),
            kiro_model: kiro_model.to_owned(),
            model_path: model_path.to_vec(),
            model_mapping_rule,
            attempts,
        },
        error,
    }))
}

pub(super) fn prepend_attempt_logs(
    attempts: &mut Vec<UpstreamAttemptLog>,
    mut previous: Vec<UpstreamAttemptLog>,
) {
    if previous.is_empty() {
        return;
    }
    previous.append(attempts);
    for (index, attempt) in previous.iter_mut().enumerate() {
        attempt.attempt = index as u32 + 1;
    }
    *attempts = previous;
}

pub(super) fn prepend_execute_error_attempts(
    error: &mut ExecuteError,
    previous: Vec<UpstreamAttemptLog>,
) {
    if let ExecuteError::Dispatch(failure) = error {
        prepend_attempt_logs(&mut failure.context.attempts, previous);
    }
}

pub(super) fn retry_attempt_count(max_retries: u32, account_count: u32) -> u32 {
    max_retries.saturating_add(1).min(account_count).max(1)
}

pub(in crate::http) fn set_payload_model(
    payload: &mut crosslane_translate::KiroPayload,
    model: &str,
) {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id = model.into();
    for message in &mut payload.conversation_state.history {
        if let Some(user) = &mut message.user_input_message {
            user.model_id = model.into();
        }
    }
}

/// Resolve common client aliases during the cold-start window before an
/// account's dynamic model cache has been populated. The catalog is filtered
/// by subscription so this cannot route a premium model through a Free account.
pub(in crate::http) fn resolve_static_model(
    account: &crosslane_core::account::Account,
    model: &str,
) -> Option<String> {
    let available = crosslane_kiro::static_models_for_account(account)
        .into_iter()
        .map(|model| model.model_id)
        .collect::<Vec<_>>();
    resolve_dynamic_model(model, &available)
}

pub(in crate::http) fn find_model_fallback(
    model: &str,
    models: &[crosslane_kiro::ModelInfo],
) -> Option<String> {
    let (family, version) = model_family(model)?;
    models
        .iter()
        .filter_map(|candidate| {
            let (candidate_family, candidate_version) = model_family(&candidate.model_id)?;
            (candidate_family == family && candidate_version < version)
                .then_some((candidate_version, candidate.model_id.clone()))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, model)| model)
}

pub(super) fn model_family(model: &str) -> Option<(String, Vec<u32>)> {
    let lower = model.to_ascii_lowercase().replace('.', "-");
    let mut family = Vec::new();
    let mut version = Vec::new();
    let mut found_version = false;
    for part in lower.split('-').filter(|part| !part.is_empty()) {
        if let Ok(number) = part.parse::<u32>() {
            found_version = true;
            version.push(number);
        } else if !found_version {
            family.push(part);
        }
    }
    (!family.is_empty() && !version.is_empty()).then(|| (family.join("-"), version))
}

pub(super) fn push_nonstream_event(
    decoded: &mut DecodedResponse,
    stop_filter: &mut StopSequenceFilter,
    visible_text: &mut String,
    event: KiroEvent,
) -> Result<bool, String> {
    if matches!(
        event,
        KiroEvent::Reasoning { .. } | KiroEvent::Citations { .. } | KiroEvent::ToolUse { .. }
    ) {
        visible_text.push_str(&stop_filter.finish());
    }
    if let KiroEvent::AssistantResponse { content } = &event {
        visible_text.push_str(&stop_filter.push(content));
    }
    decoded.push(event)?;
    let Some(sequence) = stop_filter.matched().map(str::to_owned) else {
        return Ok(false);
    };
    decoded.stop_at_sequence(std::mem::take(visible_text), sequence);
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn collect_nonstream_rounds(
    state: &Arc<AppState>,
    trace_id: &str,
    lease: &AccountLease,
    reservation: &mut CreditReservation,
    mut upstream: KiroResponse,
    mut payload: crosslane_translate::KiroPayload,
    mut compact: bool,
    max_output_tokens: Option<u32>,
    stop_sequences: &[String],
    thinking_enabled: bool,
    tool_search: Option<&Arc<ClaudeToolSearchCatalog>>,
    max_tool_search_operations: u32,
    mut tool_search_operations: u32,
    web_search_max_rounds: u32,
    web_search_client_limit: bool,
    resumed_tool_searches: Vec<crosslane_translate::ClaudeToolSearchTrace>,
    resumed_web_searches: Vec<ClaudeWebSearchTrace>,
    resumed_server_events: Vec<ClaudeServerEvent>,
) -> Result<(DecodedResponse, String, u64), ExecuteError> {
    let config = state.config.current();
    let max_tool_search_rounds = config.features.tool_search_max_rounds.clamp(1, 8);
    let client_has_tools = payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .is_some_and(|context| !context.tools.is_empty());
    let mut decoded = DecodedResponse::default();
    let mut accumulated_usage = crosslane_kiro::UsageInfo::default();
    let mut accumulated_text = String::new();
    let mut accumulated_reasoning = String::new();
    let mut accumulated_searches = resumed_tool_searches;
    let mut accumulated_web_searches = resumed_web_searches;
    let mut accumulated_server_events = resumed_server_events;
    let mut compaction_stats: Option<crosslane_translate::ContextCompactionStats> = None;
    let mut round = 0;
    let mut search_round = 0;
    let mut empty_tool_result_retries = 0u8;
    let mut web_search_round = accumulated_web_searches
        .iter()
        .filter(|search| search.executed)
        .count() as u32;
    loop {
        let effective_thinking = thinking_enabled && upstream.thinking_enabled();
        let (endpoint_definition, response, _upstream_permit) = upstream.into_parts();
        let endpoint = endpoint_definition.name.to_string();
        let events = state
            .kiro()
            .collect_events(response)
            .await
            .map_err(ExecuteError::Upstream)?;
        let event_count = events.len();
        let mut leak_filter =
            ToolLeakFilter::for_payload(config.features.enable_tool_leak_filter, &payload);
        let mut thinking_filter = ThinkingContentFilter::new(effective_thinking)
            .with_omitted_summary(payload.thinking_summary_omitted());
        let mut stop_filter = StopSequenceFilter::new(stop_sequences);
        let mut visible_text = String::new();
        let mut stop_matched = false;
        'events: for event in events {
            for event in leak_filter.push(event) {
                for event in thinking_filter.push(event) {
                    if push_nonstream_event(
                        &mut decoded,
                        &mut stop_filter,
                        &mut visible_text,
                        event,
                    )
                    .map_err(|message| {
                        ExecuteError::Upstream(KiroError {
                            status: None,
                            endpoint: endpoint.clone(),
                            message,
                        })
                    })? {
                        stop_matched = true;
                        break 'events;
                    }
                }
            }
        }
        if !stop_matched {
            let mut trailing_events = Vec::new();
            for event in leak_filter.finish() {
                trailing_events.extend(thinking_filter.push(event));
            }
            trailing_events.extend(thinking_filter.finish());
            for event in trailing_events {
                if push_nonstream_event(&mut decoded, &mut stop_filter, &mut visible_text, event)
                    .map_err(|message| {
                        ExecuteError::Upstream(KiroError {
                            status: None,
                            endpoint: endpoint.clone(),
                            message,
                        })
                    })?
                {
                    stop_matched = true;
                    break;
                }
            }
        }
        if !stop_matched {
            let _trailing = stop_filter.finish();
        }
        if decoded.stop_sequence.is_none() {
            decoded.finalize_tool_inputs().map_err(|message| {
                ExecuteError::Upstream(KiroError {
                    status: None,
                    endpoint: endpoint.clone(),
                    message,
                })
            })?;
        }
        fill_missing_usage(state, &mut decoded, &payload).await;
        if decoded.stop_sequence.is_some() {
            accumulated_text.push_str(&decoded.text);
            accumulated_reasoning.push_str(&decoded.reasoning);
            decoded.text = accumulated_text;
            decoded.reasoning = accumulated_reasoning;
            decoded.tool_searches = accumulated_searches;
            decoded.web_searches = accumulated_web_searches;
            decoded.claude_server_events = accumulated_server_events;
            merge_round_usage(&mut decoded.usage, &accumulated_usage);
            let total_output_tokens = decoded.usage.output_tokens;
            decoded.automatic_compaction = compaction_stats;
            return Ok((decoded, endpoint, total_output_tokens));
        }
        let output_exhausted = max_output_tokens.is_some_and(|maximum| {
            accumulated_usage
                .output_tokens
                .saturating_add(decoded.usage.output_tokens)
                >= u64::from(maximum)
        });

        match empty_tool_result_disposition(
            &payload,
            &decoded,
            output_exhausted,
            empty_tool_result_retries,
        ) {
            EmptyToolResultDisposition::Accept => {}
            EmptyToolResultDisposition::Retry => {
                empty_tool_result_retries += 1;
                tracing::warn!(
                    trace_id,
                    account_id = %lease.account_id(),
                    endpoint,
                    retry = empty_tool_result_retries,
                    "upstream returned an empty assistant turn after tool results; retrying"
                );
                merge_round_usage(&mut accumulated_usage, &decoded.usage);
                decoded = DecodedResponse::default();
                let budget_available = apply_remaining_output_budget(
                    &mut payload,
                    max_output_tokens,
                    accumulated_usage.output_tokens,
                );
                debug_assert!(budget_available);
                let next_input_tokens = validate_internal_continuation(
                    state,
                    &mut payload,
                    compact,
                    &endpoint,
                    "empty tool-result retry",
                    tool_search.is_some(),
                )
                .await
                .map_err(ExecuteError::Upstream)?;
                let continuation_estimate = estimated_credits(
                    next_input_tokens,
                    payload
                        .max_output_tokens()
                        .or(max_output_tokens)
                        .unwrap_or(super::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                    &config.pool,
                );
                reservation
                    .extend(continuation_estimate)
                    .map_err(ExecuteError::Meter)?;
                let account = lease.account().await;
                upstream = state
                    .generate(&account, &payload)
                    .await
                    .map_err(ExecuteError::Upstream)?;
                continue;
            }
            EmptyToolResultDisposition::Fail => {
                tracing::error!(
                    trace_id,
                    account_id = %lease.account_id(),
                    endpoint,
                    "upstream repeated an empty assistant turn after tool results"
                );
                return Err(ExecuteError::Upstream(KiroError {
                    status: None,
                    endpoint,
                    message: "upstream returned no assistant text or tool call after tool results"
                        .into(),
                }));
            }
        }

        let search_uses = tool_search
            .map(|catalog| decoded.take_tool_uses_where(|tool| catalog.is_search_tool(&tool.name)))
            .unwrap_or_default();
        if let Some(catalog) = tool_search.filter(|_| !search_uses.is_empty()) {
            let parallel_web_uses = if web_search_max_rounds > 0 {
                decoded.take_tool_uses_where(|tool| tool.name == "web_search")
            } else {
                Vec::new()
            };
            let round_text = std::mem::take(&mut decoded.text);
            let mut preceding_text = std::mem::take(&mut accumulated_text);
            preceding_text.push_str(&round_text);

            if search_round >= max_tool_search_rounds {
                for search_use in &search_uses {
                    let mut trace = catalog.pending_trace(search_use);
                    trace.id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                    accumulated_server_events.push(ClaudeServerEvent::ToolSearch {
                        index: accumulated_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_searches.push(trace);
                }
                for search_use in &parallel_web_uses {
                    accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                        index: accumulated_web_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_web_searches.push(ClaudeWebSearchTrace::pending(
                        format!("srvtoolu_{}", Uuid::new_v4().simple()),
                        search_use.input.clone(),
                    ));
                }
                accumulated_reasoning.push_str(&decoded.reasoning);
                decoded.reasoning = accumulated_reasoning;
                decoded.tool_searches = accumulated_searches;
                decoded.web_searches = accumulated_web_searches;
                decoded.claude_server_events = accumulated_server_events;
                if decoded.tools.is_empty() {
                    decoded.stop_reason = Some(
                        if output_exhausted {
                            "max_tokens"
                        } else {
                            "pause_turn"
                        }
                        .into(),
                    );
                }
                merge_round_usage(&mut decoded.usage, &accumulated_usage);
                let total_output_tokens = decoded.usage.output_tokens;
                decoded.automatic_compaction = compaction_stats;
                return Ok((decoded, endpoint, total_output_tokens));
            }

            // Anthropic leaves server calls pending when the same assistant
            // turn also contains client tool calls. The client returns those
            // results and replays these server_tool_use blocks next turn.
            if !decoded.tools.is_empty() || output_exhausted {
                for search_use in &search_uses {
                    let mut trace = catalog.pending_trace(search_use);
                    trace.id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                    accumulated_server_events.push(ClaudeServerEvent::ToolSearch {
                        index: accumulated_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_searches.push(trace);
                }
                for search_use in &parallel_web_uses {
                    accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                        index: accumulated_web_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_web_searches.push(ClaudeWebSearchTrace::pending(
                        format!("srvtoolu_{}", Uuid::new_v4().simple()),
                        search_use.input.clone(),
                    ));
                }
                accumulated_reasoning.push_str(&decoded.reasoning);
                decoded.reasoning = accumulated_reasoning;
                decoded.tool_searches = accumulated_searches;
                decoded.web_searches = accumulated_web_searches;
                decoded.claude_server_events = accumulated_server_events;
                if output_exhausted && decoded.tools.is_empty() {
                    decoded.stop_reason = Some("max_tokens".into());
                }
                merge_round_usage(&mut decoded.usage, &accumulated_usage);
                let total_output_tokens = decoded.usage.output_tokens;
                decoded.automatic_compaction = compaction_stats;
                return Ok((decoded, endpoint, total_output_tokens));
            }

            let mut budget = tool_search_budget_with_compaction(state, &payload, compact)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })?;
            let mut loaded_names = loaded_tool_names(&payload);
            let mut searches = Vec::with_capacity(search_uses.len());
            for search_use in search_uses {
                let mut outcome = if tool_search_operations >= max_tool_search_operations {
                    catalog.unavailable_outcome(
                        &search_use,
                        format!(
                            "Tool Search operation limit of {max_tool_search_operations} was reached"
                        ),
                    )
                } else {
                    tool_search_operations += 1;
                    match catalog
                        .search_with_budget_excluding_async(
                            search_use.clone(),
                            budget,
                            loaded_names.clone(),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => catalog.unavailable_outcome(&search_use, error),
                    }
                };
                outcome.trace.id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                let consumed_bytes = outcome
                    .tools
                    .iter()
                    .filter_map(|tool| serde_json::to_vec(tool).ok())
                    .map(|tool| tool.len())
                    .sum::<usize>()
                    .saturating_add(outcome.documentation.iter().map(String::len).sum());
                budget = ClaudeToolSearchBudget {
                    max_tools: budget.max_tools.saturating_sub(outcome.tools.len()),
                    max_bytes: budget.max_bytes.saturating_sub(consumed_bytes),
                };
                loaded_names.extend(
                    outcome
                        .tools
                        .iter()
                        .filter_map(|tool| tool.specification().map(|tool| tool.name.clone())),
                );
                accumulated_server_events.push(ClaudeServerEvent::ToolSearch {
                    index: accumulated_searches.len(),
                    preceding_text: std::mem::take(&mut preceding_text),
                });
                accumulated_searches.push(outcome.trace.clone());
                searches.push((search_use, outcome));
            }
            let mut parallel_web_searches = Vec::with_capacity(parallel_web_uses.len());
            for search_use in parallel_web_uses {
                let query = search_use
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let server_id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                let trace = if web_search_round >= web_search_max_rounds {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        if web_search_client_limit {
                            "max_uses_exceeded"
                        } else {
                            "unavailable"
                        },
                        format!("web search is limited to {web_search_max_rounds} uses"),
                    )
                } else if query.is_empty() {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        "invalid_tool_input",
                        "web search query must not be empty".into(),
                    )
                } else {
                    web_search_round += 1;
                    match execute_kiro_web_search(state, lease, &query).await {
                        Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                        Err(error) => ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            web_search_error_code(&error),
                            sanitize_error_message(&error.to_string()),
                        )
                        .executed(),
                    }
                };
                accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                    index: accumulated_web_searches.len(),
                    preceding_text: std::mem::take(&mut preceding_text),
                });
                accumulated_web_searches.push(trace.clone());
                parallel_web_searches.push((search_use, trace));
            }
            let documentation_tokens = state
                .tokenizer
                .count(
                    searches
                        .iter()
                        .flat_map(|(_, outcome)| outcome.documentation.iter())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })? as u64;
            tracing::info!(
                trace_id,
                account_id = %lease.account_id(),
                endpoint,
                search_round = search_round + 1,
                search_count = searches.len(),
                matched_tools = searches.iter().map(|(_, outcome)| outcome.trace.references.len()).sum::<usize>(),
                result_truncated = searches.iter().any(|(_, outcome)| outcome.trace.budget_truncated),
                search_error = searches.iter().any(|(_, outcome)| outcome.trace.error.is_some()),
                "proxy Tool Search batch executed"
            );
            accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
            payload = tool_search_continue_payload_batch(&payload, &round_text, &searches);
            empty_tool_result_retries = 0;
            if !parallel_web_searches.is_empty() {
                if let Some(assistant) = payload
                    .conversation_state
                    .history
                    .last_mut()
                    .and_then(|message| message.assistant_response_message.as_mut())
                {
                    assistant.tool_uses.extend(
                        parallel_web_searches
                            .iter()
                            .map(|(tool_use, _)| tool_use.clone()),
                    );
                }
                for (tool_use, trace) in &parallel_web_searches {
                    resume_web_search_payload(&mut payload, tool_use, trace);
                }
            }
            merge_round_usage(&mut accumulated_usage, &decoded.usage);
            decoded = DecodedResponse::default();
            let budget_available = apply_remaining_output_budget(
                &mut payload,
                max_output_tokens,
                accumulated_usage.output_tokens,
            );
            debug_assert!(budget_available);
            prepare_kiro_payload(&mut payload, &endpoint, "Tool Search")
                .map_err(ExecuteError::Upstream)?;
            if let Some(mut stats) = compact_tool_search_continuation(state, &mut payload, compact)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })?
            {
                tracing::info!(
                    trace_id,
                    compaction_reason = "tool_search_continuation_overflow",
                    compaction_mode = "extractive_fallback",
                    original_input_tokens = stats.original_tokens,
                    compacted_input_tokens = stats.compacted_tokens,
                    removed_messages = stats.removed_messages,
                    "Tool Search continuation history compacted locally"
                );
                if let Some(previous) = &compaction_stats {
                    stats.original_tokens = previous.original_tokens;
                }
                compaction_stats = Some(stats);
                compact = true;
            }
            let next_input_tokens = state
                .tokenizer
                .estimate_kiro_payload(&payload)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })? as u64;
            let next_tool_tokens = (state
                .tokenizer
                .estimate_kiro_tools(&payload)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })? as u64)
                .saturating_add(documentation_tokens);
            let next_payload_bytes = serde_json::to_vec(&payload)
                .map_err(|error| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message: error.to_string(),
                    })
                })?
                .len();
            let next_loaded_tools = loaded_tool_count(&payload);
            let max_loaded_tools = config
                .context
                .max_loaded_tools
                .min(crosslane_translate::validate::MAX_TOOLS);
            if next_loaded_tools > max_loaded_tools {
                return Err(ExecuteError::Upstream(KiroError {
                    status: Some(400),
                    endpoint,
                    message: format!(
                        "too many loaded tools after Tool Search: {next_loaded_tools} > {max_loaded_tools}"
                    ),
                }));
            }
            if next_tool_tokens > u64::from(config.context.max_tool_input_tokens) {
                return Err(ExecuteError::Upstream(KiroError {
                    status: Some(400),
                    endpoint,
                    message: format!(
                        "loaded tool definitions are too large after Tool Search: {next_tool_tokens} estimated tokens > {}",
                        config.context.max_tool_input_tokens
                    ),
                }));
            }
            if next_payload_bytes > config.context.max_upstream_payload_bytes {
                return Err(ExecuteError::Upstream(KiroError {
                    status: Some(400),
                    endpoint,
                    message: format!(
                        "translated upstream payload is too large after Tool Search: {next_payload_bytes} bytes > {}",
                        config.context.max_upstream_payload_bytes
                    ),
                }));
            }
            let model = payload
                .conversation_state
                .current_message
                .user_input_message
                .model_id
                .clone();
            check_context_limit(state, next_input_tokens, compact, &model)
                .map_err(ExecuteError::ContextLimit)?;
            tracing::debug!(
                trace_id,
                input_tokens = next_input_tokens,
                tool_tokens = next_tool_tokens,
                payload_bytes = next_payload_bytes,
                loaded_tool_count = next_loaded_tools,
                "Tool Search continuation prepared"
            );
            let continuation_estimate = estimated_credits(
                next_input_tokens,
                payload
                    .max_output_tokens()
                    .or(max_output_tokens)
                    .unwrap_or(super::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                &config.pool,
            );
            if let Err(error) = reservation.extend(continuation_estimate) {
                return Err(ExecuteError::Meter(error));
            }
            let account = lease.account().await;
            upstream = state
                .generate(&account, &payload)
                .await
                .map_err(ExecuteError::Upstream)?;
            search_round += 1;
            continue;
        }

        let search_uses = if web_search_max_rounds > 0 {
            decoded.take_tool_uses_where(|tool| tool.name == "web_search")
        } else {
            Vec::new()
        };
        if !search_uses.is_empty() {
            let round_text = std::mem::take(&mut decoded.text);
            let mut preceding_text = std::mem::take(&mut accumulated_text);
            preceding_text.push_str(&round_text);

            // Preserve every server call, but leave it unresolved until the
            // client tool results from this mixed turn are returned.
            if !decoded.tools.is_empty() || output_exhausted {
                for search_use in &search_uses {
                    accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                        index: accumulated_web_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_web_searches.push(ClaudeWebSearchTrace::pending(
                        format!("srvtoolu_{}", Uuid::new_v4().simple()),
                        search_use.input.clone(),
                    ));
                }
                accumulated_reasoning.push_str(&decoded.reasoning);
                decoded.reasoning = accumulated_reasoning;
                decoded.tool_searches = accumulated_searches;
                decoded.web_searches = accumulated_web_searches;
                decoded.claude_server_events = accumulated_server_events;
                if output_exhausted && decoded.tools.is_empty() {
                    decoded.stop_reason = Some("max_tokens".into());
                }
                merge_round_usage(&mut decoded.usage, &accumulated_usage);
                let total_output_tokens = decoded.usage.output_tokens;
                decoded.automatic_compaction = compaction_stats;
                return Ok((decoded, endpoint, total_output_tokens));
            }

            let mut searches = Vec::with_capacity(search_uses.len());
            for search_use in search_uses {
                let query = search_use
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let server_id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                let trace = if web_search_round >= web_search_max_rounds {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        if web_search_client_limit {
                            "max_uses_exceeded"
                        } else {
                            "unavailable"
                        },
                        if web_search_client_limit {
                            format!("web search is limited to max_uses={web_search_max_rounds}")
                        } else {
                            format!(
                                "web search reached the proxy safety limit of {web_search_max_rounds} uses"
                            )
                        },
                    )
                } else if query.is_empty() {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        "invalid_tool_input",
                        "web search query must not be empty".into(),
                    )
                } else {
                    web_search_round += 1;
                    match execute_kiro_web_search(state, lease, &query).await {
                        Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                        Err(error) => ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            web_search_error_code(&error),
                            sanitize_error_message(&error.to_string()),
                        )
                        .executed(),
                    }
                };
                accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                    index: accumulated_web_searches.len(),
                    preceding_text: std::mem::take(&mut preceding_text),
                });
                accumulated_web_searches.push(trace.clone());
                searches.push((search_use, trace));
            }
            tracing::info!(
                trace_id,
                account_id = %lease.account_id(),
                endpoint,
                web_search_round,
                search_count = searches.len(),
                result_count = searches.iter().map(|(_, trace)| trace.results.len()).sum::<usize>(),
                search_error = searches.iter().any(|(_, trace)| trace.error.is_some()),
                "proxy Kiro MCP web search batch executed"
            );
            accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
            payload = web_search_continue_payload_batch(&payload, &round_text, &searches);
            empty_tool_result_retries = 0;
            merge_round_usage(&mut accumulated_usage, &decoded.usage);
            decoded = DecodedResponse::default();
            let budget_available = apply_remaining_output_budget(
                &mut payload,
                max_output_tokens,
                accumulated_usage.output_tokens,
            );
            debug_assert!(budget_available);
            let next_input_tokens = validate_internal_continuation(
                state,
                &mut payload,
                compact,
                &endpoint,
                "Web Search",
                tool_search.is_some(),
            )
            .await
            .map_err(ExecuteError::Upstream)?;
            let continuation_estimate = estimated_credits(
                next_input_tokens,
                payload
                    .max_output_tokens()
                    .or(max_output_tokens)
                    .unwrap_or(super::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                &config.pool,
            );
            if let Err(error) = reservation.extend(continuation_estimate) {
                return Err(ExecuteError::Meter(error));
            }
            let account = lease.account().await;
            upstream = state
                .generate(&account, &payload)
                .await
                .map_err(ExecuteError::Upstream)?;
            continue;
        }

        if client_has_tools
            || decoded.tools.is_empty()
            || output_exhausted
            || round >= config.features.auto_continue_rounds.min(30)
        {
            accumulated_text.push_str(&decoded.text);
            accumulated_reasoning.push_str(&decoded.reasoning);
            decoded.text = accumulated_text;
            decoded.reasoning = accumulated_reasoning;
            decoded.tool_searches = accumulated_searches;
            decoded.web_searches = accumulated_web_searches;
            decoded.claude_server_events = accumulated_server_events;
            if output_exhausted && decoded.tools.is_empty() {
                decoded.stop_reason = Some("max_tokens".into());
            }
            merge_round_usage(&mut decoded.usage, &accumulated_usage);
            let total_output_tokens = decoded.usage.output_tokens;
            tracing::debug!(
                trace_id,
                account_id = %lease.account_id(),
                endpoint,
                round = round + 1,
                event_count,
                input_tokens = decoded.usage.input_tokens,
                output_tokens = decoded.usage.output_tokens,
                tool_count = decoded.tools.len(),
                "upstream non-stream round decoded"
            );
            decoded.automatic_compaction = compaction_stats;
            return Ok((decoded, endpoint, total_output_tokens));
        }
        let uses = decoded
            .tools
            .values()
            .map(|tool| crosslane_translate::KiroToolUse {
                tool_use_id: tool.id.clone(),
                name: tool.name.clone(),
                input: super::super::response::repair_json(&tool.input),
            })
            .collect();
        let round_text = std::mem::take(&mut decoded.text);
        accumulated_text.push_str(&round_text);
        accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
        payload = crosslane_translate::auto_continue_payload(&payload, &round_text, uses);
        empty_tool_result_retries = 0;
        tracing::info!(
            trace_id,
            account_id = %lease.account_id(),
            endpoint,
            round = round + 1,
            event_count,
            output_tokens = decoded.usage.output_tokens,
            "starting automatic non-stream continuation"
        );
        decoded.tools.clear();
        merge_round_usage(&mut accumulated_usage, &decoded.usage);
        decoded.usage = crosslane_kiro::UsageInfo::default();
        let budget_available = apply_remaining_output_budget(
            &mut payload,
            max_output_tokens,
            accumulated_usage.output_tokens,
        );
        debug_assert!(budget_available);
        let next_input_tokens = validate_internal_continuation(
            state,
            &mut payload,
            compact,
            &endpoint,
            "automatic continuation",
            tool_search.is_some(),
        )
        .await
        .map_err(ExecuteError::Upstream)?;
        let continuation_estimate = estimated_credits(
            next_input_tokens,
            payload
                .max_output_tokens()
                .or(max_output_tokens)
                .unwrap_or(super::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
            &config.pool,
        );
        if let Err(error) = reservation.extend(continuation_estimate) {
            return Err(ExecuteError::Meter(error));
        }
        let account = lease.account().await;
        upstream = state
            .generate(&account, &payload)
            .await
            .map_err(ExecuteError::Upstream)?;
        round += 1;
    }
}

pub(in crate::http) fn web_search_error_code(error: &KiroError) -> &'static str {
    match error.status {
        Some(429) => "too_many_requests",
        Some(413) => "request_too_large",
        Some(400) if error.message.to_ascii_lowercase().contains("too long") => "query_too_long",
        Some(400) => "invalid_tool_input",
        _ => "unavailable",
    }
}

/// Applies the remaining client output budget to the next internal Kiro
/// continuation. Returns false when no model output may be generated.
pub(in crate::http) fn apply_remaining_output_budget(
    payload: &mut crosslane_translate::KiroPayload,
    maximum: Option<u32>,
    used: u64,
) -> bool {
    let Some(maximum) = maximum else {
        return true;
    };
    let remaining = u64::from(maximum).saturating_sub(used);
    if remaining == 0 {
        return false;
    }
    let inference = payload
        .inference_config
        .get_or_insert_with(Default::default);
    inference.max_tokens = Some(remaining.min(u64::from(u32::MAX)) as u32);
    true
}

pub(in crate::http) async fn execute_kiro_web_search(
    state: &Arc<AppState>,
    lease: &AccountLease,
    query: &str,
) -> Result<crosslane_translate::WebSearchResults, KiroError> {
    let account = lease.account().await;
    let account_id = account.id;
    let rejected_access_token = account.credentials.access_token;
    match execute_kiro_web_search_once(state, lease, query).await {
        Err(error) if error.is_auth() => {
            state
                .refresh_account_token_after_auth_failure(
                    &state.pool(),
                    &account_id,
                    &rejected_access_token,
                )
                .await
                .map_err(|refresh| KiroError {
                    status: error.status,
                    endpoint: "MCP web_search".into(),
                    message: format!("web search authentication recovery failed: {refresh}"),
                })?;
            execute_kiro_web_search_once(state, lease, query).await
        }
        result => result,
    }
}

pub(super) async fn execute_kiro_web_search_once(
    state: &Arc<AppState>,
    lease: &AccountLease,
    query: &str,
) -> Result<crosslane_translate::WebSearchResults, KiroError> {
    let account = ensure_web_search_profile_arn(state, lease).await?;
    state.kiro().web_search(&account, query).await
}

pub(super) async fn ensure_web_search_profile_arn(
    state: &Arc<AppState>,
    lease: &AccountLease,
) -> Result<crosslane_core::account::Account, KiroError> {
    for _attempt in 0..2 {
        let account = lease.account().await;
        if account.credentials.auth_method == crosslane_core::account::AuthMethod::ApiKey {
            return Ok(account);
        }
        let profile_arn = state.kiro().resolve_profile_arn(&account).await?;
        if account.profile_arn.as_deref() == Some(profile_arn.as_str()) {
            return Ok(account);
        }
        let pool = state.pool();
        let account_state = pool.get(&account.id).await.ok_or_else(|| KiroError {
            status: None,
            endpoint: "MCP web_search".into(),
            message: "web search account disappeared during profile discovery".into(),
        })?;
        let mut current = account_state.account.write().await;
        if current.credentials.access_token != account.credentials.access_token
            || current.credentials.region != account.credentials.region
            || current.profile_arn != account.profile_arn
        {
            continue;
        }
        current.profile_arn = Some(profile_arn);
        let resolved = current.clone();
        drop(current);

        crate::tasks::persist_pool_accounts(state)
            .await
            .map_err(|error| KiroError {
                status: None,
                endpoint: "credential-store".into(),
                message: format!("resolved profile ARN could not be persisted: {error}"),
            })?;
        return Ok(resolved);
    }

    Err(KiroError {
        status: None,
        endpoint: "MCP web_search".into(),
        message: "account credentials changed repeatedly during profile discovery".into(),
    })
}

pub(in crate::http) fn prepare_kiro_payload(
    payload: &mut crosslane_translate::KiroPayload,
    endpoint: &str,
    stage: &str,
) -> Result<(), KiroError> {
    let repairs = sanitize_kiro_tool_history(payload);
    if repairs.has_structured_tool_repair() {
        tracing::warn!(
            endpoint,
            stage,
            flattened_tool_uses = repairs.flattened_tool_uses,
            flattened_tool_results = repairs.flattened_tool_results,
            normalized_tool_uses = repairs.normalized_tool_uses,
            removed_invalid_tool_uses = repairs.removed_invalid_tool_uses,
            relocated_tool_results = repairs.relocated_tool_results,
            synthesized_tool_results = repairs.synthesized_tool_results,
            normalized_tool_results = repairs.normalized_tool_results,
            removed_historical_tool_definitions = repairs.removed_historical_tool_definitions,
            removed_duplicate_tool_definitions = repairs.removed_duplicate_tool_definitions,
            inserted_messages = repairs.inserted_messages,
            "repaired Kiro tool history before request accounting"
        );
    } else if repairs.inserted_messages > 0 {
        tracing::info!(
            endpoint,
            stage,
            inserted_messages = repairs.inserted_messages,
            "normalized Kiro conversation roles before request accounting"
        );
    }
    validate_kiro_tool_history(payload).map_err(|message| KiroError {
        status: Some(400),
        endpoint: endpoint.into(),
        message: format!("translated Kiro tool history is invalid after repair: {message}"),
    })
}

pub(in crate::http) async fn validate_internal_continuation(
    state: &Arc<AppState>,
    payload: &mut crosslane_translate::KiroPayload,
    compact: bool,
    endpoint: &str,
    stage: &str,
    enforce_tool_search_budget: bool,
) -> Result<u64, KiroError> {
    prepare_kiro_payload(payload, endpoint, stage)?;
    let config = state.config.current();
    let mut input_tokens = state
        .tokenizer
        .estimate_kiro_payload(payload)
        .await
        .map_err(|message| KiroError {
            status: None,
            endpoint: endpoint.into(),
            message,
        })? as u64;
    let model = payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id
        .clone();
    input_tokens =
        super::truncate_context_if_requested(state, payload, input_tokens, compact, &model)
            .await
            .map_err(|message| KiroError {
                status: None,
                endpoint: endpoint.into(),
                message,
            })?;
    let tool_tokens = state
        .tokenizer
        .estimate_kiro_tools(payload)
        .await
        .map_err(|message| KiroError {
            status: None,
            endpoint: endpoint.into(),
            message,
        })? as u64;
    let payload_bytes = serde_json::to_vec(payload)
        .map_err(|error| KiroError {
            status: None,
            endpoint: endpoint.into(),
            message: error.to_string(),
        })?
        .len();
    let loaded_tools = loaded_tool_count(payload);
    let max_loaded_tools = config
        .context
        .max_loaded_tools
        .min(crosslane_translate::validate::MAX_TOOLS);
    let budget_message = if loaded_tools > max_loaded_tools {
        Some(format!(
            "too many loaded tools after {stage}: {loaded_tools} > {max_loaded_tools}"
        ))
    } else if enforce_tool_search_budget
        && tool_tokens > u64::from(config.context.max_tool_input_tokens)
    {
        Some(format!(
            "loaded Tool Search working set is too large after {stage}: {tool_tokens} tokens > {}",
            config.context.max_tool_input_tokens
        ))
    } else if payload_bytes > config.context.max_upstream_payload_bytes {
        Some(format!(
            "translated payload is too large after {stage}: {payload_bytes} bytes > {}",
            config.context.max_upstream_payload_bytes
        ))
    } else {
        None
    };
    if let Some(message) = budget_message {
        return Err(KiroError {
            status: Some(400),
            endpoint: endpoint.into(),
            message,
        });
    }
    let model = &payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id;
    check_context_limit(state, input_tokens, compact, model).map_err(|limit| KiroError {
        status: Some(400),
        endpoint: endpoint.into(),
        message: format!(
            "prompt is too long after {stage}: {} tokens > {}",
            limit.input_tokens, limit.maximum
        ),
    })?;
    Ok(input_tokens)
}

pub(super) fn merge_round_usage(
    total: &mut crosslane_kiro::UsageInfo,
    addition: &crosslane_kiro::UsageInfo,
) {
    total.input_tokens = total.input_tokens.saturating_add(addition.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(addition.output_tokens);
    total.credits += addition.credits;
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(addition.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(addition.cache_write_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(addition.reasoning_tokens);
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn nonstream_claude(
    state: Arc<AppState>,
    mut lease: AccountLease,
    mut reservation: CreditReservation,
    upstream: KiroResponse,
    payload: crosslane_translate::KiroPayload,
    trace_id: String,
    request_id: String,
    path: String,
    user_agent: String,
    request: ClaudeRequest,
    thinking_enabled: bool,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    mut input_tokens: u64,
    compact: bool,
    started: Instant,
    prompt_cache: Option<PromptCacheProfile>,
    mut compaction_summary: Option<String>,
    compaction_replay: Option<(
        String,
        super::super::compaction_replay::CompactionReplaySource,
    )>,
    compaction_iteration: Option<CompactionIterationUsage>,
    mut auto_compaction_original_input_tokens: Option<u64>,
    context_edit_stats: ClaudeContextEditStats,
    tool_search: Option<Arc<ClaudeToolSearchCatalog>>,
    max_tool_search_operations: u32,
    tool_search_operations: u32,
    web_search_max_rounds: u32,
    web_search_client_limit: bool,
    resumed_tool_searches: Vec<crosslane_translate::ClaudeToolSearchTrace>,
    resumed_web_searches: Vec<ClaudeWebSearchTrace>,
    resumed_server_events: Vec<ClaudeServerEvent>,
    diagnostics: RequestDiagnostics,
    web_tool_names: std::collections::HashMap<String, String>,
) -> Result<Response, ApiError> {
    let (mut decoded, endpoint, current_round_output_tokens) = collect_nonstream_rounds(
        &state,
        &trace_id,
        &lease,
        &mut reservation,
        upstream,
        payload,
        compact,
        Some(request.max_tokens),
        &request.stop_sequences,
        thinking_enabled,
        tool_search.as_ref(),
        max_tool_search_operations,
        tool_search_operations,
        web_search_max_rounds,
        web_search_client_limit,
        resumed_tool_searches,
        resumed_web_searches,
        resumed_server_events,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::Claude))?;
    if let Some(stats) = &decoded.automatic_compaction {
        input_tokens = stats.compacted_tokens as u64;
        auto_compaction_original_input_tokens.get_or_insert(stats.original_tokens as u64);
        compaction_summary = stats.summary.clone();
        if let (Some((scope, source)), Some(summary)) =
            (compaction_replay, compaction_summary.as_ref())
        {
            state
                .compaction_replay
                .remember(scope, source, summary.clone());
        }
    }
    decoded.restore_tool_names(&web_tool_names);
    let current_round_output_tokens = if current_round_output_tokens == 0 {
        decoded.usage.output_tokens
    } else {
        current_round_output_tokens
    };
    let account_id = lease.account_id();
    let account_name = lease.account().await.display_name().to_owned();
    state
        .prompt_cache
        .apply(&account_id, prompt_cache.as_ref(), &mut decoded.usage);
    let credits = credits(&state, &kiro_model, &decoded);
    lease.settle_credits(credits).await;
    reservation
        .settle(usage_record(
            &mapped_model,
            &request.model,
            &kiro_model,
            &path,
            &decoded,
            credits,
        ))
        .await
        .map_err(|error| meter_error(error, ErrorFormat::Claude))?;
    let response = decoded
        .claude_json_with_context_management(
            &request_id,
            &request.model,
            request.max_tokens,
            current_round_output_tokens,
            compaction_summary.as_deref(),
            compaction_iteration,
            auto_compaction_original_input_tokens,
            input_tokens,
            &context_edit_stats,
            &state.web_search_replay,
        )
        .map_err(|error| {
            tracing::error!(
                trace_id,
                request_id,
                account_id,
                endpoint,
                %error,
                "failed to protect web-search replay data"
            );
            ApiError::response_assembly(
                "failed to assemble encrypted web-search response",
                ErrorFormat::Claude,
            )
        })?;
    let mut log = request_log(
        &trace_id,
        &request_id,
        &path,
        &mapped_model,
        &request.model,
        &kiro_model,
        &account_id,
        &account_name,
        &endpoint,
        &model_path,
        model_mapping_rule.as_deref(),
        attempts,
        started,
        diagnostics,
        &decoded,
        credits,
    );
    log.user_agent = user_agent;
    state.stats.record(log);
    tracing::info!(
        event = "proxy.response.completed",
        trace_id,
        request_id,
        protocol = "claude",
        provider_id = "kiro",
        account_id,
        account_name,
        endpoint,
        model_path = %model_path.join(" -> "),
        mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
        input_tokens = decoded.usage.input_tokens,
        output_tokens = decoded.usage.output_tokens,
        credits,
        duration_ms = started.elapsed().as_millis() as u64,
        "client non-stream response completed"
    );
    Ok(Json(response).into_response())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn nonstream_openai(
    state: Arc<AppState>,
    mut lease: AccountLease,
    mut reservation: CreditReservation,
    upstream: KiroResponse,
    payload: crosslane_translate::KiroPayload,
    trace_id: String,
    request_id: String,
    path: String,
    user_agent: String,
    request: OpenAiRequest,
    thinking_enabled: bool,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    _input_tokens: u64,
    max_tokens: Option<u32>,
    started: Instant,
    prompt_cache: Option<PromptCacheProfile>,
    diagnostics: RequestDiagnostics,
    openai_tools: std::collections::HashMap<String, OpenAiToolIdentity>,
    responses_options: Option<super::super::responses::ResponsesOptions>,
) -> Result<Response, ApiError> {
    let (mut decoded, endpoint, current_round_output_tokens) = collect_nonstream_rounds(
        &state,
        &trace_id,
        &lease,
        &mut reservation,
        upstream,
        payload,
        false,
        max_tokens,
        &request.stop_sequences(),
        thinking_enabled,
        None,
        0,
        0,
        0,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::OpenAi))?;
    let current_round_output_tokens = if current_round_output_tokens == 0 {
        decoded.usage.output_tokens
    } else {
        current_round_output_tokens
    };
    let account_id = lease.account_id();
    let account_name = lease.account().await.display_name().to_owned();
    state
        .prompt_cache
        .apply(&account_id, prompt_cache.as_ref(), &mut decoded.usage);
    let credits = credits(&state, &kiro_model, &decoded);
    lease.settle_credits(credits).await;
    reservation
        .settle(usage_record(
            &mapped_model,
            &request.model,
            &kiro_model,
            &path,
            &decoded,
            credits,
        ))
        .await
        .map_err(|error| meter_error(error, ErrorFormat::OpenAi))?;
    let mut log = request_log(
        &trace_id,
        &request_id,
        &path,
        &mapped_model,
        &request.model,
        &kiro_model,
        &account_id,
        &account_name,
        &endpoint,
        &model_path,
        model_mapping_rule.as_deref(),
        attempts,
        started,
        diagnostics,
        &decoded,
        credits,
    );
    log.user_agent = user_agent;
    state.stats.record(log);
    tracing::info!(
        event = "proxy.response.completed",
        trace_id,
        request_id,
        protocol = "openai",
        provider_id = "kiro",
        account_id,
        account_name,
        endpoint,
        model_path = %model_path.join(" -> "),
        mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
        input_tokens = decoded.usage.input_tokens,
        output_tokens = decoded.usage.output_tokens,
        credits,
        duration_ms = started.elapsed().as_millis() as u64,
        "client non-stream response completed"
    );
    let thinking_format = if responses_options.is_some() {
        crosslane_core::config::ThinkingOutputFormat::Openai
    } else {
        state.config.current().features.thinking_output_format
    };
    let mut chat = decoded.openai_json(
        &request_id,
        &request.model,
        now_secs(),
        max_tokens,
        current_round_output_tokens,
        thinking_format,
        &openai_tools,
    );
    if request.legacy_functions {
        super::chat::legacy_chunk(&mut chat)
            .map_err(|message| ApiError::response_assembly(message, ErrorFormat::OpenAi))?;
    }
    let response = match responses_options {
        Some(options) => super::super::responses::json_response(chat, options)
            .map_err(|message| ApiError::response_assembly(message, ErrorFormat::OpenAi))?,
        None => chat,
    };
    Ok(Json(response).into_response())
}

pub(super) fn openai_tool_identities(
    request: &OpenAiRequest,
) -> std::collections::HashMap<String, OpenAiToolIdentity> {
    let names =
        crosslane_translate::ToolNameRegistry::new(request.tools.iter().filter_map(|tool| {
            tool.body
                .get(&tool.r#type)
                .and_then(|definition| definition.get("name"))
                .and_then(Value::as_str)
        }));
    request
        .tools
        .iter()
        .filter_map(|tool| {
            let definition = tool.body.get(&tool.r#type)?;
            let name = definition.get("name")?.as_str()?;
            Some((
                names.kiro_name(name),
                OpenAiToolIdentity {
                    kind: tool.r#type.clone(),
                    name: name.into(),
                },
            ))
        })
        .collect()
}

pub(super) fn credits(state: &Arc<AppState>, model: &str, decoded: &DecodedResponse) -> f64 {
    if decoded.usage.credits > 0.0 {
        decoded.usage.credits
    } else {
        fallback_credits(
            state,
            model,
            decoded.usage.input_tokens,
            decoded.usage.output_tokens,
        )
    }
}

pub(super) fn usage_record(
    model: &str,
    original_model: &str,
    kiro_model: &str,
    path: &str,
    decoded: &DecodedResponse,
    credits: f64,
) -> UsageRecord {
    UsageRecord {
        timestamp: now_secs(),
        provider_id: "kiro".into(),
        model: model.into(),
        original_model: Some(original_model.into()),
        kiro_model: Some(kiro_model.into()),
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
        credits,
        cache_read_tokens: Some(decoded.usage.cache_read_tokens),
        cache_write_tokens: Some(decoded.usage.cache_write_tokens),
        reasoning_tokens: Some(decoded.usage.reasoning_tokens),
        token_usage_source: if decoded.usage.credits > 0.0 {
            "server"
        } else {
            "estimated"
        }
        .into(),
        provider_billing: None,
        path: path.into(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn request_log(
    trace_id: &str,
    request_id: &str,
    path: &str,
    model: &str,
    original_model: &str,
    kiro_model: &str,
    account_id: &str,
    account_name: &str,
    endpoint: &str,
    model_path: &[String],
    model_mapping_rule: Option<&str>,
    attempts: Vec<UpstreamAttemptLog>,
    started: Instant,
    mut diagnostics: RequestDiagnostics,
    decoded: &DecodedResponse,
    credits: f64,
) -> RequestLog {
    diagnostics.tool_search_rounds = decoded.tool_searches.len();
    diagnostics.tool_search_matches = decoded
        .tool_searches
        .iter()
        .map(|search| search.matched_count)
        .sum();
    diagnostics.search_requested_limit = decoded
        .tool_searches
        .iter()
        .map(|search| search.requested_limit)
        .max()
        .unwrap_or_default();
    diagnostics.search_returned_count = decoded
        .tool_searches
        .iter()
        .map(|search| search.references.len())
        .sum();
    diagnostics.search_budget_truncated = decoded
        .tool_searches
        .iter()
        .any(|search| search.budget_truncated);
    diagnostics.web_search_rounds = decoded.web_searches.len();
    diagnostics.web_search_results = decoded
        .web_searches
        .iter()
        .map(|search| search.results.len())
        .sum();
    diagnostics.client_status = 200;
    diagnostics.upstream_status = Some(200);
    diagnostics.credits_source = if decoded.usage.credits > 0.0 {
        "server"
    } else {
        "estimated"
    }
    .into();
    RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.into(),
        request_id: request_id.into(),
        path: path.into(),
        user_agent: String::new(),
        provider_id: "kiro".into(),
        model: model.into(),
        original_model: original_model.into(),
        kiro_model: kiro_model.into(),
        account_id: account_id.into(),
        account_name: account_name.into(),
        endpoint: endpoint.into(),
        model_path: model_path.to_vec(),
        model_mapping_rule: model_mapping_rule.map(str::to_owned),
        attempts,
        duration_ms: started.elapsed().as_millis() as u64,
        status: 200,
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
        credits,
        error: None,
        diagnostics,
    }
}
