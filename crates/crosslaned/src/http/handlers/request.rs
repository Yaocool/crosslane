use super::{
    apply_context_management_edits, authenticate, claude_loaded_tools,
    claude_pending_server_tool_uses, claude_to_kiro, claude_validation_error,
    claude_web_tool_names, compact_history_for_limits, compact_target_from_maximum,
    compact_trigger_tokens, context_maximum, enforce_claude_user_agent, enforce_codex_user_agent,
    enforce_context, enforce_payload_budget, estimate_context_management_input_tokens,
    estimated_credits, execute_upstream, initial_compaction_decision, is_claude_user_agent,
    loaded_tool_bytes, loaded_tool_count, loaded_tool_names, logged_user_agent,
    matches_type_family, nonstream_claude, nonstream_openai, normalize_compaction_boundary,
    openai_to_kiro, openai_tool_identities, prepare_kiro_payload, prepare_upstream,
    prepend_attempt_logs, prepend_execute_error_attempts, reapply_compaction, reserve_credits,
    resolved_compaction_decision, resume_tool_search_payload, resume_web_search_payload,
    run_compaction, serialized_payload_bytes, stream, tool_search_budget_with_compaction,
    upstream_error, upstream_overflow_compaction_decision, validate_claude,
    validate_claude_generation, validate_openai, ApiError, AppState, Arc, Bytes, ClaudeRequest,
    ClaudeServerEvent, ClaudeToolSearchCatalog, ClaudeWebSearchTrace, CompactionDecision,
    CompactionReason, CompactionRequest, Duration, Engine, ErrorFormat, ExecuteError, HeaderMap,
    Instant, IpAddr, OpenAiRequest, RequestDiagnostics, Response, ServiceHttpState, StatusCode,
    StreamContext, StreamExt, StreamProtocol, TranslationOptions, UpstreamExecution, Url, Uuid,
    Value,
};
use crosslane_core::provider::ProviderProtocol;
use crosslane_translate::model::{map_model_for_provider, ModelMappingContext, ModelRoute};
use crosslane_translate::{
    error::log_model, responses_to_openai, ResponsesRequest, WebSearchDomainFilter,
};
use futures::TryStreamExt;

use super::super::compaction_replay::CompactionReplaySource;
use super::super::responses::{
    is_responses_path, resume_responses_request, stream_response, ResponsesOptions,
    ResponsesSessionOwner,
};
use super::provider_gateway::{execute_provider, SelectedProviderRoute};

