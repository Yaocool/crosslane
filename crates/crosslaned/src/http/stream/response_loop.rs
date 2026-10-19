use super::super::handlers::{
    current_message_has_tool_results, empty_tool_result_disposition, EmptyToolResultDisposition,
};
use super::super::tool_names::{ClientToolNames, ToolNameTrace, TOOL_NAME_MISMATCH_CODE};
use super::{
    accumulate_usage, apply_stream_stop, auto_continue_payload, build_claude_state,
    classified_stream_error, classify_stream_failure, claude_initial_events, client_visible_event,
    event_is_tool_search, event_is_web_search, event_kind, fill_missing_usage, finish_accounting,
    header, json, now_secs, prepend_pending_initial, record_account_scoped_stream_failure,
    repair_json, restore_tool_event_name, should_buffer_tool_event, sse, stream_error,
    stream_event, stream_finish, tool_search_continue_payload_batch,
    web_search_continue_payload_batch, web_search_replay_failure, Body, Bytes, BytesMut,
    ClaudeToolSearchBudget, ClaudeWebSearchTrace, DecodedResponse, Decoder, EventStreamDecoder,
    HashSet, HeaderValue, Infallible, KiroEvent, KiroResponse, KiroToolUse, Response, StatusCode,
    StopSequenceFilter, StreamContext, StreamExt, StreamFailureDiagnostics, StreamFailureScope,
    StreamProtocol, ThinkingContentFilter, ToolLeakFilter, UpstreamStreamMetrics, Value,
};