pub(super) async fn handle_claude(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
) -> Result<Response, ApiError> {
    let client_context = super::super::client_context::extract(&headers, &body);
    let context_trace_id = trace_id.clone();
    let result = handle_claude_with_context(
        service,
        trace_id,
        path,
        headers,
        body,
        connection_guard,
        admission_guard,
        &client_context,
    )
    .await;
    super::super::client_context::log(&context_trace_id, &client_context);
    result.map_err(|error| {
        if error.client_context.is_some() {
            error
        } else {
            error.with_client_context(&client_context)
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_claude_with_context(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
    client_context: &crosslane_core::client_context::ClientContext,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    let provider_hint = super::provider_gateway::request_provider_hint(
        &state.config.current(),
        &service.service,
        &body,
    );
    let authenticated_key = authenticate(
        &state,
        &service.allowed_api_key_ids,
        &headers,
        ErrorFormat::Claude,
    )
    .map_err(|error| error.with_provider_id_if_empty(&provider_hint))?;
    enforce_claude_user_agent(&service, &headers, authenticated_key.as_ref())
        .map_err(|error| error.with_provider_id_if_empty(&provider_hint))?;
    let plan = super::provider_balance::plan(
        &service,
        authenticated_key.as_ref(),
        &body,
        ProviderProtocol::ClaudeMessages,
        ErrorFormat::Claude,
    )
    .await?;
    super::provider_balance::execute(
        &state.provider_scheduler,
        &service.service.id,
        &trace_id,
        plan,
        ErrorFormat::Claude,
        |route| {
            let service = service.clone();
            let trace_id = trace_id.clone();
            let path = path.clone();
            let headers = headers.clone();
            let key = authenticated_key.clone();
            let connection = connection_guard.clone();
            let admission = admission_guard.clone();
            async move {
                handle_claude_route(
                    service,
                    trace_id,
                    path,
                    headers,
                    route,
                    key,
                    connection,
                    admission,
                    client_context,
                )
                .await
                .map(|response| (response, ()))
            }
        },
    )
    .await
    .map(|(response, ())| response)
}

fn requests_claude_web_search(body: &Value) -> bool {
    #[cfg(feature = "crs")]
    if body.pointer("/tool_choice/type").and_then(Value::as_str) == Some("none") {
        return false;
    }
    body["tools"].as_array().is_some_and(|tools| {
        tools.iter().any(|tool| {
            tool["type"]
                .as_str()
                .is_some_and(|kind| matches_type_family(kind, "web_search"))
        })
    })
}

/// CRS keeps explicit `none` declarations as historical source metadata. The
/// translator suppresses callable tools and continuations independently; feature
/// switches must not erase the intent that preserves completed tool pairs.
pub(super) fn apply_claude_tool_policy(
    request: &mut ClaudeRequest,
    features: &crosslane_core::config::FeaturesConfig,
) -> bool {
    #[cfg(feature = "crs")]
    if request.crs_tools_disabled() {
        return true;
    }
    if features.disable_tools {
        request.tools.clear();
        request.tool_choice = None;
    } else if !features.enable_web_tools {
        request.tools.retain(|tool| {
            !tool.r#type.as_deref().is_some_and(|kind| {
                matches_type_family(kind, "web_search") || matches_type_family(kind, "web_fetch")
            })
        });
    }
    false
}

#[allow(clippy::too_many_arguments)]
async fn handle_claude_route(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    mut provider_route: SelectedProviderRoute,
    authenticated_key: Option<crate::meter::AuthenticatedApiKey>,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
    client_context: &crosslane_core::client_context::ClientContext,
) -> Result<Response, ApiError> {
    let body: Value = serde_json::from_slice(&provider_route.body).unwrap_or_default();
    let search_request = requests_claude_web_search(&body);
    if search_request {
        let config = service.app.config.current();
        if config.features.disable_tools || !config.features.enable_web_tools {
            let mut error = super::hosted_web::unsupported_search("web search is disabled");
            error.format = ErrorFormat::Claude;
            return Err(error);
        }
        provider_route.enable_model_fallback = false;
        provider_route.web_search_operation = true;
    }
    let response = Box::pin(handle_claude_route_inner(
        service,
        trace_id,
        path,
        headers,
        provider_route,
        authenticated_key,
        connection_guard,
        admission_guard,
        client_context,
    ))
    .await;
    if search_request {
        response
            .map(super::hosted_web::claude_search_response)
            .map_err(|error| {
                let mut error = super::hosted_web::unsupported_search(error.message);
                error.format = ErrorFormat::Claude;
                error
            })
    } else {
        response
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_claude_route_inner(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    provider_route: SelectedProviderRoute,
    authenticated_key: Option<crate::meter::AuthenticatedApiKey>,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
    client_context: &crosslane_core::client_context::ClientContext,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    let started = Instant::now();
    if provider_route.provider_kind == "copilot" {
        let body: Value = serde_json::from_slice(&provider_route.body).unwrap_or_default();
        if requests_claude_web_search(&body) {
            // Claude Code declares the server-side web tool only in its actual
            // WebSearch subrequest, unlike the Responses tool catalog.
            let (route, key) = super::hosted_web::kiro_search_route(
                &service,
                &provider_route,
                authenticated_key.as_ref(),
                &body,
            )
            .await
            .map_err(|mut error| {
                error.format = ErrorFormat::Claude;
                error
            })?;
            return Box::pin(handle_claude_route(
                service,
                trace_id,
                path,
                headers,
                route,
                key,
                connection_guard,
                admission_guard,
                client_context,
            ))
            .await;
        }
    }
    let allow_model_fallback = !provider_route.automatic && !provider_route.web_search_operation;
    let allowed_models = authenticated_key
        .as_ref()
        .map_or_else(Vec::new, |key| key.allowed_models.clone());
    let key_id = authenticated_key.as_ref().map(|key| key.id.clone());
    if provider_route.provider_kind != "kiro" {
        return execute_provider(
            service,
            provider_route,
            ProviderProtocol::ClaudeMessages,
            path,
            headers,
            trace_id,
            authenticated_key,
            connection_guard,
            admission_guard,
            ErrorFormat::Claude,
            false,
            client_context.clone(),
        )
        .await;
    }
    let lock_kiro_mapping = provider_route.lock_kiro_mapping;
    let locked_original_model = provider_route.original_model.clone();
    let locked_upstream_model = provider_route.upstream_model.clone();
    let locked_mapping_name = provider_route.mapping_rule.clone();
    let account_ids = provider_route.account_ids.clone();
    let body = provider_route.body;
    // Only the Kiro path consults the pool, so resolve the service's account
    // scope after the non-Kiro providers have returned.
    let account_ids = match account_ids {
        Some(ids) => ids,
        None => service.account_ids().await,
    };
    tracing::debug!(
        event = "proxy.authentication.completed",
        trace_id = %trace_id,
        protocol = "claude",
        api_key_id = key_id.as_deref().unwrap_or("anonymous"),
        "client authentication completed"
    );
    let mut request: ClaudeRequest = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid JSON in request body",
            ErrorFormat::Claude,
        )
    })?;
    if lock_kiro_mapping {
        request.model.clone_from(&locked_original_model);
    }
    let replay_source = CompactionReplaySource::capture(&request.messages);
    let conversation_fingerprint = super::conversation_fingerprint(&request.messages);
    let conversation_id = super::client_conversation_id(
        &service.service.id,
        key_id.as_deref(),
        client_context,
        &headers,
        request.conversation_id.as_deref(),
        super::metadata_conversation_hint(request.metadata.as_ref())
            .or(conversation_fingerprint.as_deref()),
    );
    let replay_scope = super::client_replay_scope(
        &service.service.id,
        key_id.as_deref(),
        client_context,
        conversation_fingerprint.as_deref(),
    );
    let replay_applied = replay_scope
        .as_deref()
        .and_then(|scope| state.compaction_replay.apply(scope, &mut request.messages));
    let compact_trigger = compact_trigger_tokens(request.context_management.as_ref());
    // Claude discards everything before the latest compaction block. Apply
    // that semantic boundary before validating references or authenticated
    // replay records so ignored history cannot reject the effective request.
    let compaction_normalization = normalize_compaction_boundary(&mut request);
    let compact_boundary_applied = compaction_normalization.boundary_applied;
    validate_claude(&request).map_err(claude_validation_error)?;
    validate_claude_generation(&request).map_err(claude_validation_error)?;
    let claude_code = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_claude_user_agent);
    let context_input_estimate = estimate_context_management_input_tokens(&request);
    let context_edit_stats = apply_context_management_edits(&mut request, context_input_estimate);
    let attachment_guards = hydrate_claude_attachments(&state, &mut request).await?;
    // Hydration rewrites URL sources into base64 sources and can infer a
    // document title. Validate the effective wire input again so a remote
    // response can never bypass the same media and size rules as inline data.
    validate_claude(&request).map_err(claude_validation_error)?;
    if context_edit_stats.changed() {
        tracing::info!(
            trace_id = %trace_id,
            cleared_tool_results = context_edit_stats.cleared_tool_results,
            cleared_tool_inputs = context_edit_stats.cleared_tool_inputs,
            cleared_thinking_turns = context_edit_stats.cleared_thinking_turns,
            cleared_input_tokens = context_edit_stats.cleared_input_tokens(),
            "Claude context edits applied locally"
        );
    }
    crosslane_translate::validate_web_search_replay_content(&request, &state.web_search_replay)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error, ErrorFormat::Claude))?;
    tracing::info!(
        event = "proxy.request.validated",
        trace_id = %trace_id,
        protocol = "claude",
        model = %log_model(&request.model),
        streaming = request.stream,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        max_tokens = request.max_tokens,
        removed_noop_compaction_blocks = compaction_normalization.removed_noop_blocks,
        removed_noop_compaction_messages = compaction_normalization.removed_noop_messages,
        client_compact_edit = compact_trigger.is_some(),
        client_compaction_boundary_applied =
            compact_boundary_applied && replay_applied.is_none(),
        server_compaction_replayed = replay_applied.is_some(),
        claude_code_client = claude_code,
        "client request validated"
    );
    if let Some(replay) = replay_applied {
        tracing::info!(
            event = "proxy.compaction.checkpoint_replayed",
            trace_id = %trace_id,
            source_message_count = replay.source_message_count,
            incoming_message_count = replay.incoming_message_count,
            checkpoint_bytes = replay.checkpoint_bytes,
            "replayed a verified compaction checkpoint omitted by the client"
        );
    }
    let config = state.config.current();
    let original_tool_count = request.tools.len();
    let client_compaction =
        claude_code && crosslane_translate::prepare_claude_code_compaction(&mut request);
    if client_compaction {
        tracing::info!(
            event = "proxy.compaction.client_summary_prepared",
            trace_id = %trace_id,
            declared_tool_count = original_tool_count,
            "prepared a tool-free client summary with system and tool definitions as complete source history"
        );
    }
    let tools_disabled = apply_claude_tool_policy(&mut request, &config.features);
    if !tools_disabled
        && !config.features.enable_tool_search
        && request.tools.iter().any(|tool| {
            tool.defer_loading
                || tool
                    .r#type
                    .as_deref()
                    .is_some_and(crosslane_translate::is_tool_search_type)
        })
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Anthropic Tool Search is disabled by proxy configuration; remove defer_loading/Tool Search or enable features.enable_tool_search",
            ErrorFormat::Claude,
        ));
    }
    // Discover pending calls only from the already validated effective
    // history, so discarded calls cannot be executed again.
    let pending_server_tools = if client_compaction {
        Vec::new()
    } else {
        claude_pending_server_tool_uses(&request)
    };
    let web_search_tool = request.tools.iter().find(|tool| {
        !tools_disabled
            && tool
                .r#type
                .as_deref()
                .is_some_and(|kind| matches_type_family(kind, "web_search"))
    });
    let web_search_client_limit = web_search_tool.is_some_and(|tool| tool.max_uses.is_some());
    let web_search_domains = web_search_tool
        .map(|tool| {
            WebSearchDomainFilter::new(
                tool.allowed_domains.as_deref(),
                tool.blocked_domains.as_deref(),
            )
        })
        .transpose()
        .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, message, ErrorFormat::Claude))?
        .unwrap_or_default();
    let web_search_proxy_limit = config.features.web_search_max_rounds.max(1);
    if let Some(requested) = web_search_tool.and_then(|tool| tool.max_uses) {
        if requested > web_search_proxy_limit {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "web_search max_uses={requested} exceeds this proxy's configured execution limit of {web_search_proxy_limit}"
                ),
                ErrorFormat::Claude,
            ));
        }
    }
    let web_search_max_rounds = web_search_tool
        .map(|tool| tool.max_uses.unwrap_or(web_search_proxy_limit))
        .unwrap_or(0);
    let max_tool_search_operations = config.features.tool_search_max_operations.clamp(1, 256);
    let web_tool_names = claude_web_tool_names(&request);
    let route = if lock_kiro_mapping {
        ModelRoute {
            original: locked_original_model,
            mapped: locked_upstream_model,
            rule: locked_mapping_name,
            rule_index: None,
        }
    } else {
        map_model_for_provider(
            &request.model,
            &config.model_mapping,
            ModelMappingContext {
                provider_id: "kiro",
                service_id: Some(&service.service.id),
                api_key_id: key_id.as_deref(),
                remaining_percent: None,
            },
            "",
        )
    };
    let locked_mapping_rule = lock_kiro_mapping.then_some(route.rule.as_deref()).flatten();
    let mut options = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
    options.enhance_system_prompt = config.features.enhance_system_prompt && !client_compaction;
    options.web_search_replay = Some(state.web_search_replay.clone());
    options.enable_prompt_cache = config.features.enable_prompt_cache;
    options.separate_compaction_instruction = client_compaction;
    options.conversation_id = conversation_id;
    options.additional_model_request_fields_schema = state
        .resolved_model_info(&route.mapped)
        .and_then(|model| model.additional_model_request_fields_schema);
    let mut payload = claude_to_kiro(&request, &options);
    if client_compaction {
        crosslane_translate::prepare_compaction_source_history(&mut payload);
    }
    // Client-managed ToolSearch also uses deferred definitions, without a
    // proxy-managed server-tool catalog. Count them before catalog extraction.
    let deferred_tool_count = request
        .tools
        .iter()
        .filter(|tool| tool.defer_loading)
        .count();
    let catalog_bytes = request
        .tools
        .iter()
        .filter(|tool| tool.defer_loading)
        .filter_map(|tool| serde_json::to_vec(tool).ok())
        .fold(2usize, |total, tool| {
            total.saturating_add(tool.len()).saturating_add(1)
        });
    // Keep the deferred catalog proxy-side and resolve server calls that were
    // intentionally left pending by a prior mixed client/server turn.
    let (next_request, tool_search) = tokio::task::spawn_blocking(move || {
        let mut request = request;
        let catalog = ClaudeToolSearchCatalog::take_from_request(&mut request).map(Arc::new);
        (request, catalog)
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Tool Search catalog worker failed: {error}"),
            ErrorFormat::Claude,
        )
    })?;
    request = next_request;
    let mut early_compaction = None;
    let mut early_compaction_iteration = None;
    let initial_payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::Claude)?;
    if initial_payload_bytes > config.context.max_upstream_payload_bytes
        && config.context.auto_compact_on_overflow
        && !client_compaction
    {
        // Admission is a byte budget. Account-dependent model resolution below
        // remains responsible for deciding the actual token window.
        let maximum = context_maximum(&state, true, &route.mapped);
        let byte_decision = CompactionDecision {
            reasons: vec![CompactionReason::MappedWindowOverflow],
            model: route.mapped.clone(),
            trigger_tokens: context_maximum(&state, false, &route.mapped),
            target_tokens: compact_target_from_maximum(maximum),
            maximum_tokens: maximum,
        };
        let summary_model = if config.context.compaction_summary_model.trim().is_empty() {
            route.mapped.as_str()
        } else {
            config.context.compaction_summary_model.trim()
        };
        let run = run_compaction(
            &state,
            CompactionRequest {
                trace_id: &trace_id,
                client_context: Some(client_context),
                key_id: key_id.as_deref(),
                account_ids: Arc::clone(&account_ids),
                allowed_models: &allowed_models,
                service_id: Some(&service.service.id),
                source_payload: &payload,
                decision: &byte_decision,
                summary_model,
                summary_timeout_ms: config.context.compaction_summary_timeout_ms,
                preserve_recent_turns: config.context.compaction_preserve_recent_turns,
                explicit_client_compaction: false,
            },
        )
        .await?;
        payload = run.payload;
        early_compaction_iteration = run.iteration_usage;
        early_compaction = (run.stats.removed_messages > 0).then_some(run.stats);
        if let Some(stats) = &early_compaction {
            tracing::info!(
                trace_id = %trace_id,
                compaction_reason = "payload_budget_overflow",
                compaction_mode = "complete_checkpoint",
                original_payload_bytes = initial_payload_bytes,
                original_input_tokens = stats.original_tokens,
                compacted_input_tokens = stats.compacted_tokens,
                removed_messages = stats.removed_messages,
                "request history compacted before payload admission"
            );
        }
    }
    // Pending server calls may execute an external Web Search before the
    // first Kiro generation. Reject an oversized initial working set before
    // acquiring a search account or making that external request. The same
    // budgets are checked again after pending results and internal searches
    // have changed the payload.
    let initial_tool_tokens = (state
        .tokenizer
        .estimate_kiro_tools(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64)
        .max(
            state
                .tokenizer
                .estimate_claude_tools(&claude_loaded_tools(&request))
                .await
                .map_err(|error| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error,
                        ErrorFormat::Claude,
                    )
                })? as u64,
        );
    enforce_payload_budget(
        &state,
        initial_tool_tokens,
        serialized_payload_bytes(&payload, ErrorFormat::Claude)?,
        loaded_tool_count(&payload),
        tool_search.is_some(),
        ErrorFormat::Claude,
    )?;
    let mut resumed_tool_searches = Vec::new();
    let mut resumed_web_searches = Vec::new();
    let mut resumed_server_events = Vec::new();
    let mut pending_web_searches = Vec::new();
    let mut loaded_names = loaded_tool_names(&payload);
    let mut tool_search_operations = 0u32;
    for tool_use in pending_server_tools {
        if tool_use.name == "web_search" {
            if web_search_max_rounds == 0 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "pending web_search call has no matching web_search tool definition",
                    ErrorFormat::Claude,
                ));
            }
            pending_web_searches.push(tool_use);
            continue;
        }
        let Some(catalog) = tool_search
            .as_ref()
            .filter(|catalog| catalog.is_search_tool(&tool_use.name))
        else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "pending server tool '{}' has no matching supported tool definition",
                    tool_use.name
                ),
                ErrorFormat::Claude,
            ));
        };
        let mut outcome = if tool_search_operations >= max_tool_search_operations {
            catalog.unavailable_outcome(
                &tool_use,
                format!("Tool Search operation limit of {max_tool_search_operations} was reached"),
            )
        } else {
            let budget = tool_search_budget_with_compaction(&state, &payload, false)
                .await
                .map_err(|message| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        message,
                        ErrorFormat::Claude,
                    )
                })?;
            tool_search_operations += 1;
            match catalog
                .search_with_budget_excluding_async(tool_use.clone(), budget, loaded_names.clone())
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => catalog.unavailable_outcome(&tool_use, error),
            }
        };
        outcome.trace.id = tool_use.tool_use_id.clone();
        outcome.trace = outcome.trace.result_only();
        loaded_names.extend(
            outcome
                .tools
                .iter()
                .filter_map(|tool| tool.specification().map(|tool| tool.name.clone())),
        );
        resume_tool_search_payload(&mut payload, &tool_use, &outcome);
        resumed_server_events.push(ClaudeServerEvent::ToolSearch {
            index: resumed_tool_searches.len(),
            preceding_text: String::new(),
        });
        resumed_tool_searches.push(outcome.trace);
    }
    if !pending_web_searches.is_empty() {
        let needs_search_account = pending_web_searches
            .iter()
            .filter(|tool_use| {
                tool_use
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .is_some_and(|query| !query.trim().is_empty())
            })
            .take(web_search_max_rounds as usize)
            .next()
            .is_some();
        let search_lease = if needs_search_account {
            Some(
                state
                    .pool()
                    .acquire_scoped_excluding(
                        "",
                        0.0,
                        &account_ids,
                        &std::collections::HashSet::new(),
                    )
                    .await
                    .map_err(|error| {
                        upstream_error(ExecuteError::Pool(error), ErrorFormat::Claude)
                    })?,
            )
        } else {
            None
        };
        let mut resumed_web_search_uses = 0u32;
        for tool_use in pending_web_searches {
            let query = tool_use
                .input
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let trace = if resumed_web_search_uses >= web_search_max_rounds {
                ClaudeWebSearchTrace::error(
                    tool_use.tool_use_id.clone(),
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
                    tool_use.tool_use_id.clone(),
                    &query,
                    "invalid_tool_input",
                    "web search query must not be empty".into(),
                )
            } else {
                resumed_web_search_uses += 1;
                match search_lease.as_ref() {
                    Some(lease) => match super::execution::execute_filtered_kiro_web_search(
                        &state,
                        lease,
                        &query,
                        &web_search_domains,
                    )
                    .await
                    {
                        Ok(results) => ClaudeWebSearchTrace::success(
                            tool_use.tool_use_id.clone(),
                            &query,
                            results,
                        ),
                        Err(error) => {
                            let mut error = super::hosted_web::unsupported_search(error.message);
                            error.format = ErrorFormat::Claude;
                            return Err(error);
                        }
                    },
                    None => ClaudeWebSearchTrace::error(
                        tool_use.tool_use_id.clone(),
                        &query,
                        "unavailable",
                        "web search account is unavailable".into(),
                    ),
                }
            }
            .result_only();
            resume_web_search_payload(&mut payload, &tool_use, &trace);
            resumed_server_events.push(ClaudeServerEvent::WebSearch {
                index: resumed_web_searches.len(),
                preceding_text: String::new(),
            });
            resumed_web_searches.push(trace);
        }
    }
    if (!resumed_tool_searches.is_empty() || !resumed_web_searches.is_empty())
        && config.context.auto_compact_on_overflow
        && !client_compaction
        && serialized_payload_bytes(&payload, ErrorFormat::Claude)?
            > config.context.max_upstream_payload_bytes
    {
        if let Some(mut stats) = compact_history_for_limits(
            &state,
            &mut payload,
            u64::MAX,
            config.context.max_upstream_payload_bytes,
        )
        .await
        .map_err(|message| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                message,
                ErrorFormat::Claude,
            )
        })? {
            tracing::info!(
                trace_id = %trace_id,
                compaction_reason = "pending_server_tools_payload_overflow",
                compaction_mode = "complete_checkpoint",
                original_input_tokens = stats.original_tokens,
                compacted_input_tokens = stats.compacted_tokens,
                removed_messages = stats.removed_messages,
                "pending server tool history compacted before dispatch"
            );
            if let Some(previous) = &early_compaction {
                stats.original_tokens = previous.original_tokens;
            }
            early_compaction = Some(stats);
        }
    }
    prepare_kiro_payload(
        &mut payload,
        "request-preparation",
        "initial Claude request",
    )
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error.message,
            ErrorFormat::Claude,
        )
    })?;
    // Account-dependent rules, weighted choices, aliases, and default-model
    // resolution must finish before token-window compaction or rejection.
    let selected = prepare_upstream(
        &state,
        &account_ids,
        &trace_id,
        &route.mapped,
        &request.model,
        key_id.as_deref(),
        &allowed_models,
        Some(&service.service.id),
        if allow_model_fallback {
            config.features.default_model_id.as_str()
        } else {
            ""
        },
        locked_mapping_rule,
        &payload,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::Claude))?;
    let resolved_model = selected.kiro_model.clone();
    payload = selected.payload.clone();
    let mut prepared = Some(selected);
    let decision = state.prepare_model_request(&mut payload);
    tracing::debug!(
        event = "proxy.adaptive_thinking.decided",
        trace_id = %trace_id,
        model = %resolved_model,
        enabled = decision.enabled,
        reason = ?decision.reason,
        effort = decision.effort.as_deref(),
        "adaptive thinking decision"
    );
    let mut input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let mut compacted = compact_boundary_applied || early_compaction.is_some();
    let mut compaction_summary = early_compaction
        .as_ref()
        .and_then(|stats| stats.summary.clone());
    let initial_input_tokens = early_compaction
        .as_ref()
        .map_or(input_tokens, |stats| stats.original_tokens as u64);
    let mut compaction_artifact = None;
    let mut auto_compaction_original_input_tokens = early_compaction
        .as_ref()
        .map(|stats| stats.original_tokens as u64);
    let mut compaction_iteration = early_compaction_iteration;
    if let Some(decision) = initial_compaction_decision(
        &state,
        &resolved_model,
        input_tokens,
        compact_trigger,
        config.context.auto_compact_on_overflow || client_compaction,
    ) {
        // The summary may use this same single-concurrency account. Release
        // its slot first; dispatch rechecks any new target after compaction.
        drop(prepared.take());
        let summary_model = if config.context.compaction_summary_model.trim().is_empty() {
            resolved_model.as_str()
        } else {
            config.context.compaction_summary_model.trim()
        };
        let run = run_compaction(
            &state,
            CompactionRequest {
                trace_id: &trace_id,
                client_context: Some(client_context),
                key_id: key_id.as_deref(),
                account_ids: Arc::clone(&account_ids),
                allowed_models: &allowed_models,
                service_id: Some(&service.service.id),
                source_payload: &payload,
                decision: &decision,
                summary_model,
                summary_timeout_ms: config.context.compaction_summary_timeout_ms,
                preserve_recent_turns: config.context.compaction_preserve_recent_turns,
                explicit_client_compaction: client_compaction,
            },
        )
        .await?;
        if decision
            .reasons
            .contains(&CompactionReason::MappedWindowOverflow)
            && run.stats.removed_messages > 0
        {
            auto_compaction_original_input_tokens = Some(initial_input_tokens);
        }
        payload = run.payload;
        input_tokens = run.stats.compacted_tokens as u64;
        compacted |= run.stats.removed_messages > 0;
        compaction_summary = run.stats.summary.clone();
        compaction_iteration = run.iteration_usage;
        compaction_artifact = run.artifact;
        tracing::info!(
            trace_id = %trace_id,
            compaction_reason = %decision.reason_names(),
            compaction_model = %decision.model,
            trigger_tokens = decision.trigger_tokens,
            target_tokens = decision.target_tokens,
            maximum_tokens = decision.maximum_tokens,
            original_input_tokens = run.stats.original_tokens,
            compacted_input_tokens = run.stats.compacted_tokens,
            removed_messages = run.stats.removed_messages,
            compaction_mode = run.mode,
            compaction_summary_model = run.summary_model.as_deref().unwrap_or("none"),
            summary_input_tokens = run.summary_input_tokens.unwrap_or(0),
            summary_output_tokens = run.iteration_usage.map_or(0, |usage| usage.output_tokens),
            fallback_reason = run.fallback_reason.unwrap_or("none"),
            "request context compacted"
        );
    }
    let full_tool_tokens = state
        .tokenizer
        .estimate_claude_tools(&claude_loaded_tools(&request))
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let tool_tokens = (state
        .tokenizer
        .estimate_kiro_tools(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64)
        .max(full_tool_tokens);
    let payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::Claude)?;
    let mut diagnostics = RequestDiagnostics {
        client_context: Some(client_context.clone()),
        original_tool_count,
        loaded_tool_count: loaded_tool_count(&payload),
        deferred_tool_count,
        loaded_tool_bytes: loaded_tool_bytes(&payload),
        catalog_bytes,
        tool_tokens,
        payload_bytes,
        ..RequestDiagnostics::default()
    };
    enforce_payload_budget(
        &state,
        tool_tokens,
        payload_bytes,
        loaded_tool_count(&payload),
        tool_search.is_some(),
        ErrorFormat::Claude,
    )?;
    enforce_context(
        &state,
        input_tokens,
        compacted,
        &resolved_model,
        ErrorFormat::Claude,
        &payload,
    )
    .await?;
    let mut estimate = estimated_credits(input_tokens, request.max_tokens, &config.pool);
    let reservation = reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::Claude)?;
    tracing::info!(
        event = "proxy.request.prepared",
        trace_id = %trace_id,
        protocol = "claude",
        requested_model = %log_model(&request.model),
        mapped_model = %route.mapped,
        input_tokens,
        original_tool_count = diagnostics.original_tool_count,
        tool_tokens,
        payload_bytes,
        loaded_tool_count = loaded_tool_count(&payload),
        loaded_tool_bytes = diagnostics.loaded_tool_bytes,
        deferred_tool_count = diagnostics.deferred_tool_count,
        catalog_bytes = diagnostics.catalog_bytes,
        estimated_credits = estimate,
        "upstream request prepared"
    );
    drop(body);
    let request_id = format!("msg_{}", Uuid::new_v4().simple());
    let first_execution = execute_upstream(
        &state,
        &account_ids,
        &trace_id,
        &route.mapped,
        &request.model,
        key_id.as_deref(),
        &allowed_models,
        Some(&service.service.id),
        if allow_model_fallback {
            config.features.default_model_id.as_str()
        } else {
            ""
        },
        locked_mapping_rule,
        estimate,
        input_tokens,
        compacted,
        &payload,
        prepared,
        allow_model_fallback,
    )
    .await;
    let (execution, reservation) = match first_execution {
        Ok(execution) => (execution, reservation),
        Err(error) if config.context.auto_compact_on_overflow || client_compaction => {
            let retry_plan = match &error {
                ExecuteError::ContextLimit(limit) => {
                    Some((resolved_compaction_decision(limit), Vec::new(), false))
                }
                ExecuteError::Dispatch(failure) if failure.error.is_context_too_long() => {
                    let model = if failure.context.kiro_model.is_empty() {
                        route.mapped.as_str()
                    } else {
                        failure.context.kiro_model.as_str()
                    };
                    upstream_overflow_compaction_decision(&state, model, input_tokens)
                        .map(|decision| (decision, failure.context.attempts.clone(), true))
                }
                ExecuteError::Upstream(upstream) if upstream.is_context_too_long() => {
                    upstream_overflow_compaction_decision(&state, &route.mapped, input_tokens)
                        .map(|decision| (decision, Vec::new(), true))
                }
                _ => None,
            };
            let Some((decision, previous_attempts, upstream_rejected)) = retry_plan else {
                return Err(upstream_error(error, ErrorFormat::Claude));
            };
            drop(reservation);
            let summary_model = if config.context.compaction_summary_model.trim().is_empty() {
                route.mapped.as_str()
            } else {
                config.context.compaction_summary_model.trim()
            };
            let run = if let Some(artifact) = compaction_artifact.as_ref() {
                reapply_compaction(
                    &state,
                    artifact,
                    &decision,
                    Some(&CompactionRequest {
                        trace_id: &trace_id,
                        client_context: Some(client_context),
                        key_id: key_id.as_deref(),
                        account_ids: Arc::clone(&account_ids),
                        allowed_models: &allowed_models,
                        service_id: Some(&service.service.id),
                        source_payload: &payload,
                        decision: &decision,
                        summary_model,
                        summary_timeout_ms: config.context.compaction_summary_timeout_ms,
                        preserve_recent_turns: config.context.compaction_preserve_recent_turns,
                        explicit_client_compaction: client_compaction,
                    }),
                )
                .await?
            } else {
                run_compaction(
                    &state,
                    CompactionRequest {
                        trace_id: &trace_id,
                        client_context: Some(client_context),
                        key_id: key_id.as_deref(),
                        account_ids: Arc::clone(&account_ids),
                        allowed_models: &allowed_models,
                        service_id: Some(&service.service.id),
                        source_payload: &payload,
                        decision: &decision,
                        summary_model,
                        summary_timeout_ms: config.context.compaction_summary_timeout_ms,
                        preserve_recent_turns: config.context.compaction_preserve_recent_turns,
                        explicit_client_compaction: client_compaction,
                    },
                )
                .await?
            };
            if run.stats.removed_messages > 0 {
                auto_compaction_original_input_tokens.get_or_insert(initial_input_tokens);
            }
            payload = run.payload;
            input_tokens = run.stats.compacted_tokens as u64;
            compacted |= run.stats.removed_messages > 0;
            compaction_summary = run.stats.summary.clone();
            compaction_iteration = run.iteration_usage;
            let replanned_tool_tokens = (state
                .tokenizer
                .estimate_kiro_tools(&payload)
                .await
                .map_err(|error| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error,
                        ErrorFormat::Claude,
                    )
                })? as u64)
                .max(full_tool_tokens);
            let replanned_payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::Claude)?;
            enforce_payload_budget(
                &state,
                replanned_tool_tokens,
                replanned_payload_bytes,
                loaded_tool_count(&payload),
                tool_search.is_some(),
                ErrorFormat::Claude,
            )?;
            enforce_context(
                &state,
                input_tokens,
                true,
                &decision.model,
                ErrorFormat::Claude,
                &payload,
            )
            .await?;
            diagnostics.tool_tokens = replanned_tool_tokens;
            diagnostics.payload_bytes = replanned_payload_bytes;
            diagnostics.loaded_tool_count = loaded_tool_count(&payload);
            diagnostics.loaded_tool_bytes = loaded_tool_bytes(&payload);
            estimate = estimated_credits(input_tokens, request.max_tokens, &config.pool);
            let reservation =
                reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::Claude)?;
            tracing::info!(
                trace_id = %trace_id,
                compaction_reason = %decision.reason_names(),
                resolved_model = %decision.model,
                resolved_context_maximum = decision.maximum_tokens,
                target_tokens = decision.target_tokens,
                original_input_tokens = run.stats.original_tokens,
                compacted_input_tokens = run.stats.compacted_tokens,
                removed_messages = run.stats.removed_messages,
                compaction_mode = run.mode,
                compaction_summary_model = run.summary_model.as_deref().unwrap_or("reused"),
                summary_input_tokens = run.summary_input_tokens.unwrap_or(0),
                summary_output_tokens = run.iteration_usage.map_or(0, |usage| usage.output_tokens),
                fallback_reason = run.fallback_reason.unwrap_or("none"),
                resolved_replanned = !upstream_rejected,
                upstream_overflow_retry = upstream_rejected,
                "request context replanned for automatic overflow retry"
            );
            let retry_execution = execute_upstream(
                &state,
                &account_ids,
                &trace_id,
                &route.mapped,
                &request.model,
                key_id.as_deref(),
                &allowed_models,
                Some(&service.service.id),
                if allow_model_fallback {
                    config.features.default_model_id.as_str()
                } else {
                    ""
                },
                locked_mapping_rule,
                estimate,
                input_tokens,
                true,
                &payload,
                None,
                allow_model_fallback,
            )
            .await;
            let execution = match retry_execution {
                Ok(mut execution) => {
                    prepend_attempt_logs(&mut execution.attempts, previous_attempts);
                    execution
                }
                Err(mut retry_error) => {
                    prepend_execute_error_attempts(&mut retry_error, previous_attempts);
                    let error = upstream_error(retry_error, ErrorFormat::Claude);
                    #[cfg(feature = "crs")]
                    let error = ApiError {
                        compaction_retried: true,
                        ..error
                    };
                    return Err(error);
                }
            };
            (execution, reservation)
        }
        Err(error) => return Err(upstream_error(error, ErrorFormat::Claude)),
    };
    if !client_compaction {
        if let (Some(scope), Some(source), Some(summary)) = (
            replay_scope.as_ref(),
            replay_source,
            compaction_summary.as_ref(),
        ) {
            if state
                .compaction_replay
                .remember(scope.clone(), source, summary.clone())
            {
                tracing::debug!(
                    event = "proxy.compaction.checkpoint_cached",
                    trace_id = %trace_id,
                    source_message_count = source.message_count(),
                    checkpoint_bytes = summary.len(),
                    "cached a bounded compaction checkpoint for verified replay"
                );
            }
        }
    }
    let prompt_cache = if config.features.enable_prompt_cache {
        if compaction_summary.is_some() {
            state
                .prompt_cache
                .claude_compacted_profile(&state.tokenizer, &request, input_tokens)
                .await
        } else {
            state.prompt_cache.claude_profile(&request, input_tokens)
        }
    } else {
        None
    };
    let UpstreamExecution {
        lease,
        response: upstream,
        upstream_access_token,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execution;
    let compaction_replay = if client_compaction {
        None
    } else {
        replay_scope.zip(replay_source)
    };
    if request.stream {
        return Ok(stream::response(
            upstream,
            StreamProtocol::Claude,
            StreamContext {
                state,
                account_ids: Arc::clone(&account_ids),
                lease,
                upstream_access_token,
                reservation,
                trace_id,
                request_id,
                user_agent: logged_user_agent(&headers),
                path,
                service_id: service.service.id.clone(),
                model: request.model.clone(),
                mapped_model,
                original_model: request.model,
                api_key_id: key_id.clone(),
                allowed_models,
                lock_model_mapping: lock_kiro_mapping,
                allow_model_fallback,
                kiro_model,
                model_path,
                model_mapping_rule,
                attempts,
                input_tokens,
                compact: compacted,
                compaction_summary,
                compaction_replay,
                compaction_in_start: headers
                    .get("user-agent")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(is_claude_user_agent),
                compaction_iteration,
                auto_compaction_original_input_tokens,
                context_edit_stats,
                estimated_credits: estimate,
                max_tokens: Some(request.max_tokens),
                stop_sequences: request.stop_sequences.clone(),
                started,
                prompt_cache,
                payload,
                auto_continue_rounds: config.features.auto_continue_rounds.min(30),
                buffer_tool_calls: config.features.buffer_tool_calls,
                enable_tool_leak_filter: config.features.enable_tool_leak_filter,
                thinking_enabled: decision.enabled,
                thinking_output_format: config.features.thinking_output_format,
                include_usage_chunk: false,
                web_tool_names,
                tool_search,
                max_tool_search_operations,
                tool_search_operations,
                web_search_max_rounds,
                web_search_client_limit,
                web_search_domains,
                resumed_tool_searches,
                resumed_web_searches,
                resumed_server_events,
                diagnostics,
                openai_tools: std::collections::HashMap::new(),
                _attachment_guards: attachment_guards,
                _connection_guard: connection_guard,
                _admission_guard: admission_guard,
            },
        ));
    }
    nonstream_claude(
        state,
        lease,
        reservation,
        upstream,
        payload,
        trace_id,
        request_id,
        path,
        logged_user_agent(&headers),
        request,
        decision.enabled,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        input_tokens,
        compacted,
        started,
        prompt_cache,
        compaction_summary,
        compaction_replay,
        compaction_iteration,
        auto_compaction_original_input_tokens,
        context_edit_stats,
        tool_search,
        max_tool_search_operations,
        tool_search_operations,
        web_search_max_rounds,
        web_search_client_limit,
        &web_search_domains,
        resumed_tool_searches,
        resumed_web_searches,
        resumed_server_events,
        diagnostics,
        web_tool_names,
    )
    .await
}