/// Keep empty attempts retryable without delaying valid visible output until
/// EOF. Returning the whole prefix preserves event order.
fn guard_tool_result_output(
    guarded: &mut bool,
    buffered: &mut Vec<String>,
    confirms_valid_output: bool,
    output: Vec<String>,
) -> Vec<String> {
    if !*guarded {
        return output;
    }
    buffered.extend(output);
    if confirms_valid_output {
        *guarded = false;
        std::mem::take(buffered)
    } else {
        Vec::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn retry_mapped_model(
    locked: bool,
    current: &str,
    original: &str,
    fallback: Option<&str>,
    config: &crosslane_core::config::Config,
    service_id: &str,
    key_id: Option<&str>,
    remaining_percent: Option<f64>,
) -> String {
    if let Some(fallback) = fallback {
        return fallback.to_owned();
    }
    if locked {
        return current.to_owned();
    }
    crosslane_translate::model::map_model_for_provider(
        original,
        &config.model_mapping,
        crosslane_translate::model::ModelMappingContext {
            provider_id: "kiro",
            service_id: Some(service_id),
            api_key_id: key_id,
            remaining_percent,
        },
        "",
    )
    .mapped
}

pub fn response(
    upstream: KiroResponse,
    protocol: StreamProtocol,
    mut context: StreamContext,
) -> Response {
    let mut effective_thinking = upstream.thinking_enabled();
    let (initial_endpoint, initial_response, initial_permit) = upstream.into_parts();
    let mut upstream_permit = Some(initial_permit);
    let mut source = Some(initial_response.bytes_stream());
    let mut endpoint = initial_endpoint.name.to_string();
    let mut keepalive = context.state.keepalive.subscribe();
    let upstream_config = context.state.runtime_config_snapshot().upstream;
    let configured_read_timeout_ms = upstream_config.stream_read_timeout_ms;
    let configured_pool_idle_timeout_ms = upstream_config
        .pool
        .keep_alive_idle_ms
        .saturating_sub(2_000)
        .max(1)
        .min(upstream_config.pool.keep_alive_max_ms.max(1));
    let bridge_trace_id = context.trace_id.clone();
    let bridge_request_id = context.request_id.clone();
    let bridge_user_agent = context.user_agent.clone();
    tracing::info!(
        event = "proxy.stream.started",
        trace_id = %context.trace_id,
        request_id = %context.request_id,
        provider_id = "kiro",
        user_agent = %context.user_agent,
        protocol = protocol.as_str(),
        account_id = %context.lease.account_id(),
        endpoint,
        model = %context.model,
        upstream_total_timeout = "disabled",
        upstream_connect_timeout_ms = 15_000u64,
        upstream_stream_slot_wait_timeout_ms = upstream_config.stream_slot_wait_timeout_ms,
        upstream_stream_read_timeout_ms = configured_read_timeout_ms,
        upstream_pool_idle_timeout_ms = configured_pool_idle_timeout_ms,
        "client stream response started"
    );
    let stream = async_stream::stream! {
        let mut buffer = BytesMut::new();
        let mut decoder = EventStreamDecoder;
        let mut decoded = DecodedResponse::default();
        let mut stop_filter = StopSequenceFilter::new(&context.stop_sequences);
        let mut accumulated_usage = crosslane_kiro::UsageInfo::default();
        let mut prompt_cache_plan = context.state.prompt_cache.plan(
            &context.lease.account_id(),
            context.prompt_cache.as_ref(),
        );
        let mut claude = build_claude_state(&context, &prompt_cache_plan);
        let mut accumulated_searches = std::mem::take(&mut context.resumed_tool_searches);
        let mut accumulated_web_searches = std::mem::take(&mut context.resumed_web_searches);
        let mut data_started = false;
        let mut failed = None;
        let mut pending_initial = if matches!(protocol, StreamProtocol::Claude) {
            match claude_initial_events(
                &mut claude,
                context.compaction_summary.as_deref(),
                &context.resumed_server_events,
                &accumulated_searches,
                &accumulated_web_searches,
            ) {
                Ok(events) => events,
                Err(error) => {
                    failed = Some(web_search_replay_failure(
                        &context.trace_id,
                        &context.request_id,
                        &context.user_agent,
                        &error,
                    ));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let created = now_secs();
        let mut stream_failure = None::<StreamFailureDiagnostics>;
        let mut upstream_metrics = UpstreamStreamMetrics::new();
        let mut upstream_access_token = context.upstream_access_token.clone();
        let mut pre_data_retries = 0;
        let mut attempted_accounts = HashSet::new();
        let mut fallback_model = None::<String>;
        let mut accumulated_text = String::new();
        let mut accumulated_reasoning = String::new();
        let mut payload = context.payload.clone();
        let tool_names = match protocol {
            StreamProtocol::Claude => ClientToolNames::claude(&context.web_tool_names),
            StreamProtocol::OpenAi => ClientToolNames::openai(&context.openai_tools),
        };
        let client_has_tools = payload.conversation_state.current_message.user_input_message
            .user_input_message_context.as_ref().is_some_and(|context| !context.tools.is_empty());
        // Server calls must be emitted before client tool calls in mixed
        // turns, so buffering is mandatory whenever a server tool is active.
        // A pending Claude prelude also forces buffering: a tool event is not
        // a successful semantic boundary until its complete JSON validates.
        let buffer_tool_calls = context.buffer_tool_calls
            || context.tool_search.is_some()
            || context.web_search_max_rounds > 0
            || !pending_initial.is_empty()
            || !client_has_tools;
        let mut auto_round = 0;
        let mut search_round = 0u32;
        let mut empty_tool_result_retries = 0u8;
        #[cfg(feature = "crs")]
        let mut diagnostic_round = 0u64;
        let mut tool_search_operations = context.tool_search_operations;
        let mut web_search_round = accumulated_web_searches
            .iter()
            .filter(|search| search.executed)
            .count() as u32;
        'rounds: loop {
            #[cfg(feature = "crs")]
            let mut event_diagnostics = {
                diagnostic_round = diagnostic_round.saturating_add(1);
                super::super::crs::KiroEventDiagnostics::new(
                    &context.trace_id,
                    &context.request_id,
                    &endpoint,
                    true,
                    diagnostic_round,
                )
            };
            // A Kiro round following tool output is not semantically complete
            // until it contains visible text, another tool call, or a terminal
            // reason. Buffer reasoning-only attempts, but release the guard as
            // soon as valid text appears so successful turns remain streaming.
            let tool_result_round = current_message_has_tool_results(&payload);
            // Hold a compaction prelude until this attempt produces valid
            // output. Empty ordinary attempts can then retry without emitting
            // duplicate boundaries or assistant blocks.
            let mut guards_tool_result_round = true;
            let mut guarded_output = Vec::new();
            let mut claude_checkpoint = guards_tool_result_round.then(|| claude.clone());
            let mut pending_initial_checkpoint =
                guards_tool_result_round.then(|| pending_initial.clone());
            if let Some(message) = failed.as_deref() {
                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                    &protocol,
                    &context.request_id,
                    message,
                )));
                break 'rounds;
            }
            let mut leak_filter = ToolLeakFilter::for_payload(context.enable_tool_leak_filter, &payload);
            let mut upstream_tool_names = std::collections::HashMap::new();
            let mut thinking_filter = ThinkingContentFilter::new(context.thinking_enabled && effective_thinking)
                .with_omitted_summary(payload.thinking_summary_omitted());
            loop {
                tokio::select! {
                    chunk = source.as_mut().expect("active upstream stream").next() => match chunk {
                        Some(Ok(chunk)) => {
                            upstream_metrics.observe_chunk(chunk.len());
                            buffer.extend_from_slice(&chunk);
                            loop {
                                match decoder.decode(&mut buffer) {
                                    Ok(Some(event)) => {
                                        upstream_metrics.observe_event();
                                        #[cfg(feature = "crs")]
                                        event_diagnostics.observe(&event);
                                        tracing::trace!(
                                            trace_id = %context.trace_id,
                                            request_id = %context.request_id,
                                            event = event_kind(&event),
                                            "upstream stream event decoded"
                                        );
                                        if let KiroEvent::Error { kind, message } = &event {
                                            stream_failure = Some(
                                                StreamFailureDiagnostics::from_upstream_event(
                                                    &upstream_metrics,
                                                    buffer.len(),
                                                    configured_read_timeout_ms,
                                                ),
                                            );
                                            failed = Some(format!("{kind}: {message}"));
                                            break;
                                        }
                                        let events = leak_filter
                                            .push(event)
                                            .into_iter()
                                            .flat_map(|event| thinking_filter.push(event))
                                            .collect::<Vec<_>>();
                                        for mut event in events {
                                            restore_tool_event_name(&mut event, &mut upstream_tool_names);
                                            let internal_search = event_is_tool_search(
                                                &event,
                                                context.tool_search.as_deref(),
                                            );
                                            let internal_web_search = event_is_web_search(
                                                &event,
                                                context.web_search_max_rounds,
                                            );
                                            if !internal_search && !internal_web_search && client_has_tools {
                                                if let Err(message) = tool_names.restore_event(&mut event, &ToolNameTrace {
                                                    trace_id: &context.trace_id,
                                                    request_id: &context.request_id,
                                                    path: &context.path,
                                                    streaming: true,
                                                }) {
                                                    failed = Some(message.to_owned());
                                                    break;
                                                }
                                            }
                                            if matches!(&event, KiroEvent::Reasoning { .. } | KiroEvent::Citations { .. } | KiroEvent::ToolUse { .. }) {
                                                let pending = stop_filter.finish();
                                                if !pending.is_empty() {
                                                    let confirms_valid_text = !pending.trim().is_empty();
                                                    let output = stream_event(
                                                        &protocol,
                                                        &mut claude,
                                                        &KiroEvent::AssistantResponse { content: pending },
                                                        created,
                                                        &context.model,
                                                        context.thinking_output_format,
                                                        &context.openai_tools,
                                                    );
                                                    let output = prepend_pending_initial(
                                                        &mut pending_initial,
                                                        output,
                                                    );
                                                    let output = guard_tool_result_output(
                                                        &mut guards_tool_result_round, &mut guarded_output,
                                                        confirms_valid_text, output,
                                                    );
                                                    data_started |= !output.is_empty();
                                                    for data in output {
                                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                                    }
                                                }
                                            }
                                            let visible_event = client_visible_event(
                                                &event,
                                                &mut stop_filter,
                                            );
                                            if !internal_search && !internal_web_search {
                                                if let Some(visible_event) = visible_event.as_ref().filter(|event| {
                                                    !should_buffer_tool_event(
                                                        event,
                                                        buffer_tool_calls,
                                                        &context.openai_tools,
                                                    )
                                                }) {
                                                    // Unbuffered ordinary tool arguments are already
                                                    // client-visible output and must keep streaming.
                                                    let confirms_valid_output = matches!(visible_event, KiroEvent::AssistantResponse { content } if !content.trim().is_empty())
                                                        || (!tool_result_round && (matches!(visible_event, KiroEvent::Reasoning { content, .. } if !content.trim().is_empty())
                                                            || matches!(visible_event, KiroEvent::ToolUse { .. })));
                                                    let output = stream_event(
                                                        &protocol,
                                                        &mut claude,
                                                        visible_event,
                                                        created,
                                                        &context.model,
                                                        context.thinking_output_format,
                                                        &context.openai_tools,
                                                    );
                                                    let output = prepend_pending_initial(
                                                        &mut pending_initial,
                                                        output,
                                                    );
                                                    let output = guard_tool_result_output(
                                                        &mut guards_tool_result_round, &mut guarded_output,
                                                        confirms_valid_output, output,
                                                    );
                                                    data_started |= !output.is_empty();
                                                    for data in output {
                                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                                    }
                                                }
                                            }
                                            if let Err(error) = decoded.push(event) {
                                                failed = Some(error);
                                                break;
                                            }
                                            if stop_filter.matched().is_some() {
                                                break;
                                            }
                                        }
                                        if failed.is_some() || stop_filter.matched().is_some() {
                                            break;
                                        }
                                    }
                                    Ok(None) => break,
                                    Err(error) => {
                                        stream_failure = Some(
                                            StreamFailureDiagnostics::from_event_stream(
                                                "event_stream_decode",
                                                &error,
                                                &upstream_metrics,
                                                buffer.len(),
                                                configured_read_timeout_ms,
                                            ),
                                        );
                                        failed = Some(error.to_string());
                                        break;
                                    }
                                }
                            }
                            if failed.is_some() || stop_filter.matched().is_some() { break; }
                        }
                        Some(Err(error)) => {
                            stream_failure = Some(StreamFailureDiagnostics::from_http_body(
                                &error,
                                &upstream_metrics,
                                buffer.len(),
                                configured_read_timeout_ms,
                            ));
                            failed = Some(error.to_string());
                            break;
                        }
                        None => {
                            if let Err(error) = decoder.decode_eof(&mut buffer) {
                                stream_failure = Some(
                                    StreamFailureDiagnostics::from_event_stream(
                                        "event_stream_eof",
                                        &error,
                                        &upstream_metrics,
                                        buffer.len(),
                                        configured_read_timeout_ms,
                                    ),
                                );
                                failed = Some(error.to_string());
                            }
                            break;
                        },
                    },
                    heartbeat = keepalive.recv() => {
                        if heartbeat.is_ok() {
                            // Response headers are already committed when this
                            // body is polled. A ping is transport-only, so it
                            // deliberately does not flip `data_started`; a
                            // slow upstream can still be retried before its
                            // first semantic event.
                            let ping = match protocol {
                                StreamProtocol::Claude => sse(&json!({"type":"ping"})),
                                StreamProtocol::OpenAi => ": keepalive\n\n".into(),
                            };
                            yield Ok::<Bytes, Infallible>(Bytes::from(ping));
                        }
                    }
                }
            }
            // End the previous transport before any token refresh, server tool,
            // retry, or continuation can acquire another streaming slot.
            drop(source.take());
            drop(upstream_permit.take());
            if stop_filter.matched().is_none() {
                let mut events = Vec::new();
                for event in leak_filter.finish() {
                    events.extend(thinking_filter.push(event));
                }
                events.extend(thinking_filter.finish());
                for mut event in events {
                    restore_tool_event_name(&mut event, &mut upstream_tool_names);
                    let internal_search =
                        event_is_tool_search(&event, context.tool_search.as_deref());
                    let internal_web_search =
                        event_is_web_search(&event, context.web_search_max_rounds);
                    if !internal_search && !internal_web_search && client_has_tools {
                        if let Err(message) = tool_names.restore_event(&mut event, &ToolNameTrace {
                            trace_id: &context.trace_id,
                            request_id: &context.request_id,
                            path: &context.path,
                            streaming: true,
                        }) {
                            failed = Some(message.to_owned());
                            break;
                        }
                    }
                    if matches!(
                        &event,
                        KiroEvent::Reasoning { .. }
                            | KiroEvent::Citations { .. }
                            | KiroEvent::ToolUse { .. }
                    ) {
                        let pending = stop_filter.finish();
                        if !pending.is_empty() {
                            let confirms_valid_text = !pending.trim().is_empty();
                            let output = stream_event(
                                &protocol,
                                &mut claude,
                                &KiroEvent::AssistantResponse { content: pending },
                                created,
                                &context.model,
                                context.thinking_output_format,
                                &context.openai_tools,
                            );
                            let output = prepend_pending_initial(&mut pending_initial, output);
                            let output = guard_tool_result_output(
                                &mut guards_tool_result_round, &mut guarded_output,
                                confirms_valid_text, output,
                            );
                            data_started |= !output.is_empty();
                            for data in output {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                    }
                    let visible_event = client_visible_event(&event, &mut stop_filter);
                    if !internal_search && !internal_web_search {
                        if let Some(visible_event) = visible_event.as_ref().filter(|event| {
                            !should_buffer_tool_event(
                                event,
                                buffer_tool_calls,
                                &context.openai_tools,
                            )
                        }) {
                            let confirms_valid_output = matches!(visible_event, KiroEvent::AssistantResponse { content } if !content.trim().is_empty())
                                || (!tool_result_round && (matches!(visible_event, KiroEvent::Reasoning { content, .. } if !content.trim().is_empty())
                                    || matches!(visible_event, KiroEvent::ToolUse { .. })));
                            let output = stream_event(
                                &protocol,
                                &mut claude,
                                visible_event,
                                created,
                                &context.model,
                                context.thinking_output_format,
                                &context.openai_tools,
                            );
                            let output = prepend_pending_initial(&mut pending_initial, output);
                            let output = guard_tool_result_output(
                                &mut guards_tool_result_round, &mut guarded_output,
                                confirms_valid_output, output,
                            );
                            data_started |= !output.is_empty();
                            for data in output {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                    }
                    if let Err(error) = decoded.push(event) {
                        failed = Some(error);
                        break;
                    }
                    if stop_filter.matched().is_some() {
                        break;
                    }
                }
            }
            // Buffered tool calls have not produced client-visible data yet.
            // Finalize them before committing the pending message/compaction
            // prelude so malformed tool JSON can still use the pre-data retry
            // path and cannot publish a failed compaction boundary.
            if failed.is_none() && stop_filter.matched().is_none() {
                if let Err(error) = decoded.finalize_tool_inputs() {
                    failed = Some(match tool_names.validate_decoded(&decoded, &ToolNameTrace {
                        trace_id: &context.trace_id,
                        request_id: &context.request_id,
                        path: &context.path,
                        streaming: true,
                    }) {
                        Ok(()) => error,
                        Err(message) => message.to_owned(),
                    });
                }
            }
            if failed.is_some() {
                #[cfg(feature = "crs")]
                event_diagnostics.log_summary(Some(&decoded), "failed");
                // Once semantic output has been emitted, neither replay the
                // attempt nor restore the pre-output protocol state.
                if !data_started {
                    if let Some(checkpoint) = claude_checkpoint.take() {
                        claude = checkpoint;
                    }
                    if let Some(checkpoint) = pending_initial_checkpoint.take() {
                        pending_initial = checkpoint;
                    }
                }
                let failure_text = failed.as_deref().unwrap_or_default().to_string();
                let failed_account_id = context.lease.account_id();
                let failure_details =
                    classify_stream_failure(&failure_text, stream_failure.as_ref());
                let is_auth = failure_details.is_auth_error();
                let is_quota = failure_details.is_quota_error();
                let is_throttle = failure_details.is_throttle_error();
                let is_model_unavailable = failure_details.is_model_unavailable();
                let is_model_capacity_error = is_throttle || is_model_unavailable;
                let is_request_rejection = failure_details.is_request_rejection();
                if is_model_capacity_error {
                    context.state.record_stream_overload();
                }
                let failure_diagnostics = stream_failure.clone().unwrap_or_default();
                let stream_failure_kind = if failure_diagnostics.kind.is_empty() {
                    "none"
                } else {
                    failure_diagnostics.kind
                };
                let transport_error_class = if failure_diagnostics.transport_class.is_empty() {
                    "none"
                } else {
                    failure_diagnostics.transport_class
                };
                tracing::warn!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    user_agent = %context.user_agent,
                    account_id = %failed_account_id,
                    endpoint,
                    data_started,
                    auth_error = is_auth,
                    quota_error = is_quota,
                    throttle_error = is_throttle,
                    model_unavailable = is_model_unavailable,
                    request_rejection = is_request_rejection,
                    error_code = failure_details.error_code,
                    error_stage = failure_details.error_stage,
                    failure_scope = failure_details.scope.as_str(),
                    account_error = failure_details.account_error(),
                    stream_failure_kind,
                    transport_error_class,
                    transport_timeout = failure_diagnostics.transport_timeout,
                    transport_decode = failure_diagnostics.transport_decode,
                    transport_body = failure_diagnostics.transport_body,
                    transport_connect = failure_diagnostics.transport_connect,
                    transport_error_chain = %failure_diagnostics.source_chain,
                    upstream_stream_elapsed_ms = failure_diagnostics.stream_elapsed_ms,
                    upstream_idle_ms = failure_diagnostics.upstream_idle_ms,
                    upstream_chunk_seen = failure_diagnostics.chunk_seen,
                    upstream_chunks = failure_diagnostics.chunks,
                    upstream_bytes = failure_diagnostics.bytes,
                    upstream_events = failure_diagnostics.events,
                    upstream_buffered_bytes = failure_diagnostics.buffered_bytes,
                    configured_stream_read_timeout_ms = failure_diagnostics.configured_read_timeout_ms,
                    error = %crosslane_translate::sanitize_error_message(&failure_text),
                    "upstream stream failed"
                );
                let config = context.state.config.current();
                // This budget includes same-account auth/model retries. Pool
                // size only bounds candidate switches; it cannot add retries.
                let retry_limit = config.upstream.max_retries;
                let may_switch = (!is_quota || config.pool.auto_switch_on_quota_exhausted)
                    && !is_request_rejection
                    && failure_details.scope != StreamFailureScope::Proxy
                    && failure_details.error_code != TOOL_NAME_MISMATCH_CODE;
                let mut account_health_handled = false;
                if !data_started && pre_data_retries < retry_limit && may_switch && is_auth {
                    let mut disable_account = true;
                    if context
                        .state
                        .refresh_account_token_after_auth_failure(
                            &context.state.pool(),
                            &failed_account_id,
                            &upstream_access_token,
                        )
                        .await
                        .is_ok()
                    {
                        tracing::info!(
                            trace_id = %context.trace_id,
                            request_id = %context.request_id,
                            account_id = %failed_account_id,
                            "stream account token refreshed"
                        );
                        let account = context.lease.account().await;
                        payload.profile_arn.clone_from(&account.profile_arn);
                        pre_data_retries += 1;
                        match context.state.generate(&account, &payload).await {
                            Ok(retry) => {
                                upstream_access_token = account.credentials.access_token.clone();
                                effective_thinking = retry.thinking_enabled();
                                let (next_endpoint, next_response, next_permit) = retry.into_parts();
                                endpoint = next_endpoint.name.to_string();
                                source = Some(next_response.bytes_stream());
                                upstream_metrics.reset();
                                stream_failure = None;
                                upstream_permit = Some(next_permit);
                                buffer.clear();
                                decoder = EventStreamDecoder;
                                decoded = DecodedResponse::default();
                                stop_filter = StopSequenceFilter::new(&context.stop_sequences);
                                failed = None;
                                tracing::info!(
                                    trace_id = %context.trace_id,
                                    request_id = %context.request_id,
                                    account_id = %failed_account_id,
                                    endpoint,
                                    retry = pre_data_retries,
                                    "retrying stream after token refresh"
                                );
                                continue 'rounds;
                            }
                            Err(error) => {
                                disable_account = error.is_auth();
                                stream_failure = None;
                                failed = Some(error.to_string());
                            }
                        }
                    }
                    if disable_account {
                        context.state.pool().mark_banned(&failed_account_id).await;
                        account_health_handled = true;
                        let account_name = if let Some(runtime) =
                            context.state.pool().get(&failed_account_id).await
                        {
                            runtime.account.read().await.display_name().to_owned()
                        } else {
                            failed_account_id.clone()
                        };
                        crate::alerts::emit_token_refresh_failure(
                            &context.state,
                            &failed_account_id,
                            &account_name,
                            "刷新后流式请求的上游认证仍然失败",
                        );
                    }
                }

                if !data_started
                    && pre_data_retries < retry_limit
                    && may_switch
                    && is_model_capacity_error
                    && context.allow_model_fallback
                    && config.features.enable_model_fallback
                    && fallback_model.is_none()
                {
                    let (mut models, _) = context.state.models.get(config.models.cache_ttl_ms);
                    models.retain(|model| !context.state.config.current().is_model_disabled(&model.model_id));
                    if let Some(fallback) =
                        super::super::handlers::find_model_fallback(&context.kiro_model, &models)
                            .filter(|fallback| {
                                super::super::handlers::model_is_allowed(
                                    &context.allowed_models,
                                    "kiro",
                                    fallback,
                                )
                            })
                    {
                        let account = context.lease.account().await;
                        let resolved = if let Some(runtime) =
                            context.state.pool().get(&account.id).await
                        {
                            let resolved = runtime.resolve_model(&fallback).await;
                            if resolved.is_none() && !runtime.has_model_cache().await {
                                Some(fallback.clone())
                            } else {
                                resolved
                            }
                        } else {
                            Some(fallback.clone())
                        };
                        if let Some(resolved) = resolved {
                            let fallback_input_tokens = match super::super::handlers::truncate_context_if_requested(
                            &context.state, &mut payload, context.input_tokens, context.compact, &resolved,
                        ).await {
                            Ok(tokens) => tokens,
                            Err(error) => {
                                failed = Some(error.clone());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &context.request_id, &error)));
                                break 'rounds;
                            }
                        };
                            let fits_context = super::super::handlers::check_context_limit(
                                &context.state,
                                fallback_input_tokens,
                                context.compact,
                                &resolved,
                            )
                            .is_ok();
                            if !fits_context {
                                tracing::warn!(
                                    trace_id = %context.trace_id,
                                    request_id = %context.request_id,
                                    user_agent = %context.user_agent,
                                    fallback_model = %resolved,
                                    input_tokens = context.input_tokens,
                                    "skipping stream model fallback because its context window is too small"
                                );
                            }
                            if fits_context {
                                context.input_tokens = fallback_input_tokens;
                                fallback_model = Some(fallback.clone());
                                context.mapped_model.clone_from(&fallback);
                                context.kiro_model.clone_from(&resolved);
                                super::super::handlers::set_payload_model(&mut payload, &resolved);
                                pre_data_retries += 1;
                                match context.state.generate(&account, &payload).await {
                                    Ok(retry) => {
                                        upstream_access_token =
                                            account.credentials.access_token.clone();
                                        effective_thinking = retry.thinking_enabled();
                                        let (next_endpoint, next_response, next_permit) =
                                            retry.into_parts();
                                        endpoint = next_endpoint.name.to_string();
                                        source = Some(next_response.bytes_stream());
                                        upstream_metrics.reset();
                                        stream_failure = None;
                                        upstream_permit = Some(next_permit);
                                        buffer.clear();
                                        decoder = EventStreamDecoder;
                                        decoded = DecodedResponse::default();
                                        stop_filter =
                                            StopSequenceFilter::new(&context.stop_sequences);
                                        failed = None;
                                        continue 'rounds;
                                    }
                                    Err(error) => {
                                        stream_failure = None;
                                        failed = Some(error.to_string());
                                    }
                                }
                            }
                        } else {
                            tracing::debug!(
                                trace_id = %context.trace_id,
                                request_id = %context.request_id,
                                account_id = %account.id,
                                fallback_model = %fallback,
                                "stream fallback is unavailable in the current service account"
                            );
                        }
                    }
                }

                // Token refresh and model fallback dispatches can replace the
                // original stream failure. Reclassify the active error before
                // mutating health or deciding whether another account may run.
                let active_failure_text = failed.as_deref().unwrap_or_default();
                let failure_details =
                    classify_stream_failure(active_failure_text, stream_failure.as_ref());
                let is_quota = failure_details.is_quota_error();
                let is_request_rejection = failure_details.is_request_rejection();
                let may_switch = (!is_quota || config.pool.auto_switch_on_quota_exhausted)
                    && !is_request_rejection
                    && failure_details.scope != StreamFailureScope::Proxy
                    && failure_details.error_code != TOOL_NAME_MISMATCH_CODE;
                if !account_health_handled {
                    record_account_scoped_stream_failure(
                        &context.state,
                        &context.trace_id,
                        &context.request_id,
                        &context.user_agent,
                        &failed_account_id,
                        failure_details,
                    )
                    .await;
                }
                attempted_accounts.insert(failed_account_id.clone());
                if !data_started && pre_data_retries < retry_limit && may_switch {
                    let (new_lease, account) = loop {
                        let pool = context.state.pool();
                        let candidate_ids = pool
                            .matching_account_ids(|account| {
                                context.account_ids.contains(&account.id)
                                    && !attempted_accounts.contains(&account.id)
                            })
                            .await;
                        if candidate_ids.is_empty() {
                            if let Some(message) = failed.as_deref() {
                                yield Ok::<Bytes, Infallible>(Bytes::from(classified_stream_error(
                                    &protocol,
                                    message,
                                    &context.request_id,
                                    stream_failure.as_ref(),
                                )));
                            }
                            break 'rounds;
                        }
                        let new_lease = match pool
                            .acquire_scoped_excluding(
                                &context.kiro_model,
                                context.estimated_credits,
                                &context.account_ids,
                                &attempted_accounts,
                            )
                            .await
                        {
                            Ok(lease) => lease,
                            Err(error) => {
                                if context
                                    .state
                                    .pool()
                                    .all_matching_credit_exhausted_scoped(
                                        &context.kiro_model,
                                        &context.account_ids,
                                    )
                                    .await
                                {
                                    crate::alerts::sync_service_quota(&context.state).await;
                                }
                                stream_failure = None;
                                failed = Some(error.to_string());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                    &protocol,
                                    &context.request_id,
                                    &error.to_string(),
                                )));
                                break 'rounds;
                            }
                        };
                        let account = new_lease.account().await;
                        let mut incompatible = false;
                        if let Some(runtime) = context.state.pool().get(&account.id).await {
                        let remaining = account.usage.as_ref().and_then(|usage| {
                            crosslane_pool::remaining_credit_percent(usage, &config.pool)
                        });
                        context.mapped_model = retry_mapped_model(
                            context.lock_model_mapping,
                            &context.mapped_model,
                            &context.original_model,
                            fallback_model.as_deref(),
                            &config,
                            &context.service_id,
                            context.api_key_id.as_deref(),
                            remaining,
                        );
                        context.kiro_model.clone_from(&context.mapped_model);
                        if let Some(resolved) = runtime.resolve_model(&context.kiro_model).await {
                            context.kiro_model = resolved;
                            super::super::handlers::set_payload_model(&mut payload, &context.kiro_model);
                        } else if runtime.has_model_cache().await {
                            if let Some(resolved) = runtime
                                .resolve_model(&config.features.default_model_id)
                                .await
                            {
                                context.kiro_model = resolved;
                                super::super::handlers::set_payload_model(
                                    &mut payload,
                                    &context.kiro_model,
                                );
                            } else {
                                incompatible = true;
                            }
                        } else if let Some(resolved) = super::super::handlers::resolve_static_model(
                            &account,
                            &context.kiro_model,
                        ) {
                            context.kiro_model = resolved;
                            super::super::handlers::set_payload_model(
                                &mut payload,
                                &context.kiro_model,
                            );
                        }
                        if !incompatible
                            && !super::super::handlers::model_is_allowed(
                                &context.allowed_models,
                                "kiro",
                                &context.kiro_model,
                            )
                        {
                            incompatible = true;
                            tracing::warn!(
                                trace_id = %context.trace_id,
                                request_id = %context.request_id,
                                user_agent = %context.user_agent,
                                account_id = %account.id,
                                resolved_model = %context.kiro_model,
                                "skipping stream retry account because its resolved model is outside the API key model scope"
                            );
                        }
                        let retry_input_tokens = if incompatible {
                            context.input_tokens
                        } else {
                            match super::super::handlers::truncate_context_if_requested(
                                &context.state, &mut payload, context.input_tokens, context.compact, &context.kiro_model,
                            ).await {
                                Ok(tokens) => tokens,
                                Err(error) => {
                                    failed = Some(error.clone());
                                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &context.request_id, &error)));
                                    break 'rounds;
                                }
                            }
                        };
                        if !incompatible
                            && super::super::handlers::check_context_limit(
                                &context.state,
                                retry_input_tokens,
                                context.compact,
                                &context.kiro_model,
                            )
                            .is_err()
                        {
                            incompatible = true;
                            tracing::warn!(
                                trace_id = %context.trace_id,
                                request_id = %context.request_id,
                                user_agent = %context.user_agent,
                                account_id = %account.id,
                                resolved_model = %context.kiro_model,
                                input_tokens = context.input_tokens,
                                "skipping stream retry account because resolved model context is too small"
                            );
                        }
                        if !incompatible {
                            context.input_tokens = retry_input_tokens;
                        }
                        }
                        if incompatible {
                            attempted_accounts.insert(account.id);
                            drop(new_lease);
                            continue;
                        }
                        break (new_lease, account);
                    };
                    context.lease = new_lease;
                    prompt_cache_plan = context.state.prompt_cache.plan(
                        &context.lease.account_id(),
                        context.prompt_cache.as_ref(),
                    );
                    claude = build_claude_state(&context, &prompt_cache_plan);
                    pending_initial = if matches!(protocol, StreamProtocol::Claude) {
                        match claude_initial_events(
                            &mut claude,
                            context.compaction_summary.as_deref(),
                            &context.resumed_server_events,
                            &accumulated_searches,
                            &accumulated_web_searches,
                        ) {
                            Ok(events) => events,
                            Err(error) => {
                                let message = web_search_replay_failure(
                                    &context.trace_id,
                                    &context.request_id,
                                    &context.user_agent,
                                    &error,
                                );
                                failed = Some(message.clone());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                    &protocol,
                                    &context.request_id,
                                    &message,
                                )));
                                break 'rounds;
                            }
                        }
                    } else {
                        Vec::new()
                    };
                    tracing::info!(
                        trace_id = %context.trace_id,
                        request_id = %context.request_id,
                        previous_account_id = %failed_account_id,
                        account_id = %account.id,
                        retry = pre_data_retries + 1,
                        "switching stream to another account"
                    );
                    payload.profile_arn.clone_from(&account.profile_arn);
                    let exponent = pre_data_retries.min(5);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        200u64.saturating_mul(1u64 << exponent).min(5_000),
                    ))
                    .await;
                    pre_data_retries += 1;
                    match context.state.generate(&account, &payload).await {
                        Ok(retry) => {
                            upstream_access_token = account.credentials.access_token.clone();
                            effective_thinking = retry.thinking_enabled();
                            let (next_endpoint, next_response, next_permit) = retry.into_parts();
                            endpoint = next_endpoint.name.to_string();
                            source = Some(next_response.bytes_stream());
                            upstream_metrics.reset();
                            stream_failure = None;
                            upstream_permit = Some(next_permit);
                            buffer.clear();
                            decoder = EventStreamDecoder;
                            decoded = DecodedResponse::default();
                            stop_filter = StopSequenceFilter::new(&context.stop_sequences);
                            failed = None;
                            continue 'rounds;
                        }
                        Err(error) => {
                            let message = error.to_string();
                            stream_failure = None;
                            record_account_scoped_stream_failure(
                                &context.state,
                                &context.trace_id,
                                &context.request_id,
                                &context.user_agent,
                                &account.id,
                                classify_stream_failure(&message, None),
                            )
                            .await;
                            failed = Some(message);
                        }
                    }
                }
                if let Some(message) = failed.as_deref() {
                    yield Ok::<Bytes, Infallible>(Bytes::from(classified_stream_error(
                        &protocol,
                        message,
                        &context.request_id,
                        stream_failure.as_ref(),
                    )));
                }
                break 'rounds;
            }
            pre_data_retries = 0;
            if !pending_initial.is_empty() {
                let output = std::mem::take(&mut pending_initial);
                if guards_tool_result_round {
                    guarded_output.extend(output);
                } else {
                    data_started = true;
                    for data in output {
                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                    }
                }
            }
            decoded.log_context_usage(&context.trace_id, context.input_tokens);
            fill_missing_usage(&context.state, &mut decoded, &payload).await;
            if stop_filter.matched().is_some() {
                for data in std::mem::take(&mut guarded_output) {
                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                }
                break 'rounds;
            }
            let output_exhausted = context.max_tokens.is_some_and(|maximum| {
                accumulated_usage
                    .output_tokens
                    .saturating_add(decoded.usage.output_tokens)
                    >= u64::from(maximum)
            });
            let empty_disposition = empty_tool_result_disposition(
                &payload,
                &decoded,
                output_exhausted,
                empty_tool_result_retries,
            );
            #[cfg(feature = "crs")]
            event_diagnostics.log_summary(
                Some(&decoded),
                match empty_disposition {
                    EmptyToolResultDisposition::Accept => "complete",
                    EmptyToolResultDisposition::Retry => "empty_retry",
                    EmptyToolResultDisposition::Fail => "empty_fail",
                },
            );
            match empty_disposition {
                EmptyToolResultDisposition::Accept => {
                    data_started |= !guarded_output.is_empty();
                    for data in std::mem::take(&mut guarded_output) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                    }
                }
                EmptyToolResultDisposition::Retry => {
                    if let Some(checkpoint) = claude_checkpoint.take() {
                        claude = checkpoint;
                    }
                    if let Some(checkpoint) = pending_initial_checkpoint.take() {
                        pending_initial = checkpoint;
                    }
                    empty_tool_result_retries += 1;
                    tracing::warn!(
                        trace_id = %context.trace_id,
                        request_id = %context.request_id,
                        user_agent = %context.user_agent,
                        account_id = %context.lease.account_id(),
                        endpoint,
                        retry = empty_tool_result_retries,
                        "upstream returned no assistant output; retrying once"
                    );
                    accumulate_usage(&mut accumulated_usage, &decoded.usage);
                    decoded = DecodedResponse::default();
                    let budget_available =
                        super::super::handlers::apply_remaining_output_budget(
                            &mut payload,
                            context.max_tokens,
                            accumulated_usage.output_tokens,
                        );
                    debug_assert!(budget_available);
                    #[cfg(feature = "crs")]
                    if matches!(protocol, StreamProtocol::Claude) {
                        super::super::crs::clarify_tool_execution_boundary(&mut payload);
                    }
                    match super::super::handlers::validate_internal_continuation(
                        &context.state,
                        &mut payload,
                        context.compact,
                        &endpoint,
                        "empty assistant retry",
                        context.tool_search.is_some(),
                    )
                    .await
                    {
                        Ok(tokens) => context.input_tokens = tokens,
                        Err(error) => {
                            let message = error.to_string();
                            failed = Some(message.clone());
                            yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                &protocol,
                                &context.request_id,
                                &message,
                            )));
                            break 'rounds;
                        }
                    }
                    let config = context.state.config.current();
                    let continuation_estimate = super::super::handlers::estimated_credits(
                        context.input_tokens,
                        payload
                            .max_output_tokens()
                            .or(context.max_tokens)
                            .unwrap_or(super::super::handlers::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                        &config.pool,
                    );
                    if let Err(error) = context.reservation.extend(continuation_estimate) {
                        let message = error.to_string();
                        failed = Some(message.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &message,
                        )));
                        break 'rounds;
                    }
                    let account = context.lease.account().await;
                    #[cfg(feature = "crs")]
                    super::super::crs::log_generation_request(
                        &payload,
                        &context.trace_id,
                        u32::from(empty_tool_result_retries),
                        "empty_assistant_retry",
                    );
                    match context.state.generate(&account, &payload).await {
                        Ok(next) => {
                            upstream_access_token = account.credentials.access_token.clone();
                            effective_thinking = next.thinking_enabled();
                            let (next_endpoint, next_response, next_permit) = next.into_parts();
                            endpoint = next_endpoint.name.to_string();
                            source = Some(next_response.bytes_stream());
                            upstream_metrics.reset();
                            stream_failure = None;
                            upstream_permit = Some(next_permit);
                            buffer.clear();
                            decoder = EventStreamDecoder;
                            stop_filter = StopSequenceFilter::new(&context.stop_sequences);
                            continue 'rounds;
                        }
                        Err(error) => {
                            let message = error.to_string();
                            stream_failure = None;
                            record_account_scoped_stream_failure(
                                &context.state,
                                &context.trace_id,
                                &context.request_id,
                                &context.user_agent,
                                &account.id,
                                classify_stream_failure(&message, None),
                            )
                            .await;
                            failed = Some(message.clone());
                            yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                &protocol,
                                &context.request_id,
                                &message,
                            )));
                            break 'rounds;
                        }
                    }
                }
                EmptyToolResultDisposition::Fail => {
                    if let Some(checkpoint) = claude_checkpoint.take() {
                        claude = checkpoint;
                    }
                    if let Some(checkpoint) = pending_initial_checkpoint.take() {
                        pending_initial = checkpoint;
                    }
                    let message =
                        "upstream returned no assistant output after a bounded retry";
                    tracing::error!(
                        trace_id = %context.trace_id,
                        request_id = %context.request_id,
                        user_agent = %context.user_agent,
                        account_id = %context.lease.account_id(),
                        endpoint,
                        "upstream repeated an empty assistant turn"
                    );
                    failed = Some(message.into());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        message,
                    )));
                    break 'rounds;
                }
            }
            let search_uses = context
                .tool_search
                .as_ref()
                .map(|catalog| {
                    decoded.take_tool_uses_where(|tool| catalog.is_search_tool(&tool.name))
                })
                .unwrap_or_default();
            if let Some(catalog) = context
                .tool_search
                .as_ref()
                .filter(|_| !search_uses.is_empty())
            {
                let max_tool_search_rounds = context
                    .state
                    .config
                    .current()
                    .features
                    .tool_search_max_rounds
                    .clamp(1, 8);
                let parallel_web_uses = if context.web_search_max_rounds > 0 {
                    decoded.take_tool_uses_where(|tool| tool.name == "web_search")
                } else {
                    Vec::new()
                };
                if search_round >= max_tool_search_rounds {
                    for search_use in &search_uses {
                        let mut trace = catalog.pending_trace(search_use);
                        trace.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.tool_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_searches.push(trace);
                    }
                    for search_use in &parallel_web_uses {
                        let trace = ClaudeWebSearchTrace::pending(
                            format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
                            search_use.input.clone(),
                        );
                        if matches!(protocol, StreamProtocol::Claude) {
                            match claude.web_search(&trace) {
                                Ok(events) => {
                                    for data in events {
                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                    }
                                }
                                Err(error) => {
                                    let message = web_search_replay_failure(
                                        &context.trace_id,
                                        &context.request_id,
                                        &context.user_agent,
                                        &error,
                                    );
                                    failed = Some(message.clone());
                                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                        &protocol,
                                        &context.request_id,
                                        &message,
                                    )));
                                    break 'rounds;
                                }
                            }
                        }
                        accumulated_web_searches.push(trace);
                    }
                    if decoded.tools.is_empty() {
                        decoded.stop_reason = Some(
                            if output_exhausted { "max_tokens" } else { "pause_turn" }.into(),
                        );
                    }
                    break 'rounds;
                }
                if !decoded.tools.is_empty() || output_exhausted {
                    for search_use in &search_uses {
                        let mut trace = catalog.pending_trace(search_use);
                        trace.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.tool_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_searches.push(trace);
                    }
                    for search_use in &parallel_web_uses {
                        let trace = ClaudeWebSearchTrace::pending(
                            format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
                            search_use.input.clone(),
                        );
                        if matches!(protocol, StreamProtocol::Claude) {
                            match claude.web_search(&trace) {
                                Ok(events) => {
                                    for data in events {
                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                    }
                                }
                                Err(error) => {
                                    let message = web_search_replay_failure(
                                        &context.trace_id,
                                        &context.request_id,
                                        &context.user_agent,
                                        &error,
                                    );
                                    failed = Some(message.clone());
                                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                        &protocol,
                                        &context.request_id,
                                        &message,
                                    )));
                                    break 'rounds;
                                }
                            }
                        }
                        accumulated_web_searches.push(trace);
                    }
                    if output_exhausted && decoded.tools.is_empty() {
                        decoded.stop_reason = Some("max_tokens".into());
                    }
                    break 'rounds;
                }
                let previous_search_count = accumulated_searches.len();
                let mut budget = match super::super::handlers::tool_search_budget_with_compaction(
                    &context.state,
                    &payload,
                    context.compact,
                )
                .await
                {
                    Ok(budget) => budget,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &error,
                        )));
                        break 'rounds;
                    }
                };
                let mut loaded_names = super::super::handlers::loaded_tool_names(&payload);
                let mut searches = Vec::with_capacity(search_uses.len());
                for search_use in search_uses {
                    let mut outcome = if tool_search_operations
                        >= context.max_tool_search_operations
                    {
                        catalog.unavailable_outcome(
                            &search_use,
                            format!(
                                "Tool Search operation limit of {} was reached",
                                context.max_tool_search_operations
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
                    outcome.trace.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                    let consumed_bytes = outcome.tools.iter()
                        .filter_map(|tool| serde_json::to_vec(tool).ok())
                        .map(|tool| tool.len()).sum::<usize>()
                        .saturating_add(outcome.documentation.iter().map(String::len).sum());
                    budget = ClaudeToolSearchBudget {
                        max_tools: budget.max_tools.saturating_sub(outcome.tools.len()),
                        max_bytes: budget.max_bytes.saturating_sub(consumed_bytes),
                    };
                    loaded_names.extend(outcome.tools.iter().filter_map(|tool| {
                        tool.specification().map(|tool| tool.name.clone())
                    }));
                    accumulated_searches.push(outcome.trace.clone());
                    searches.push((search_use, outcome));
                }
                let mut parallel_web_searches = Vec::with_capacity(parallel_web_uses.len());
                for search_use in parallel_web_uses {
                    let query = search_use.input.get("query").and_then(Value::as_str)
                        .unwrap_or_default().trim().to_owned();
                    let server_id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                    let trace = if web_search_round >= context.web_search_max_rounds {
                        ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            if context.web_search_client_limit { "max_uses_exceeded" } else { "unavailable" },
                            format!("web search is limited to {} uses", context.web_search_max_rounds),
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
                        match super::super::handlers::execute_filtered_kiro_web_search(
                            &context.state,
                            &context.lease,
                            &query,
                            &context.web_search_domains,
                        ).await {
                            Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                            Err(error) => {
                                let message = format!("web_search is not supported: {}", crosslane_translate::sanitize_error_message(&error.message));
                                failed = Some(message.clone());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &context.request_id, &message)));
                                break 'rounds;
                            },
                        }
                    };
                    accumulated_web_searches.push(trace.clone());
                    parallel_web_searches.push((search_use, trace));
                }
                let documentation_tokens = match context
                    .state
                    .tokenizer
                    .count(searches.iter().flat_map(|(_, outcome)| outcome.documentation.iter())
                        .cloned().collect::<Vec<_>>().join("\n\n"))
                    .await
                {
                    Ok(tokens) => tokens as u64,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &error,
                        )));
                        break 'rounds;
                    }
                };
                tracing::info!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    account_id = %context.lease.account_id(),
                    endpoint,
                    search_round = search_round + 1,
                    search_count = searches.len(),
                    matched_tools = searches.iter().map(|(_, outcome)| outcome.trace.references.len()).sum::<usize>(),
                    result_truncated = searches.iter().any(|(_, outcome)| outcome.trace.budget_truncated),
                    search_error = searches.iter().any(|(_, outcome)| outcome.trace.error.is_some()),
                    "proxy Tool Search batch executed"
                );
                let round_text = std::mem::take(&mut decoded.text);
                accumulated_text.push_str(&round_text);
                accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
                payload = tool_search_continue_payload_batch(&payload, &round_text, &searches);
                empty_tool_result_retries = 0;
                if !parallel_web_searches.is_empty() {
                    if let Some(assistant) = payload.conversation_state.history.last_mut()
                        .and_then(|message| message.assistant_response_message.as_mut())
                    {
                        assistant.tool_uses.extend(parallel_web_searches.iter()
                            .map(|(tool_use, _)| tool_use.clone()));
                    }
                    for (tool_use, trace) in &parallel_web_searches {
                        crosslane_translate::resume_web_search_payload(&mut payload, tool_use, trace);
                    }
                }
                accumulate_usage(&mut accumulated_usage, &decoded.usage);
                decoded = DecodedResponse::default();
                let budget_available = super::super::handlers::apply_remaining_output_budget(
                    &mut payload,
                    context.max_tokens,
                    accumulated_usage.output_tokens,
                );
                debug_assert!(budget_available);

                if let Err(error) = super::super::handlers::prepare_kiro_payload(
                    &mut payload,
                    &endpoint,
                    "Tool Search",
                ) {
                    let error = error.to_string();
                    failed = Some(error.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &error,
                    )));
                    break 'rounds;
                }

                let local_compaction = match super::super::handlers::compact_tool_search_continuation(
                    &context.state, &mut payload, context.compact,
                ).await {
                    Ok(stats) => stats,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol, &context.request_id, &error,
                        )));
                        break 'rounds;
                    }
                };
                if let Some(stats) = local_compaction {
                    tracing::info!(
                        trace_id = %context.trace_id,
                        request_id = %context.request_id,
                        compaction_reason = "tool_search_continuation_overflow",
                        compaction_mode = "extractive_fallback",
                        original_input_tokens = stats.original_tokens,
                        compacted_input_tokens = stats.compacted_tokens,
                        removed_messages = stats.removed_messages,
                        "Tool Search continuation history compacted locally"
                    );
                    context.compact = true;
                    context.auto_compaction_original_input_tokens.get_or_insert(stats.original_tokens as u64);
                    claude.auto_compaction_original_input_tokens = context.auto_compaction_original_input_tokens;
                    if let Some(summary) = stats.summary {
                        // Publish the checkpoint before the newly loaded tool
                        // references, preserving their client replay boundary.
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.compaction(&summary) {
                                data_started = true;
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                            // The boundary supersedes earlier tool references.
                            // Restore them with fresh paired server blocks so
                            // the next request can still load those tools.
                            for previous in &accumulated_searches[..previous_search_count] {
                                if previous.references.is_empty() {
                                    continue;
                                }
                                let mut replay = previous.clone();
                                replay.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                                replay.emission = crosslane_translate::ClaudeServerToolEmission::Complete;
                                for data in claude.tool_search(&replay) {
                                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                }
                            }
                        }
                        if let Some((scope, source)) = &context.compaction_replay {
                            context.state.compaction_replay.remember(scope.clone(), *source, summary.clone());
                        }
                        context.compaction_summary = Some(summary);
                    }
                }
                if matches!(protocol, StreamProtocol::Claude) {
                    for (_, outcome) in &searches {
                        for data in claude.tool_search(&outcome.trace) {
                            data_started = true;
                            yield Ok::<Bytes, Infallible>(Bytes::from(data));
                        }
                    }
                    for (_, trace) in &parallel_web_searches {
                        match claude.web_search(trace) {
                            Ok(events) => {
                                for data in events {
                                    data_started = true;
                                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                }
                            }
                            Err(error) => {
                                let message = web_search_replay_failure(
                                    &context.trace_id, &context.request_id, &context.user_agent, &error,
                                );
                                failed = Some(message.clone());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                    &protocol, &context.request_id, &message,
                                )));
                                break 'rounds;
                            }
                        }
                    }
                }

                let next_input_tokens = match context.state.tokenizer.estimate_kiro_payload(&payload).await {
                    Ok(tokens) => tokens as u64,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &error,
                        )));
                        break 'rounds;
                    }
                };
                let next_tool_tokens = match context.state.tokenizer.estimate_kiro_tools(&payload).await {
                    Ok(tokens) => (tokens as u64).saturating_add(documentation_tokens),
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &error,
                        )));
                        break 'rounds;
                    }
                };
                let next_payload_bytes = match serde_json::to_vec(&payload) {
                    Ok(payload) => payload.len(),
                    Err(error) => {
                        failed = Some(error.to_string());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &error.to_string(),
                        )));
                        break 'rounds;
                    }
                };
                let config = context.state.config.current();
                let max_loaded_tools = config
                    .context
                    .max_loaded_tools
                    .min(crosslane_translate::validate::MAX_TOOLS);
                let next_loaded_tools = payload.conversation_state.current_message
                    .user_input_message.user_input_message_context.as_ref()
                    .map_or(0, |context| context.tools.len());
                let budget_error = if next_loaded_tools > max_loaded_tools {
                    Some(format!(
                        "too many loaded tools after Tool Search: {next_loaded_tools} > {max_loaded_tools}"
                    ))
                } else if next_tool_tokens > u64::from(config.context.max_tool_input_tokens) {
                    Some(format!(
                        "loaded tool definitions are too large after Tool Search: {next_tool_tokens} estimated tokens > {}",
                        config.context.max_tool_input_tokens
                    ))
                } else if next_payload_bytes > config.context.max_upstream_payload_bytes {
                    Some(format!(
                        "translated upstream payload is too large after Tool Search: {next_payload_bytes} bytes > {}",
                        config.context.max_upstream_payload_bytes
                    ))
                } else {
                    None
                };
                if let Some(error) = budget_error {
                    failed = Some(error.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &error,
                    )));
                    break 'rounds;
                }
                let model = payload.conversation_state.current_message.user_input_message.model_id.clone();
                if let Err(limit) = super::super::handlers::check_context_limit(
                    &context.state,
                    next_input_tokens,
                    context.compact,
                    &model,
                ) {
                    let error = format!(
                        "prompt is too long after Tool Search: {} tokens > {}",
                        limit.input_tokens, limit.maximum
                    );
                    failed = Some(error.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &error,
                    )));
                    break 'rounds;
                }
                context.input_tokens = next_input_tokens;
                claude.input_tokens = next_input_tokens;
                let continuation_estimate = super::super::handlers::estimated_credits(
                    next_input_tokens,
                    payload.max_output_tokens().or(context.max_tokens).unwrap_or(super::super::handlers::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                    &config.pool,
                );
                if let Err(error) = context.reservation.extend(continuation_estimate) {
                    failed = Some(error.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &error.to_string(),
                    )));
                    break 'rounds;
                }
                let account = context.lease.account().await;
                match context.state.generate(&account, &payload).await {
                    Ok(next) => {
                        upstream_access_token = account.credentials.access_token.clone();
                        effective_thinking = next.thinking_enabled();
                        let (next_endpoint, next_response, next_permit) = next.into_parts();
                        endpoint = next_endpoint.name.to_string();
                        source = Some(next_response.bytes_stream());
                        upstream_metrics.reset();
                        stream_failure = None;
                        upstream_permit = Some(next_permit);
                        buffer.clear();
                        decoder = EventStreamDecoder;
                        search_round += 1;
                        continue 'rounds;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        stream_failure = None;
                        record_account_scoped_stream_failure(
                            &context.state,
                            &context.trace_id,
                            &context.request_id,
                            &context.user_agent,
                            &account.id,
                            classify_stream_failure(&message, None),
                        )
                        .await;
                        failed = Some(message.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &message,
                        )));
                        break 'rounds;
                    }
                }
            }
            let search_uses = if context.web_search_max_rounds > 0 {
                decoded.take_tool_uses_where(|tool| tool.name == "web_search")
            } else {
                Vec::new()
            };
            if !search_uses.is_empty() {
                if !decoded.tools.is_empty() || output_exhausted {
                    for search_use in &search_uses {
                        let trace = ClaudeWebSearchTrace::pending(
                            format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
                            search_use.input.clone(),
                        );
                        if matches!(protocol, StreamProtocol::Claude) {
                            match claude.web_search(&trace) {
                                Ok(events) => {
                                    for data in events {
                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                    }
                                }
                                Err(error) => {
                                    let message = web_search_replay_failure(
                                        &context.trace_id,
                                        &context.request_id,
                                        &context.user_agent,
                                        &error,
                                    );
                                    failed = Some(message.clone());
                                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                        &protocol,
                                        &context.request_id,
                                        &message,
                                    )));
                                    break 'rounds;
                                }
                            }
                        }
                        accumulated_web_searches.push(trace);
                    }
                    if output_exhausted && decoded.tools.is_empty() {
                        decoded.stop_reason = Some("max_tokens".into());
                    }
                    break 'rounds;
                }
                let mut searches = Vec::with_capacity(search_uses.len());
                for search_use in search_uses {
                    let query = search_use.input.get("query").and_then(Value::as_str)
                        .unwrap_or_default().trim().to_owned();
                    let server_id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                    let trace = if web_search_round >= context.web_search_max_rounds {
                        ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            if context.web_search_client_limit { "max_uses_exceeded" } else { "unavailable" },
                            if context.web_search_client_limit {
                                format!("web search is limited to max_uses={}", context.web_search_max_rounds)
                            } else {
                                format!("web search reached the proxy safety limit of {} uses", context.web_search_max_rounds)
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
                        match super::super::handlers::execute_filtered_kiro_web_search(
                            &context.state,
                            &context.lease,
                            &query,
                            &context.web_search_domains,
                        ).await {
                            Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                            Err(error) => {
                                let message = format!("web_search is not supported: {}", crosslane_translate::sanitize_error_message(&error.message));
                                failed = Some(message.clone());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &context.request_id, &message)));
                                break 'rounds;
                            },
                        }
                    };
                    if matches!(protocol, StreamProtocol::Claude) {
                        match claude.web_search(&trace) {
                            Ok(events) => {
                                for data in events {
                                    data_started = true;
                                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                }
                            }
                            Err(error) => {
                                let message = web_search_replay_failure(
                                    &context.trace_id,
                                    &context.request_id,
                                    &context.user_agent,
                                    &error,
                                );
                                failed = Some(message.clone());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                                    &protocol,
                                    &context.request_id,
                                    &message,
                                )));
                                break 'rounds;
                            }
                        }
                    }
                    accumulated_web_searches.push(trace.clone());
                    searches.push((search_use, trace));
                }
                tracing::info!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    account_id = %context.lease.account_id(),
                    endpoint,
                    web_search_round,
                    search_count = searches.len(),
                    result_count = searches.iter().map(|(_, trace)| trace.results.len()).sum::<usize>(),
                    search_error = searches.iter().any(|(_, trace)| trace.error.is_some()),
                    "proxy Kiro MCP web search batch executed"
                );
                let round_text = std::mem::take(&mut decoded.text);
                accumulated_text.push_str(&round_text);
                accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
                payload = web_search_continue_payload_batch(&payload, &round_text, &searches);
                empty_tool_result_retries = 0;
                accumulate_usage(&mut accumulated_usage, &decoded.usage);
                decoded = DecodedResponse::default();
                let budget_available = super::super::handlers::apply_remaining_output_budget(
                    &mut payload,
                    context.max_tokens,
                    accumulated_usage.output_tokens,
                );
                debug_assert!(budget_available);
                match super::super::handlers::validate_internal_continuation(
                    &context.state,
                    &mut payload,
                    context.compact,
                    &endpoint,
                    "Web Search",
                    context.tool_search.is_some(),
                )
                .await
                {
                    Ok(tokens) => context.input_tokens = tokens,
                    Err(error) => {
                        let error = error.to_string();
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &error,
                        )));
                        break 'rounds;
                    }
                }
                let config = context.state.config.current();
                let continuation_estimate = super::super::handlers::estimated_credits(
                    context.input_tokens,
                    payload.max_output_tokens().or(context.max_tokens).unwrap_or(super::super::handlers::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                    &config.pool,
                );
                if let Err(error) = context.reservation.extend(continuation_estimate) {
                    failed = Some(error.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &error.to_string(),
                    )));
                    break 'rounds;
                }
                let account = context.lease.account().await;
                match context.state.generate(&account, &payload).await {
                    Ok(next) => {
                        upstream_access_token = account.credentials.access_token.clone();
                        effective_thinking = next.thinking_enabled();
                        let (next_endpoint, next_response, next_permit) = next.into_parts();
                        endpoint = next_endpoint.name.to_string();
                        source = Some(next_response.bytes_stream());
                        upstream_metrics.reset();
                        stream_failure = None;
                        upstream_permit = Some(next_permit);
                        buffer.clear();
                        decoder = EventStreamDecoder;
                        continue 'rounds;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        stream_failure = None;
                        record_account_scoped_stream_failure(
                            &context.state,
                            &context.trace_id,
                            &context.request_id,
                            &context.user_agent,
                            &account.id,
                            classify_stream_failure(&message, None),
                        )
                        .await;
                        failed = Some(message.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                            &protocol,
                            &context.request_id,
                            &message,
                        )));
                        break 'rounds;
                    }
                }
            }
            let should_continue = !client_has_tools
                && !decoded.tools.is_empty()
                && !output_exhausted
                && auto_round < context.auto_continue_rounds;
            if !should_continue {
                if output_exhausted && decoded.tools.is_empty() {
                    decoded.stop_reason = Some("max_tokens".into());
                }
                break 'rounds;
            }
            tracing::info!(
                trace_id = %context.trace_id,
                request_id = %context.request_id,
                account_id = %context.lease.account_id(),
                endpoint,
                round = auto_round + 1,
                "starting automatic stream continuation"
            );
            let tool_uses = decoded.tools.values().map(|tool| KiroToolUse {
                tool_use_id: tool.id.clone(),
                name: tool.name.clone(),
                input: repair_json(&tool.input),
            }).collect::<Vec<_>>();
            let round_text = std::mem::take(&mut decoded.text);
            accumulated_text.push_str(&round_text);
            accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
            payload = auto_continue_payload(&payload, &round_text, tool_uses);
            empty_tool_result_retries = 0;
            decoded.tools.clear();
            accumulate_usage(&mut accumulated_usage, &decoded.usage);
            decoded.usage = crosslane_kiro::UsageInfo::default();
            let budget_available = super::super::handlers::apply_remaining_output_budget(
                &mut payload,
                context.max_tokens,
                accumulated_usage.output_tokens,
            );
            debug_assert!(budget_available);
            match super::super::handlers::validate_internal_continuation(
                &context.state,
                &mut payload,
                context.compact,
                &endpoint,
                "automatic continuation",
                context.tool_search.is_some(),
            )
            .await
            {
                Ok(tokens) => context.input_tokens = tokens,
                Err(error) => {
                    let error = error.to_string();
                    failed = Some(error.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &error,
                    )));
                    break 'rounds;
                }
            }
            let config = context.state.config.current();
            let continuation_estimate = super::super::handlers::estimated_credits(
                context.input_tokens,
                payload.max_output_tokens().or(context.max_tokens).unwrap_or(super::super::handlers::DEFAULT_OUTPUT_TOKEN_ESTIMATE),
                &config.pool,
            );
            if let Err(error) = context.reservation.extend(continuation_estimate) {
                failed = Some(error.to_string());
                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                    &protocol,
                    &context.request_id,
                    &error.to_string(),
                )));
                break 'rounds;
            }
            let account = context.lease.account().await;
            match context.state.generate(&account, &payload).await {
                Ok(next) => {
                    upstream_access_token = account.credentials.access_token.clone();
                    effective_thinking = next.thinking_enabled();
                    let (next_endpoint, next_response, next_permit) = next.into_parts();
                    endpoint = next_endpoint.name.to_string();
                    source = Some(next_response.bytes_stream());
                    upstream_metrics.reset();
                    stream_failure = None;
                    upstream_permit = Some(next_permit);
                    buffer.clear();
                    decoder = EventStreamDecoder;
                    auto_round += 1;
                }
                Err(error) => {
                    let message = error.to_string();
                    stream_failure = None;
                    record_account_scoped_stream_failure(
                        &context.state,
                        &context.trace_id,
                        &context.request_id,
                        &context.user_agent,
                        &account.id,
                        classify_stream_failure(&message, None),
                    )
                    .await;
                    failed = Some(message.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &message,
                    )));
                    break 'rounds;
                }
            }
        }
        let trailing = stop_filter.finish();
        if failed.is_none() && !trailing.is_empty() {
            let output = stream_event(
                &protocol,
                &mut claude,
                &KiroEvent::AssistantResponse { content: trailing },
                created,
                &context.model,
                context.thinking_output_format,
                &context.openai_tools,
            );
            for data in prepend_pending_initial(&mut pending_initial, output) {
                yield Ok::<Bytes, Infallible>(Bytes::from(data));
            }
        }
        accumulated_text.push_str(&decoded.text);
        accumulated_reasoning.push_str(&decoded.reasoning);
        decoded.text = accumulated_text;
        decoded.reasoning = accumulated_reasoning;
        decoded.tool_searches = accumulated_searches;
        decoded.web_searches = accumulated_web_searches;
        accumulate_usage(&mut decoded.usage, &accumulated_usage);
        apply_stream_stop(&mut decoded, &stop_filter);
        let current_round_output_tokens = decoded.usage.output_tokens;
        context.state.prompt_cache.commit(
            &context.lease.account_id(),
            &prompt_cache_plan,
            &mut decoded.usage,
        );
        context.prompt_cache = None;
        if failed.is_none()
            && matches!(protocol, StreamProtocol::Claude)
            && !decoded.text.is_empty()
        {
            match claude.citations(&decoded.web_searches, &decoded.text) {
                Ok(events) => {
                    for data in events {
                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                    }
                }
                Err(error) => {
                    let message = web_search_replay_failure(
                        &context.trace_id,
                        &context.request_id,
                        &context.user_agent,
                        &error,
                    );
                    failed = Some(message.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                        &protocol,
                        &context.request_id,
                        &message,
                    )));
                }
            }
        }
        // Calls used for the legacy no-tools continuation stay upstream-only.
        // If that continuation ends with a tool call, never expose an undeclared
        // name (including a hash alias) to the client.
        if failed.is_none() && !client_has_tools {
            if let Err(message) = tool_names.restore_decoded(&mut decoded, &ToolNameTrace {
                trace_id: &context.trace_id,
                request_id: &context.request_id,
                path: &context.path,
                streaming: true,
            }) {
                failed = Some(message.to_owned());
                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(
                    &protocol,
                    &context.request_id,
                    message,
                )));
            }
        }
        if failed.is_none() {
            // Custom tools are buffered individually so their JSON wrapper
            // can be removed. Flush them even when ordinary function calls
            // stream immediately, using the same predicate as ingestion.
            if buffer_tool_calls
                || context.openai_tools.values().any(|identity| identity.kind == "custom")
            {
                for tool in decoded.tools.values() {
                    let event = KiroEvent::ToolUse {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        input_delta: tool.input.clone(),
                        stop: true,
                    };
                    if !should_buffer_tool_event(&event, buffer_tool_calls, &context.openai_tools) {
                        continue;
                    }
                    for data in stream_event(
                        &protocol,
                        &mut claude,
                        &event,
                        created,
                        &context.model,
                        context.thinking_output_format,
                        &context.openai_tools,
                    ) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                    }
                }
            }
            for data in stream_finish(
                &protocol,
                &mut claude,
                &decoded,
                created,
                &context.model,
                context.max_tokens,
                current_round_output_tokens,
                context.thinking_output_format,
                context.include_usage_chunk,
            ) {
                yield Ok::<Bytes, Infallible>(Bytes::from(data));
            }
        }
        drop(upstream_permit);
        finish_accounting(
            context,
            endpoint,
            decoded,
            failed,
            stream_failure,
            &payload,
        )
        .await;
    };
    // A bounded bridge supplies backpressure and ensures a client that stops
    // reading cannot hold an account lease forever.
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    #[cfg(feature = "crs")]
    let stats_attempt = crate::stats::RequestStatsAttempt::current();
    let bridge = async move {
        futures::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            match tokio::time::timeout(std::time::Duration::from_secs(30), sender.send(item)).await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    tracing::warn!(
                        trace_id = %bridge_trace_id,
                        request_id = %bridge_request_id,
                        user_agent = %bridge_user_agent,
                        "client disconnected before stream completed"
                    );
                    break;
                }
                Err(_) => {
                    tracing::warn!(
                        trace_id = %bridge_trace_id,
                        request_id = %bridge_request_id,
                        user_agent = %bridge_user_agent,
                        "client stream write timed out"
                    );
                    break;
                }
            }
        }
    };
    #[cfg(feature = "crs")]
    let bridge = crate::stats::RequestStatsAttempt::scope(stats_attempt, bridge);
    tokio::spawn(bridge);
    let receiver = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let mut response = Response::new(Body::from_stream(receiver));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}