pub(super) async fn handle_openai(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
) -> Result<Response, ApiError> {
    let mut client_context = super::super::client_context::extract(&headers, &body);
    let context_trace_id = trace_id.clone();
    let result = handle_openai_with_context(
        service,
        trace_id,
        path,
        headers,
        body,
        connection_guard,
        admission_guard,
        &mut client_context,
    )
    .await;
    super::super::client_context::log(&context_trace_id, &client_context);
    result.map_err(|error| {
        if error.client_context.is_some() {
            error
        } else {
            error.with_client_context(&client_context)
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_openai_with_context(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
    client_context: &mut crosslane_core::client_context::ClientContext,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    // Multi-candidate chat re-enters this handler for every candidate. Keep
    // the client body so provider selection and a cross-provider mapping are
    // evaluated from the same request rather than from the first routed body.
    let provider_hint = super::provider_gateway::request_provider_hint(
        &state.config.current(),
        &service.service,
        &body,
    );
    let authenticated_key = authenticate(
        &state,
        &service.allowed_api_key_ids,
        &headers,
        ErrorFormat::OpenAi,
    )
    .map_err(|error| error.with_provider_id_if_empty(&provider_hint))?;
    enforce_codex_user_agent(&service, &headers, authenticated_key.as_ref())
        .map_err(|error| error.with_provider_id_if_empty(&provider_hint))?;
    let protocol = if is_responses_path(&path) {
        ProviderProtocol::OpenAiResponses
    } else {
        ProviderProtocol::OpenAiChat
    };
    let plan = super::provider_balance::plan(
        &service,
        authenticated_key.as_ref(),
        &body,
        protocol,
        ErrorFormat::OpenAi,
    )
    .await?;
    let automatic = plan.is_automatic();
    let mut connection_guard = Some(connection_guard);
    let mut admission_guard = Some(admission_guard);
    let result = super::provider_balance::execute(
        &state.provider_scheduler,
        &service.service.id,
        &trace_id,
        plan,
        ErrorFormat::OpenAi,
        |route| {
            let service = service.clone();
            let trace_id = trace_id.clone();
            let path = path.clone();
            let headers = headers.clone();
            let body = body.clone();
            let key = authenticated_key.clone();
            let connection = if automatic {
                connection_guard.as_ref().expect("request guard").clone()
            } else {
                connection_guard.take().expect("single pinned attempt")
            };
            let admission = if automatic {
                admission_guard.as_ref().expect("request guard").clone()
            } else {
                admission_guard.take().expect("single pinned attempt")
            };
            let mut context = client_context.clone();
            async move {
                let result = handle_openai_route(
                    service,
                    trace_id,
                    path,
                    headers,
                    body,
                    route,
                    key,
                    connection,
                    admission,
                    &mut context,
                )
                .await;
                result
                    .map(|response| (response, context.clone()))
                    .map_err(|error| error.with_client_context(&context))
            }
        },
    )
    .await;
    result.map(|(response, context)| {
        *client_context = context;
        response
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_openai_route(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    original_body: Bytes,
    provider_route: SelectedProviderRoute,
    authenticated_key: Option<crate::meter::AuthenticatedApiKey>,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
    client_context: &mut crosslane_core::client_context::ClientContext,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    let started = Instant::now();
    if is_responses_path(&path)
        && super::hosted_web::has_web_search(
            &service,
            authenticated_key.as_ref(),
            &provider_route.body,
        )
    {
        return super::hosted_web::handle(
            service,
            trace_id,
            path,
            headers,
            provider_route,
            authenticated_key,
            connection_guard,
            admission_guard,
            client_context.clone(),
        )
        .await;
    }
    let allow_model_fallback = !provider_route.automatic && !provider_route.web_search_operation;
    let allowed_models = authenticated_key
        .as_ref()
        .map_or_else(Vec::new, |key| key.allowed_models.clone());
    let key_id = authenticated_key.as_ref().map(|key| key.id.clone());
    if provider_route.provider_kind != "kiro" {
        if is_responses_path(&path) {
            if let Some(previous_id) =
                super::super::client_context::previous_response_id(&original_body)
            {
                let owner = ResponsesSessionOwner::new(service.service.id.clone(), key_id.clone());
                if let Some(previous) = state.response_client_contexts.get(
                    &owner,
                    provider_route.provider_id.as_str(),
                    &previous_id,
                ) {
                    client_context.inherit(&previous).map_err(|message| {
                        ApiError::new(StatusCode::BAD_REQUEST, message, ErrorFormat::OpenAi)
                    })?;
                }
            }
        }
        let protocol = if is_responses_path(&path) {
            ProviderProtocol::OpenAiResponses
        } else {
            ProviderProtocol::OpenAiChat
        };
        return execute_provider(
            service,
            provider_route,
            protocol,
            path,
            headers,
            trace_id,
            authenticated_key,
            connection_guard,
            admission_guard,
            ErrorFormat::OpenAi,
            false,
            client_context.clone(),
        )
        .await;
    }
    let lock_kiro_mapping = provider_route.lock_kiro_mapping;
    let locked_original_model = provider_route.original_model.clone();
    let locked_upstream_model = provider_route.upstream_model.clone();
    let locked_mapping_name = provider_route.mapping_rule.clone();
    let account_ids = provider_route.account_ids.clone();
    let body = provider_route.body;
    // Only the Kiro path consults the pool, so resolve the service's account
    // scope after the non-Kiro providers have returned.
    let account_ids = match account_ids {
        Some(ids) => ids,
        None => service.account_ids().await,
    };
    tracing::debug!(
        event = "proxy.authentication.completed",
        trace_id = %trace_id,
        protocol = "openai",
        api_key_id = key_id.as_deref().unwrap_or("anonymous"),
        "client authentication completed"
    );
    let (mut request, mut responses_options) = if is_responses_path(&path) {
        let mut responses: ResponsesRequest = serde_json::from_slice(&body).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid Responses request: {error}"),
                ErrorFormat::OpenAi,
            )
        })?;
        let session_owner = ResponsesSessionOwner::new(service.service.id.clone(), key_id.clone());
        let mut inherited_conversation_id = None;
        if let Some(previous_response_id) = responses.previous_response_id.clone() {
            let session = state
                .responses_sessions
                .get(&previous_response_id, &session_owner)
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "unknown or expired previous_response_id",
                        ErrorFormat::OpenAi,
                    )
                })?;
            let forks = client_context.forks(session.client_context());
            client_context
                .inherit(session.client_context())
                .map_err(|message| {
                    ApiError::new(StatusCode::BAD_REQUEST, message, ErrorFormat::OpenAi)
                })?;
            inherited_conversation_id = (!forks)
                .then(|| {
                    session
                        .conversation_id()
                        .ok_or_else(|| {
                            ApiError::response_assembly(
                                "stored response is missing its upstream conversation state",
                                ErrorFormat::OpenAi,
                            )
                        })
                        .map(str::to_owned)
                })
                .transpose()?;
            resume_responses_request(&mut responses, session);
        }
        let translated = responses_to_openai(&responses).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                ErrorFormat::OpenAi,
            )
        })?;
        let mut options = ResponsesOptions::new(
            &responses,
            translated.tools,
            translated.tool_names,
            Some((state.responses_sessions.clone(), session_owner)),
            inherited_conversation_id,
        );
        options.set_client_context(client_context);
        (translated.request, Some(options))
    } else {
        let value: Value = serde_json::from_slice(&body).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Invalid JSON in request body",
                ErrorFormat::OpenAi,
            )
        })?;
        let request = crosslane_translate::parse_openai_request(value).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                ErrorFormat::OpenAi,
            )
        })?;
        validate_openai(&request).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                ErrorFormat::OpenAi,
            )
        })?;
        (request, None)
    };
    if lock_kiro_mapping {
        request.model.clone_from(&locked_original_model);
    }
    if responses_options.is_none() && request.n.unwrap_or(1) > 1 {
        let include_usage = request
            .stream_options
            .as_ref()
            .and_then(|value| value["include_usage"].as_bool())
            .unwrap_or(false);
        // Each sequential candidate owns the ordinary admission/quota guards.
        // Do not retain an additional parent slot (a configured limit of one
        // must still permit n > 1).
        drop(connection_guard);
        drop(admission_guard);
        let count = request.n.unwrap();
        let streaming = request.stream;
        drop(request);
        return super::chat::multiple(
            service,
            path,
            headers,
            original_body,
            count,
            streaming,
            include_usage,
        )
        .await;
    }
    tracing::info!(
        event = "proxy.request.validated",
        trace_id = %trace_id,
        protocol = "openai",
        model = %log_model(&request.model),
        streaming = request.stream,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        "client request validated"
    );
    let config = state.config.current();
    if config.features.disable_tools {
        request.tools.clear();
        request.tool_choice = None;
    } else if !config.features.enable_web_tools {
        request.tools.retain(|tool| tool.r#type != "web_search");
    }
    let openai_tools = openai_tool_identities(&request);
    let image_guards = hydrate_openai_attachments(&state, &mut request).await?;
    validate_openai(&request).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            error.to_string(),
            ErrorFormat::OpenAi,
        )
    })?;
    // Omission must survive through response formatting and continuations,
    // not just the initial Kiro payload. Only credit estimates use a default.
    let max_tokens = request.output_token_limit();
    let route = if lock_kiro_mapping {
        ModelRoute {
            original: locked_original_model,
            mapped: locked_upstream_model,
            rule: locked_mapping_name,
            rule_index: None,
        }
    } else {
        map_model_for_provider(
            &request.model,
            &config.model_mapping,
            ModelMappingContext {
                provider_id: "kiro",
                service_id: Some(&service.service.id),
                api_key_id: key_id.as_deref(),
                remaining_percent: None,
            },
            "",
        )
    };
    let locked_mapping_rule = lock_kiro_mapping.then_some(route.rule.as_deref()).flatten();
    let mut options = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
    options.enhance_system_prompt = config.features.enhance_system_prompt;
    options.enable_prompt_cache = config.features.enable_prompt_cache;
    let inherited_responses_conversation_id = responses_options
        .as_ref()
        .and_then(|options| options.inherited_conversation_id())
        .map(str::to_owned);
    let conversation_fingerprint = super::conversation_fingerprint(&request.messages);
    options.conversation_id = inherited_responses_conversation_id.or_else(|| {
        super::client_conversation_id(
            &service.service.id,
            key_id.as_deref(),
            client_context,
            &headers,
            request.conversation_id.as_deref(),
            super::metadata_conversation_hint(request.metadata.as_ref())
                .or_else(|| {
                    responses_options
                        .as_ref()
                        .and_then(|options| options.conversation_hint())
                })
                .or(conversation_fingerprint.as_deref()),
        )
    });
    if let Some(responses_options) = responses_options.as_mut() {
        responses_options.set_conversation_id(options.conversation_id.clone());
    }
    options.additional_model_request_fields_schema = state
        .resolved_model_info(&route.mapped)
        .and_then(|model| model.additional_model_request_fields_schema);
    let mut payload = openai_to_kiro(&request, &options);
    let decision = state.prepare_model_request(&mut payload);
    tracing::debug!(
        event = "proxy.adaptive_thinking.decided",
        trace_id = %trace_id,
        enabled = decision.enabled,
        reason = ?decision.reason,
        effort = decision.effort.as_deref(),
        "adaptive thinking decision"
    );
    prepare_kiro_payload(
        &mut payload,
        "request-preparation",
        "initial OpenAI request",
    )
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error.message,
            ErrorFormat::OpenAi,
        )
    })?;
    let mut input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::OpenAi,
            )
        })? as u64;
    let mut prepared = None;
    let automatic_truncation = responses_options
        .as_ref()
        .is_some_and(ResponsesOptions::automatic_truncation);
    if automatic_truncation {
        payload
            .model_request_intent
            .get_or_insert_default()
            .automatic_history_truncation = true;
        let mut selected = prepare_upstream(
            &state,
            &account_ids,
            &trace_id,
            &route.mapped,
            &request.model,
            key_id.as_deref(),
            &allowed_models,
            Some(&service.service.id),
            if allow_model_fallback {
                config.features.default_model_id.as_str()
            } else {
                ""
            },
            locked_mapping_rule,
            &payload,
        )
        .await
        .map_err(|error| upstream_error(error, ErrorFormat::OpenAi))?;
        payload = selected.payload.clone();
        let original_history = payload.conversation_state.history.len();
        input_tokens = super::truncate_context_if_requested(
            &state,
            &mut payload,
            input_tokens,
            false,
            &selected.kiro_model,
        )
        .await
        .map_err(|message| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                message,
                ErrorFormat::OpenAi,
            )
        })?;
        if original_history != payload.conversation_state.history.len() {
            tracing::info!(
                trace_id,
                removed_messages = original_history - payload.conversation_state.history.len(),
                input_tokens,
                "Responses history truncated to the resolved model window"
            );
        }
        selected.payload = payload.clone();
        prepared = Some(selected);
    }
    // A fallback may trim additional history after this point. Native cache
    // usage remains authoritative; don't estimate it from the original input.
    let prompt_cache = (config.features.enable_prompt_cache && !automatic_truncation)
        .then(|| state.prompt_cache.openai_profile(&request, input_tokens))
        .flatten();
    let tool_tokens = state
        .tokenizer
        .estimate_kiro_tools(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::OpenAi,
            )
        })? as u64;
    let payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::OpenAi)?;
    let diagnostics = RequestDiagnostics {
        client_context: Some(client_context.clone()),
        original_tool_count: request.tools.len(),
        loaded_tool_count: loaded_tool_count(&payload),
        deferred_tool_count: 0,
        loaded_tool_bytes: loaded_tool_bytes(&payload),
        catalog_bytes: 0,
        tool_tokens,
        payload_bytes,
        ..RequestDiagnostics::default()
    };
    enforce_payload_budget(
        &state,
        tool_tokens,
        payload_bytes,
        loaded_tool_count(&payload),
        false,
        ErrorFormat::OpenAi,
    )?;
    // The dispatcher validates against the account's actual model. Checking
    // the provisional mapping here can reject input that its final target fits.
    let estimate = estimated_credits(
        input_tokens,
        max_tokens.unwrap_or(super::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
        &config.pool,
    );
    let reservation = reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::OpenAi)?;
    tracing::info!(
        event = "proxy.request.prepared",
        trace_id = %trace_id,
        protocol = "openai",
        requested_model = %log_model(&request.model),
        mapped_model = %route.mapped,
        input_tokens,
        original_tool_count = diagnostics.original_tool_count,
        tool_tokens,
        payload_bytes,
        loaded_tool_count = loaded_tool_count(&payload),
        loaded_tool_bytes = diagnostics.loaded_tool_bytes,
        max_tokens = ?max_tokens,
        estimated_credits = estimate,
        "upstream request prepared"
    );
    drop(body);
    let include_usage_chunk = request
        .stream_options
        .as_ref()
        .and_then(|options| options.get("include_usage"))
        .and_then(Value::as_bool)
        == Some(true);
    let request_id = responses_options
        .as_ref()
        .map(|options| options.id().to_owned())
        .unwrap_or_else(|| format!("chatcmpl-{}", Uuid::new_v4().simple()));
    let UpstreamExecution {
        lease,
        response: upstream,
        upstream_access_token,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execute_upstream(
        &state,
        &account_ids,
        &trace_id,
        &route.mapped,
        &request.model,
        key_id.as_deref(),
        &allowed_models,
        Some(&service.service.id),
        if allow_model_fallback {
            config.features.default_model_id.as_str()
        } else {
            ""
        },
        locked_mapping_rule,
        estimate,
        input_tokens,
        false,
        &payload,
        prepared,
        allow_model_fallback,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::OpenAi))?;
    if automatic_truncation {
        input_tokens = state
            .tokenizer
            .estimate_kiro_payload(&payload)
            .await
            .map_err(|message| ApiError::response_assembly(message, ErrorFormat::OpenAi))?
            as u64;
    }
    if request.stream {
        let legacy_functions = request.legacy_functions;
        let thinking_output_format = if responses_options.is_some() {
            crosslane_core::config::ThinkingOutputFormat::Openai
        } else {
            config.features.thinking_output_format
        };
        let response = stream::response(
            upstream,
            StreamProtocol::OpenAi,
            StreamContext {
                state,
                account_ids: Arc::clone(&account_ids),
                lease,
                upstream_access_token,
                reservation,
                trace_id,
                request_id,
                user_agent: logged_user_agent(&headers),
                path,
                service_id: service.service.id.clone(),
                model: request.model.clone(),
                mapped_model,
                original_model: request.model.clone(),
                api_key_id: key_id.clone(),
                allowed_models,
                lock_model_mapping: lock_kiro_mapping,
                allow_model_fallback,
                kiro_model,
                model_path,
                model_mapping_rule,
                attempts,
                input_tokens,
                compact: false,
                compaction_summary: None,
                compaction_replay: None,
                compaction_in_start: false,
                compaction_iteration: None,
                auto_compaction_original_input_tokens: None,
                context_edit_stats: Default::default(),
                estimated_credits: estimate,
                max_tokens,
                stop_sequences: request.stop_sequences(),
                started,
                prompt_cache,
                payload,
                auto_continue_rounds: config.features.auto_continue_rounds.min(30),
                buffer_tool_calls: config.features.buffer_tool_calls,
                enable_tool_leak_filter: config.features.enable_tool_leak_filter,
                thinking_enabled: decision.enabled,
                thinking_output_format,
                include_usage_chunk,
                web_tool_names: std::collections::HashMap::new(),
                tool_search: None,
                max_tool_search_operations: 0,
                tool_search_operations: 0,
                web_search_max_rounds: 0,
                web_search_client_limit: false,
                web_search_domains: WebSearchDomainFilter::default(),
                resumed_tool_searches: Vec::new(),
                resumed_web_searches: Vec::new(),
                resumed_server_events: Vec::new(),
                diagnostics,
                openai_tools,
                _attachment_guards: image_guards,
                _connection_guard: connection_guard,
                _admission_guard: admission_guard,
            },
        );
        return Ok(match responses_options {
            Some(options) => stream_response(response, options),
            None if legacy_functions => super::chat::legacy_stream(response),
            None => response,
        });
    }
    nonstream_openai(
        state,
        lease,
        reservation,
        upstream,
        payload,
        trace_id,
        request_id,
        path,
        logged_user_agent(&headers),
        request,
        decision.enabled,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        input_tokens,
        max_tokens,
        started,
        prompt_cache,
        diagnostics,
        openai_tools,
        responses_options,
    )
    .await
}

const REMOTE_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;
const REMOTE_DOCUMENT_MAX_BYTES: usize = 4_500_000;
const REMOTE_ATTACHMENT_MAX_REDIRECTS: usize = 3;
const REMOTE_ATTACHMENT_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy)]
pub(super) enum RemoteAttachmentKind {
    Image,
    Document,
}

#[derive(Debug)]
struct RemoteAttachment {
    bytes: Vec<u8>,
    media_type: String,
    final_url: Url,
}

#[derive(Debug)]
struct ClaudeRemoteAttachment {
    message_index: usize,
    block_pointer: String,
    kind: RemoteAttachmentKind,
    url: String,
}

pub(super) async fn hydrate_claude_attachments(
    state: &Arc<AppState>,
    request: &mut ClaudeRequest,
) -> Result<Vec<crate::state::BodyGuard>, ApiError> {
    let mut targets = Vec::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        collect_claude_remote_attachments(&message.content, message_index, "", &mut targets);
    }

    let guarded_targets = targets
        .into_iter()
        .map(|target| {
            let maximum = match target.kind {
                RemoteAttachmentKind::Image => REMOTE_IMAGE_MAX_BYTES,
                RemoteAttachmentKind::Document => REMOTE_DOCUMENT_MAX_BYTES,
            };
            reserve_remote_attachment(state, maximum, ErrorFormat::Claude)
                .map(|guard| (target, maximum, guard))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let fetched = futures::stream::iter(guarded_targets)
        .map(|(target, maximum, guard)| async move {
            let attachment = fetch_remote_attachment(&target.url, target.kind, maximum)
                .await
                .map_err(|message| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("unable to fetch remote attachment: {message}"),
                        ErrorFormat::Claude,
                    )
                })?;
            Ok::<_, ApiError>((target, attachment, guard))
        })
        .buffer_unordered(REMOTE_ATTACHMENT_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut guards = Vec::with_capacity(fetched.len());
    for (target, fetched, guard) in fetched {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&fetched.bytes);
        let block = request.messages[target.message_index]
            .content
            .pointer_mut(&target.block_pointer)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "remote attachment changed while preparing the request",
                    ErrorFormat::Claude,
                )
            })?;
        block["source"] = serde_json::json!({
            "type":"base64",
            "media_type":fetched.media_type,
            "data":encoded
        });
        if matches!(target.kind, RemoteAttachmentKind::Document)
            && block.get("title").is_none()
            && block.get("name").is_none()
        {
            let title = fetched
                .final_url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .filter(|segment| !segment.is_empty())
                .unwrap_or("document");
            block["title"] = Value::String(title.to_owned());
        }
        guards.push(guard);
    }
    Ok(guards)
}