#[cfg(test)]
mod tests {
    use super::retry_mapped_model;
    use crosslane_core::config::{Config, ModelMappingRule};

    fn cross_provider_config() -> Config {
        Config {
            model_mapping: vec![ModelMappingRule {
                name: "mixed".into(),
                source_models: vec!["team".into()],
                target_models: vec!["copilot/gpt-test".into()],
                providers: vec!["kiro".into()],
                ..ModelMappingRule::default()
            }],
            ..Config::default()
        }
    }

    #[test]
    fn locked_stream_retry_keeps_the_selected_kiro_target() {
        let config = cross_provider_config();
        assert_eq!(
            retry_mapped_model(
                true,
                "claude-sonnet-test",
                "team",
                None,
                &config,
                "svc",
                None,
                None,
            ),
            "claude-sonnet-test"
        );
    }

    #[test]
    fn unlocked_stream_retry_reapplies_account_aware_mapping() {
        let config = cross_provider_config();
        assert_eq!(
            retry_mapped_model(false, "team", "team", None, &config, "svc", None, None,),
            "copilot/gpt-test"
        );
    }

    #[test]
    fn stream_retry_fallback_takes_precedence_over_a_locked_route() {
        let config = cross_provider_config();
        assert_eq!(
            retry_mapped_model(
                true,
                "claude-sonnet-test",
                "team",
                Some("claude-haiku-test"),
                &config,
                "svc",
                None,
                None,
            ),
            "claude-haiku-test"
        );
    }
}