fn collect_claude_remote_attachments(
    content: &Value,
    message_index: usize,
    pointer: &str,
    output: &mut Vec<ClaudeRemoteAttachment>,
) {
    let Some(blocks) = content.as_array() else {
        return;
    };
    for (index, block) in blocks.iter().enumerate() {
        let block_pointer = format!("{pointer}/{index}");
        match block.get("type").and_then(Value::as_str) {
            Some("image" | "document")
                if block.pointer("/source/type").and_then(Value::as_str) == Some("url") =>
            {
                if let Some(url) = block.pointer("/source/url").and_then(Value::as_str) {
                    output.push(ClaudeRemoteAttachment {
                        message_index,
                        block_pointer,
                        kind: if block.get("type").and_then(Value::as_str) == Some("image") {
                            RemoteAttachmentKind::Image
                        } else {
                            RemoteAttachmentKind::Document
                        },
                        url: url.to_owned(),
                    });
                }
            }
            Some("tool_result") => {
                if let Some(nested) = block.get("content") {
                    collect_claude_remote_attachments(
                        nested,
                        message_index,
                        &format!("{block_pointer}/content"),
                        output,
                    );
                }
            }
            Some("document")
                if block.pointer("/source/type").and_then(Value::as_str) == Some("content") =>
            {
                if let Some(nested) = block.pointer("/source/content") {
                    collect_claude_remote_attachments(
                        nested,
                        message_index,
                        &format!("{block_pointer}/source/content"),
                        output,
                    );
                }
            }
            _ => {}
        }
    }
}

async fn hydrate_openai_attachments(
    state: &Arc<AppState>,
    request: &mut OpenAiRequest,
) -> Result<Vec<crate::state::BodyGuard>, ApiError> {
    let mut targets = Vec::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(parts) = message.content.as_ref().and_then(Value::as_array) else {
            continue;
        };
        for (part_index, part) in parts.iter().enumerate() {
            if let Some(url) = part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .filter(|url| {
                    Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                })
            {
                targets.push((
                    message_index,
                    part_index,
                    RemoteAttachmentKind::Image,
                    url.to_owned(),
                ));
            } else if part.get("type").and_then(Value::as_str) == Some("document")
                && part.pointer("/source/type").and_then(Value::as_str) == Some("url")
            {
                if let Some(url) = part.pointer("/source/url").and_then(Value::as_str) {
                    targets.push((
                        message_index,
                        part_index,
                        RemoteAttachmentKind::Document,
                        url.to_owned(),
                    ));
                }
            }
        }
    }

    let guarded_targets = targets
        .into_iter()
        .map(|target| {
            let maximum = match target.2 {
                RemoteAttachmentKind::Image => REMOTE_IMAGE_MAX_BYTES,
                RemoteAttachmentKind::Document => REMOTE_DOCUMENT_MAX_BYTES,
            };
            reserve_remote_attachment(state, maximum, ErrorFormat::OpenAi)
                .map(|guard| (target, maximum, guard))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let fetched = futures::stream::iter(guarded_targets)
        .map(
            |((message_index, part_index, kind, url), maximum, guard)| async move {
                let image =
                    fetch_remote_attachment(&url, kind, maximum)
                        .await
                        .map_err(|message| {
                            ApiError::new(
                                StatusCode::BAD_GATEWAY,
                                format!("unable to fetch remote attachment: {message}"),
                                ErrorFormat::OpenAi,
                            )
                        })?;
                Ok::<_, ApiError>((message_index, part_index, kind, image, guard))
            },
        )
        .buffer_unordered(REMOTE_ATTACHMENT_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut guards = Vec::with_capacity(fetched.len());
    for (message_index, part_index, kind, fetched, guard) in fetched {
        let encoded = base64::engine::general_purpose::STANDARD.encode(fetched.bytes);
        let value = request.messages[message_index]
            .content
            .as_mut()
            .and_then(Value::as_array_mut)
            .and_then(|parts| parts.get_mut(part_index))
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "remote attachment changed while preparing the request",
                    ErrorFormat::OpenAi,
                )
            })?;
        match kind {
            RemoteAttachmentKind::Image => {
                value["image_url"]["url"] =
                    Value::String(format!("data:{};base64,{encoded}", fetched.media_type));
            }
            RemoteAttachmentKind::Document => {
                value["source"] = serde_json::json!({"type":"base64","media_type":fetched.media_type,"data":encoded});
                if value.get("title").is_none() {
                    value["title"] = Value::String(
                        fetched
                            .final_url
                            .path_segments()
                            .and_then(|mut segments| segments.next_back())
                            .filter(|name| !name.is_empty())
                            .unwrap_or("document")
                            .to_owned(),
                    );
                }
            }
        }
        guards.push(guard);
    }
    Ok(guards)
}

fn reserve_remote_attachment(
    state: &Arc<AppState>,
    decoded_maximum: usize,
    format: ErrorFormat,
) -> Result<crate::state::BodyGuard, ApiError> {
    let encoded_maximum = decoded_maximum
        .saturating_add(2)
        .saturating_div(3)
        .saturating_mul(4)
        .saturating_add(decoded_maximum)
        .saturating_add(16 * 1024);
    state.body_budget.reserve(encoded_maximum).ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "remote attachment memory budget exceeded",
            format,
        )
    })
}

async fn fetch_remote_attachment(
    value: &str,
    kind: RemoteAttachmentKind,
    maximum: usize,
) -> Result<RemoteAttachment, String> {
    let mut url = Url::parse(value).map_err(|_| "invalid URL")?;
    for redirect in 0..=REMOTE_ATTACHMENT_MAX_REDIRECTS {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("URL must be HTTP(S) and must not contain credentials".into());
        }
        let host = url.host_str().ok_or("URL has no host")?.to_owned();
        let port = url
            .port_or_known_default()
            .ok_or("URL has no usable port")?;
        let addresses = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| "host resolution failed")?
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|address| !is_public_address(address.ip()))
        {
            return Err("URL resolved to a non-public address".into());
        }
        // Pin every validated DNS answer, disable environment proxies, and
        // process redirects ourselves so every hop is resolved and checked.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve_to_addrs(&host, &addresses)
            .build()
            .map_err(|_| "unable to initialize HTTP client")?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| "remote request failed")?;
        if response.status().is_redirection() {
            if redirect == REMOTE_ATTACHMENT_MAX_REDIRECTS {
                return Err("too many redirects".into());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or("redirect did not include a valid Location header")?;
            url = url.join(location).map_err(|_| "invalid redirect URL")?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("remote server returned HTTP {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > maximum as u64)
        {
            return Err(format!("attachment exceeds the {maximum}-byte limit"));
        }
        let declared_media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "remote response body failed")?;
            if bytes.len().saturating_add(chunk.len()) > maximum {
                return Err(format!("attachment exceeds the {maximum}-byte limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err("remote attachment was empty".into());
        }
        let media_type =
            validate_remote_media_type(kind, declared_media_type.as_deref(), &url, &bytes)
                .ok_or("remote attachment has an unsupported or mismatched media type")?;
        return Ok(RemoteAttachment {
            bytes,
            media_type: media_type.into(),
            final_url: url,
        });
    }
    Err("too many redirects".into())
}

pub(super) fn validate_remote_media_type(
    kind: RemoteAttachmentKind,
    declared: Option<&str>,
    url: &Url,
    bytes: &[u8],
) -> Option<&'static str> {
    match kind {
        RemoteAttachmentKind::Image => {
            let detected = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                "image/jpeg"
            } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                "image/png"
            } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
                "image/gif"
            } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
                "image/webp"
            } else {
                return None;
            };
            let declared = match declared {
                Some("image/jpg") => Some("image/jpeg"),
                Some(value) => Some(value),
                None => None,
            };
            declared
                .is_none_or(|declared| declared == detected)
                .then_some(detected)
        }
        RemoteAttachmentKind::Document => {
            let extension = url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
                .map(str::to_ascii_lowercase);
            let candidate = match declared {
                Some("application/pdf") => "application/pdf",
                Some("text/csv" | "application/csv") => "text/csv",
                Some("text/markdown" | "text/x-markdown") => "text/markdown",
                Some("text/html") => "text/html",
                Some("application/msword") => "application/msword",
                Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document") => {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                }
                Some("application/vnd.ms-excel") => "application/vnd.ms-excel",
                Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet") => {
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                }
                Some(value) if value.starts_with("text/") => "text/plain",
                Some("application/octet-stream") | None => match extension.as_deref() {
                    Some("pdf") => "application/pdf",
                    Some("csv") => "text/csv",
                    Some("md" | "markdown") => "text/markdown",
                    Some("html" | "htm") => "text/html",
                    Some("txt") => "text/plain",
                    Some("doc") => "application/msword",
                    Some("docx") => {
                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    }
                    Some("xls") => "application/vnd.ms-excel",
                    Some("xlsx") => {
                        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                    }
                    _ => return None,
                },
                _ => return None,
            };
            let format = match candidate {
                "application/pdf" => "pdf",
                "application/msword" => "doc",
                "application/vnd.ms-excel" => "xls",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
                "text/csv" => "csv",
                "text/markdown" => "md",
                "text/html" => "html",
                "text/plain" => "txt",
                _ => return None,
            };
            crosslane_translate::document_bytes_match_format(format, bytes).then_some(candidate)
        }
    }
}

pub(super) fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !address.is_private()
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_unspecified()
                && !address.is_broadcast()
                && !address.is_multicast()
                && !address.is_documentation()
                && octets[0] != 0
                && octets[0] < 240
                && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
                && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                && !(octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
                && !(octets[0] == 198 && (18..=19).contains(&octets[1]))
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4() {
                return is_public_address(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (segments[0] & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
                || (segments[0] == 0x0064 && segments[1] == 0xff9b) // NAT64 well-known prefix
                || (segments[0] == 0x2001 && segments[1] == 0x0000) // Teredo
                || segments[0] == 0x2002 // 6to4
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)) // documentation
        }
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[test]
    fn remote_attachment_pointers_include_custom_document_images_and_tool_results() {
        let mut content = serde_json::json!([
            {"type":"image","source":{"type":"url","url":"https://example.com/first.png"}},
            {"type":"document","source":{"type":"content","content":[
                {"type":"text","text":"custom document"},
                {"type":"image","source":{"type":"url","url":"https://example.com/second.png"}}
            ]}},
            {"type":"tool_result","tool_use_id":"tool_1","content":[
                {"type":"document","source":{"type":"content","content":[
                    {"type":"image","source":{"type":"url","url":"https://example.com/third.png"}}
                ]}},
                {"type":"document","source":{"type":"url","url":"https://example.com/report.pdf"}}
            ]}
        ]);
        let mut targets = Vec::new();
        collect_claude_remote_attachments(&content, 7, "", &mut targets);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.block_pointer.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/0",
                "/1/source/content/1",
                "/2/content/0/source/content/0",
                "/2/content/1"
            ]
        );
        assert!(targets.iter().all(|target| target.message_index == 7));
        assert!(targets[..3]
            .iter()
            .all(|target| matches!(target.kind, RemoteAttachmentKind::Image)));
        assert!(matches!(targets[3].kind, RemoteAttachmentKind::Document));
        for target in targets {
            let block = content
                .pointer_mut(&target.block_pointer)
                .expect("collected block");
            assert_eq!(block["source"]["url"].as_str(), Some(target.url.as_str()));
            block["source"] = serde_json::json!({"type":"base64","data":"hydrated"});
        }
        let mut remaining = Vec::new();
        collect_claude_remote_attachments(&content, 7, "", &mut remaining);
        assert!(remaining.is_empty());
        assert_eq!(
            content[1]["source"]["content"][0]["text"],
            "custom document"
        );
    }
}
