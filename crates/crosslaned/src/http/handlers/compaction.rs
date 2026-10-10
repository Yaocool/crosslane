use super::{
    compact_target_from_maximum, compact_target_tokens, compaction_summary_payload,
    context_maximum, credits, estimated_credits, execute_upstream, fill_missing_usage,
    prepare_kiro_payload, request_log, sanitize_error_message, upstream_error, usage_record,
    ApiError, AppState, Arc, BytesMut, CancellationToken, CompactionArtifact, CompactionDecision,
    CompactionIterationUsage, CompactionReason, CompactionRequest, CompactionRun,
    CompactionSummaryFailure, ContextLimitError, DecodedResponse, Duration, ErrorFormat,
    EventStreamDecoder, ExecuteError, GeneratedCompactionSummary, Instant, KiroEvent, KiroPayload,
    RequestDiagnostics, StatusCode, StreamExt, UpstreamExecution, Uuid, COMPACTION_CLEANUP_GRACE,
    COMPACTION_USAGE_PATH, MAX_COMPACTION_BACKGROUND_GRACE, MIN_COMPACTION_BACKGROUND_GRACE,
};
use crate::http::usage::produced_output;
use tokio_util::codec::Decoder;

/// Local, transactional recovery for a request whose history exceeds either
/// budget. Continuations already hold an account lease, so they must not open
/// a second upstream request to generate a summary.
pub(in crate::http) async fn compact_history_for_limits(
    state: &Arc<AppState>,
    payload: &mut KiroPayload,
    maximum_tokens: u64,
    maximum_bytes: usize,
) -> Result<Option<crosslane_translate::ContextCompactionStats>, String> {
    let original_tokens = state.tokenizer.estimate_kiro_payload(payload).await? as u64;
    let original_bytes = super::serialized_payload_bytes(payload, ErrorFormat::Claude)
        .map_err(|error| error.message)?;
    if original_tokens <= maximum_tokens && original_bytes <= maximum_bytes {
        return Ok(None);
    }
    let mut minimum = payload.clone();
    minimum.retain_required_history();
    if state.tokenizer.estimate_kiro_payload(&minimum).await? as u64 > maximum_tokens
        || super::serialized_payload_bytes(&minimum, ErrorFormat::Claude)
            .map_err(|error| error.message)?
            > maximum_bytes
    {
        return Ok(None);
    }
    let preserve = state
        .config
        .current()
        .context
        .compaction_preserve_recent_turns;
    // The 75% target supplies headroom. If the indivisible current turn only
    // fits the hard budget, retry from the original rather than rejecting it.
    for target in [compact_target_from_maximum(maximum_tokens), maximum_tokens] {
        let mut candidate = payload.clone();
        let result = state
            .tokenizer
            .compact_kiro_payload_with_limits(
                &mut candidate,
                usize::try_from(target).unwrap_or(usize::MAX),
                maximum_bytes,
                preserve,
            )
            .await;
        let mut stats = match result {
            Ok(stats) if stats.removed_messages > 0 => stats,
            Ok(_) => return Ok(None),
            Err(error) if error.contains("local compaction did not reach target") => continue,
            Err(error) => return Err(error),
        };
        prepare_kiro_payload(&mut candidate, "compaction", "local budget recovery")
            .map_err(|error| error.message)?;
        stats.compacted_tokens = state.tokenizer.estimate_kiro_payload(&candidate).await?;
        if stats.compacted_tokens as u64 <= maximum_tokens
            && super::serialized_payload_bytes(&candidate, ErrorFormat::Claude)
                .map_err(|error| error.message)?
                <= maximum_bytes
        {
            *payload = candidate;
            return Ok(Some(stats));
        }
    }
    // Keep the original intact so the existing guards report the real limit.
    Ok(None)
}

pub(in crate::http) async fn tool_search_budget_with_compaction(
    state: &Arc<AppState>,
    payload: &KiroPayload,
    compact: bool,
) -> Result<crosslane_translate::ClaudeToolSearchBudget, String> {
    let config = state.config.current();
    let available = super::remaining_tool_search_budget(state, payload, compact).await?;
    if !config.context.auto_compact_on_overflow
        || available.max_tools == 0
        || payload.conversation_state.history.len()
            <= payload.protected_history_len() + payload.required_history_tail_len()
    {
        return Ok(available);
    }
    let model = &payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id;
    let maximum = context_maximum(state, compact, model);
    let reserve_tokens = u64::from(config.context.max_tool_input_tokens).min(maximum / 4);
    let reserve_bytes = (reserve_tokens.saturating_mul(3) as usize)
        .min(config.context.max_upstream_payload_bytes / 4);
    if available.max_bytes >= reserve_bytes {
        return Ok(available);
    }
    // This is only a preview. Compact the actual continuation after adding the
    // generated text and search results, so its checkpoint covers those too.
    let mut preview = payload.clone();
    compact_history_for_limits(
        state,
        &mut preview,
        maximum.saturating_sub(reserve_tokens),
        config
            .context
            .max_upstream_payload_bytes
            .saturating_sub(reserve_bytes),
    )
    .await?;
    super::remaining_tool_search_budget(state, &preview, compact).await
}

pub(in crate::http) async fn compact_tool_search_continuation(
    state: &Arc<AppState>,
    payload: &mut KiroPayload,
    compact: bool,
) -> Result<Option<crosslane_translate::ContextCompactionStats>, String> {
    let config = state.config.current();
    if !config.context.auto_compact_on_overflow {
        return Ok(None);
    }
    let model = &payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id;
    let maximum = context_maximum(state, compact, model);
    compact_history_for_limits(
        state,
        payload,
        maximum,
        config.context.max_upstream_payload_bytes,
    )
    .await
}

fn compaction_summary_concurrency(state: &AppState) -> usize {
    state
        .config
        .current()
        .pool
        .max_concurrent_per_account
        .clamp(1, 2)
}

#[allow(clippy::too_many_arguments)]
async fn generate_compaction_summary(
    state: &Arc<AppState>,
    account_ids: Arc<std::collections::HashSet<String>>,
    trace_id: &str,
    client_context: &crosslane_core::client_context::ClientContext,
    key_id: Option<&str>,
    allowed_models: &[String],
    service_id: Option<&str>,
    summary_model: &str,
    payloads: Vec<KiroPayload>,
    timeout_ms: u64,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    let owned_state = Arc::clone(state);
    let owned_account_ids = account_ids;
    let owned_trace_id = trace_id.to_owned();
    let owned_client_context = client_context.clone();
    let owned_key_id = key_id.map(str::to_owned);
    let owned_allowed_models = allowed_models.to_vec();
    let owned_service_id = service_id.map(str::to_owned);
    let owned_summary_model = summary_model.to_owned();
    let concurrency = compaction_summary_concurrency(state);
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let completed_usage = Arc::new(std::sync::Mutex::new(None));
    let progress = Arc::clone(&completed_usage);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let task = tokio::spawn(async move {
        let count = payloads.len();
        let failed = std::sync::atomic::AtomicBool::new(false);
        let mut summaries = vec![String::new(); count];
        let mut failure = None;
        let mut usage = None;
        // A single operation deadline covers all parts. At most two accepted
        // requests run at once; failures stop dispatching new parts while
        // already accepted streams still settle their own reservations/stats.
        let mut requests = futures::stream::iter(payloads.into_iter().enumerate())
            .map(|(index, payload)| {
                let state = &owned_state;
                let account_ids = &owned_account_ids;
                let trace_id = &owned_trace_id;
                let client_context = &owned_client_context;
                let key_id = owned_key_id.as_deref();
                let allowed_models = owned_allowed_models.as_slice();
                let service_id = owned_service_id.as_deref();
                let model = &owned_summary_model;
                let cancel = task_cancel.clone();
                let failed = &failed;
                async move {
                    let result = if Instant::now() >= deadline || cancel.is_cancelled() {
                        Err("Kiro compaction summary timed out before the next part"
                            .to_owned()
                            .into())
                    } else if failed.load(std::sync::atomic::Ordering::Relaxed) {
                        Err("Kiro compaction summary stopped after a failed part"
                            .to_owned()
                            .into())
                    } else {
                        generate_compaction_summary_inner(
                            state,
                            account_ids,
                            trace_id,
                            client_context,
                            key_id,
                            allowed_models,
                            service_id,
                            model,
                            payload,
                            cancel,
                        )
                        .await
                    };
                    if result.is_err() {
                        failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    (index, result)
                }
            })
            .buffer_unordered(concurrency);
        while let Some((index, result)) = requests.next().await {
            let part_usage = match result {
                Ok(summary) => {
                    summaries[index] = summary.content;
                    Some(summary.usage)
                }
                Err(error) => {
                    let part_usage = error.usage;
                    if failure.is_none() {
                        failure = Some(error.message);
                    }
                    part_usage
                }
            };
            if let Some(part) = part_usage {
                let total = usage.get_or_insert(CompactionIterationUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                });
                total.input_tokens = total.input_tokens.saturating_add(part.input_tokens);
                total.output_tokens = total.output_tokens.saturating_add(part.output_tokens);
                *progress.lock().unwrap_or_else(|error| error.into_inner()) = usage;
            }
        }
        if let Some(message) = failure {
            return Err(CompactionSummaryFailure { message, usage });
        }
        let content = if count == 1 {
            summaries.remove(0)
        } else {
            summaries
                .into_iter()
                .enumerate()
                .map(|(index, summary)| {
                    format!(
                        "Checkpoint part {} of {count} (chronological):\n{summary}",
                        index + 1
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        Ok(GeneratedCompactionSummary {
            content,
            usage: usage.ok_or_else(|| {
                CompactionSummaryFailure::from("No compaction summary parts completed".to_owned())
            })?,
        })
    });
    let mut result = await_compaction_summary_task_with_policy(
        trace_id,
        task,
        timeout_ms,
        cancel,
        compaction_background_grace(timeout_ms),
        COMPACTION_CLEANUP_GRACE,
    )
    .await;
    if let Err(error) = &mut result {
        if error.usage.is_none() {
            error.usage = *completed_usage
                .lock()
                .unwrap_or_else(|error| error.into_inner());
        }
    }
    result
}

#[cfg(test)]
pub(super) async fn await_compaction_summary_task(
    task: tokio::task::JoinHandle<Result<GeneratedCompactionSummary, CompactionSummaryFailure>>,
    timeout_ms: u64,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    await_compaction_summary_task_with_policy(
        "trace_compaction_test",
        task,
        timeout_ms,
        CancellationToken::new(),
        compaction_background_grace(timeout_ms),
        COMPACTION_CLEANUP_GRACE,
    )
    .await
}

fn compaction_background_grace(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms)
        .max(MIN_COMPACTION_BACKGROUND_GRACE)
        .min(MAX_COMPACTION_BACKGROUND_GRACE)
}

pub(super) async fn await_compaction_summary_task_with_policy(
    trace_id: &str,
    mut task: tokio::task::JoinHandle<Result<GeneratedCompactionSummary, CompactionSummaryFailure>>,
    timeout_ms: u64,
    cancel: CancellationToken,
    background_grace: Duration,
    cleanup_grace: Duration,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    match tokio::time::timeout(Duration::from_millis(timeout_ms), &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(CompactionSummaryFailure {
            message: format!("Kiro compaction summary task failed: {error}"),
            usage: None,
        }),
        Err(_) => {
            let trace_id = trace_id.to_owned();
            tokio::spawn(async move {
                match tokio::time::timeout(background_grace, &mut task).await {
                    Ok(result) => log_late_compaction_result(&trace_id, result),
                    Err(_) => {
                        cancel.cancel();
                        match tokio::time::timeout(cleanup_grace, &mut task).await {
                            Ok(result) => log_late_compaction_result(&trace_id, result),
                            Err(_) => {
                                task.abort();
                                let _ = task.await;
                                tracing::warn!(
                                    trace_id,
                                    background_grace_ms = background_grace.as_millis() as u64,
                                    cleanup_grace_ms = cleanup_grace.as_millis() as u64,
                                    "timed-out compaction summary exceeded its bounded accounting grace and was aborted"
                                );
                            }
                        }
                    }
                }
            });
            Err(CompactionSummaryFailure {
                message: format!("Kiro compaction summary timed out after {timeout_ms} ms"),
                usage: None,
            })
        }
    }
}

fn log_late_compaction_result(
    trace_id: &str,
    result: Result<
        Result<GeneratedCompactionSummary, CompactionSummaryFailure>,
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok(Ok(summary)) => tracing::debug!(
            trace_id,
            input_tokens = summary.usage.input_tokens,
            output_tokens = summary.usage.output_tokens,
            "timed-out compaction summary completed and was accounted"
        ),
        Ok(Err(error)) => tracing::debug!(
            trace_id,
            reason = %sanitize_error_message(&error.message),
            "timed-out compaction summary finished with an error"
        ),
        Err(error) => tracing::warn!(
            trace_id,
            %error,
            "timed-out compaction summary task could not be joined"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn generate_compaction_summary_inner(
    state: &Arc<AppState>,
    account_ids: &std::collections::HashSet<String>,
    trace_id: &str,
    client_context: &crosslane_core::client_context::ClientContext,
    key_id: Option<&str>,
    allowed_models: &[String],
    service_id: Option<&str>,
    summary_model: &str,
    payload: crosslane_translate::KiroPayload,
    cancel: CancellationToken,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    let started = Instant::now();
    let input_tokens = state.tokenizer.estimate_kiro_payload(&payload).await? as u64;
    let max_output_tokens = payload.max_output_tokens().unwrap_or(1);
    let estimate = estimated_credits(
        input_tokens,
        max_output_tokens,
        &state.config.current().pool,
    );
    let reservation = state
        .meter
        .reserve(key_id, estimate)
        .map_err(|error| error.to_string())?;
    let default_model = state.config.current().features.default_model_id.clone();
    let execution = tokio::select! {
        result = execute_upstream(
            state,
            account_ids,
            trace_id,
            summary_model,
            summary_model,
            key_id,
            allowed_models,
            service_id,
            &default_model,
            None,
            estimate,
            input_tokens,
            true,
            &payload,
            None,
            true,
        ) => result.map_err(execute_error_message)?,
        _ = cancel.cancelled() => {
            return Err(CompactionSummaryFailure {
                message: "Kiro compaction summary canceled after its accounting grace expired".into(),
                usage: None,
            });
        }
    };
    let UpstreamExecution {
        mut lease,
        response,
        upstream_access_token: _,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execution;
    let account_id = lease.account_id();
    let account_name = lease.account().await.display_name().to_owned();
    let (endpoint_definition, response, upstream_permit) = response.into_parts();
    let endpoint = endpoint_definition.name.to_string();
    let mut source = response.bytes_stream();
    let mut buffer = BytesMut::new();
    let mut decoder = EventStreamDecoder;
    let mut decoded = DecodedResponse::default();
    let mut collection_error = None;
    'collect: loop {
        let chunk = tokio::select! {
            chunk = source.next() => chunk,
            _ = cancel.cancelled() => {
                collection_error = Some(
                    "Kiro compaction summary canceled after its accounting grace expired".into(),
                );
                break 'collect;
            }
        };
        match chunk {
            Some(Ok(chunk)) => {
                buffer.extend_from_slice(&chunk);
                loop {
                    match decoder.decode(&mut buffer) {
                        Ok(Some(KiroEvent::Error { kind, message })) => {
                            collection_error = Some(format!("{kind}: {message}"));
                            break 'collect;
                        }
                        Ok(Some(event)) => {
                            if let Err(error) = decoded.push(event) {
                                collection_error = Some(error);
                                break 'collect;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            collection_error = Some(error.to_string());
                            break 'collect;
                        }
                    }
                }
            }
            Some(Err(error)) => {
                collection_error = Some(error.to_string());
                break 'collect;
            }
            None => break,
        }
    }
    if collection_error.is_none() {
        loop {
            match decoder.decode_eof(&mut buffer) {
                Ok(Some(KiroEvent::Error { kind, message })) => {
                    collection_error = Some(format!("{kind}: {message}"));
                    break;
                }
                Ok(Some(event)) => {
                    if let Err(error) = decoded.push(event) {
                        collection_error = Some(error);
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    collection_error = Some(error.to_string());
                    break;
                }
            }
        }
    }
    drop(source);
    drop(upstream_permit);
    let received_output = produced_output(&decoded);
    fill_missing_usage(state, &mut decoded, &payload).await;
    let parsed_summary = if let Some(error) = collection_error.clone() {
        Err(error)
    } else if decoded.tools.is_empty() {
        parse_compaction_summary(&decoded.text)
    } else {
        Err("Kiro compaction summary unexpectedly returned a tool call".into())
    };
    // Match main-stream accounting: a failed summary with no output/usage must
    // not spend an input-only fallback estimate as if it were a reported cost.
    let credits = if parsed_summary.is_err() && !received_output {
        0.0
    } else {
        credits(state, &kiro_model, &decoded)
    };
    let credits_source = if decoded.usage.credits > 0.0 {
        "server"
    } else {
        "estimated"
    };
    lease.settle_credits(credits).await;
    let settlement_error = reservation
        .settle(usage_record(
            &mapped_model,
            summary_model,
            &kiro_model,
            COMPACTION_USAGE_PATH,
            &decoded,
            credits,
        ))
        .await
        .err()
        .map(|error| error.to_string());
    let mut log = request_log(
        trace_id,
        &format!("cmp_{}", Uuid::new_v4().simple()),
        COMPACTION_USAGE_PATH,
        &mapped_model,
        summary_model,
        &kiro_model,
        &account_id,
        &account_name,
        &endpoint,
        &model_path,
        model_mapping_rule.as_deref(),
        attempts,
        started,
        RequestDiagnostics {
            client_context: Some(client_context.clone()),
            payload_bytes: serde_json::to_vec(&payload).map_or(0, |value| value.len()),
            ..RequestDiagnostics::default()
        },
        &decoded,
        credits,
    );
    if let Err(error) = &parsed_summary {
        log.status = if cancel.is_cancelled() { 504 } else { 502 };
        log.error = Some(sanitize_error_message(error));
        log.diagnostics.client_status = log.status;
        log.diagnostics.error_code = if cancel.is_cancelled() {
            "compaction_timeout"
        } else if collection_error.is_some() {
            "compaction_stream_error"
        } else {
            "compaction_invalid_summary"
        }
        .into();
        log.diagnostics.error_stage = if collection_error.is_some() {
            "upstream_stream"
        } else {
            "response_validation"
        }
        .into();
    }
    state.stats.record(log);
    let usage = CompactionIterationUsage {
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
    };
    if let Some(error) = settlement_error {
        tracing::error!(
            trace_id,
            account_id,
            summary_model,
            reason = %sanitize_error_message(&error),
            "Kiro compaction summary usage settlement failed"
        );
        return Err(CompactionSummaryFailure {
            message: error,
            usage: Some(usage),
        });
    }
    if let Err(error) = &parsed_summary {
        // run_compaction already emitted the fallback warning (or will do so
        // when this returns). Accounting is the same event's informational tail.
        tracing::info!(
            trace_id,
            account_id,
            account_name,
            summary_model,
            mapped_model,
            kiro_model,
            endpoint,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            credits_source,
            canceled = cancel.is_cancelled(),
            duration_ms = started.elapsed().as_millis() as u64,
            reason = %sanitize_error_message(error),
            "Kiro semantic compaction summary failed after partial usage was accounted"
        );
    } else {
        tracing::info!(
            trace_id,
            account_id,
            account_name,
            summary_model,
            mapped_model,
            kiro_model,
            endpoint,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            credits_source,
            duration_ms = started.elapsed().as_millis() as u64,
            "Kiro semantic compaction summary completed"
        );
    }
    match parsed_summary {
        Ok(content) => Ok(GeneratedCompactionSummary { content, usage }),
        Err(message) => Err(CompactionSummaryFailure {
            message,
            usage: Some(usage),
        }),
    }
}

pub(super) fn parse_compaction_summary(output: &str) -> Result<String, String> {
    let output = output.trim();
    if output.is_empty() {
        return Err("Kiro returned an empty compaction summary".into());
    }
    if let Some(open) = output.find("<summary>") {
        let content_start = open + "<summary>".len();
        let close = output[content_start..]
            .find("</summary>")
            .map(|offset| content_start + offset)
            .ok_or_else(|| "Kiro returned an unterminated <summary> block".to_owned())?;
        let summary = output[content_start..close].trim();
        if summary.is_empty() {
            return Err("Kiro returned an empty <summary> block".into());
        }
        return Ok(summary.to_owned());
    }
    let output = output
        .strip_prefix("```markdown")
        .or_else(|| output.strip_prefix("```"))
        .unwrap_or(output);
    let output = output.strip_suffix("```").unwrap_or(output).trim();
    if output.is_empty() {
        Err("Kiro returned an empty compaction summary".into())
    } else {
        Ok(output.to_owned())
    }
}

pub(super) fn initial_compaction_decision(
    state: &Arc<AppState>,
    model: &str,
    input_tokens: u64,
    client_trigger: Option<u64>,
    auto_compact_on_overflow: bool,
) -> Option<CompactionDecision> {
    let mapped_maximum = context_maximum(state, false, model);
    let mut reasons = Vec::new();
    let mut triggers = Vec::new();
    if let Some(trigger) = client_trigger
        .map(|trigger| trigger.min(mapped_maximum))
        .filter(|trigger| input_tokens >= *trigger)
    {
        reasons.push(CompactionReason::ClientTrigger);
        triggers.push(trigger);
    }
    if auto_compact_on_overflow && input_tokens > mapped_maximum {
        reasons.push(CompactionReason::MappedWindowOverflow);
        triggers.push(mapped_maximum);
    }
    let trigger_tokens = triggers.into_iter().min()?;
    Some(CompactionDecision {
        reasons,
        model: model.to_owned(),
        trigger_tokens,
        target_tokens: compact_target_tokens(state, model, trigger_tokens),
        maximum_tokens: context_maximum(state, true, model),
    })
}

pub(super) fn resolved_compaction_decision(limit: &ContextLimitError) -> CompactionDecision {
    CompactionDecision {
        reasons: vec![CompactionReason::ResolvedWindowOverflow],
        model: limit.model.clone(),
        trigger_tokens: limit.maximum,
        target_tokens: compact_target_from_maximum(limit.maximum),
        maximum_tokens: limit.maximum,
    }
}

pub(super) fn upstream_overflow_compaction_decision(
    state: &Arc<AppState>,
    model: &str,
    input_tokens: u64,
) -> Option<CompactionDecision> {
    if input_tokens <= 1 {
        return None;
    }
    let context = &state.config.current().context;
    // An upstream overflow means the dynamic model metadata is not a safe
    // authority for this request. Fall back to the operator-controlled default
    // window and force meaningful progress even when the request is already
    // below that default.
    let configured_maximum =
        (f64::from(context.max_input_tokens) * context.safe_input_ratio) as u64;
    let maximum_tokens = configured_maximum
        .max(1)
        .min(input_tokens.saturating_sub(1));
    Some(CompactionDecision {
        reasons: vec![CompactionReason::UpstreamWindowOverflow],
        model: model.to_owned(),
        trigger_tokens: maximum_tokens,
        target_tokens: compact_target_from_maximum(maximum_tokens),
        maximum_tokens,
    })
}

fn conservative_summary_context_maximum(state: &Arc<AppState>, model: &str) -> u64 {
    let context = &state.config.current().context;
    let configured =
        (f64::from(context.max_input_tokens) * context.compact_safe_input_ratio) as u64;
    configured.max(1).min(context_maximum(state, true, model))
}

async fn extractive_compaction(
    state: &Arc<AppState>,
    source_payload: &KiroPayload,
    target_tokens: u64,
    preserve_recent_turns: usize,
    context_limit: Option<&CompactionDecision>,
) -> Result<(KiroPayload, crosslane_translate::ContextCompactionStats), ApiError> {
    let mut payload = source_payload.clone();
    let compacted = state
        .tokenizer
        .compact_kiro_payload_with_limits(
            &mut payload,
            target_tokens as usize,
            state.config.current().context.max_upstream_payload_bytes,
            preserve_recent_turns,
        )
        .await;
    let mut stats = match compacted {
        Ok(stats) => stats,
        Err(error)
            if error.contains("local compaction did not reach target")
                && context_limit.is_some() =>
        {
            let decision = context_limit.expect("checked above");
            super::enforce_payload_budget(
                state,
                0,
                super::serialized_payload_bytes(&payload, ErrorFormat::Claude)?,
                0,
                false,
                ErrorFormat::Claude,
            )?;
            let actual = state
                .tokenizer
                .estimate_kiro_payload(&payload)
                .await
                .unwrap_or(decision.maximum_tokens as usize + 1) as u64;
            return Err(upstream_error(
                ExecuteError::ContextLimit(ContextLimitError {
                    model: decision.model.clone(),
                    input_tokens: actual.max(decision.maximum_tokens.saturating_add(1)),
                    maximum: decision.maximum_tokens,
                }),
                ErrorFormat::Claude,
            ));
        }
        Err(error) => {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            ));
        }
    };
    finalize_compaction_payload(state, &mut payload, &mut stats, "extractive compaction").await?;
    Ok((payload, stats))
}

async fn finalize_compaction_payload(
    state: &Arc<AppState>,
    payload: &mut KiroPayload,
    stats: &mut crosslane_translate::ContextCompactionStats,
    stage: &str,
) -> Result<(), ApiError> {
    prepare_kiro_payload(payload, "compaction", stage).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error.message,
            ErrorFormat::Claude,
        )
    })?;
    stats.compacted_tokens = state
        .tokenizer
        .estimate_kiro_payload(payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    Ok(())
}

pub(super) async fn compaction_operation_target(
    state: &Arc<AppState>,
    source_payload: &KiroPayload,
    decision: &CompactionDecision,
) -> Result<u64, ApiError> {
    let mut minimum_payload = source_payload.clone();
    minimum_payload.retain_required_history();
    let minimum_tokens = state
        .tokenizer
        .estimate_kiro_payload(&minimum_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    if minimum_tokens > decision.maximum_tokens {
        return Err(upstream_error(
            ExecuteError::ContextLimit(ContextLimitError {
                model: decision.model.clone(),
                input_tokens: minimum_tokens,
                maximum: decision.maximum_tokens,
            }),
            ErrorFormat::Claude,
        )
        .with_context_diagnostics(state, source_payload, decision.maximum_tokens)
        .await);
    }
    Ok(if minimum_tokens > decision.target_tokens {
        // The 75% target is desirable headroom, not a smaller model window.
        // Prefer the trigger as the relaxed target, but never turn a client
        // trigger into a false hard limit when the indivisible current turn
        // still fits the model's real safe window.
        let relaxed_trigger = decision.trigger_tokens.min(decision.maximum_tokens);
        if minimum_tokens <= relaxed_trigger {
            relaxed_trigger
        } else {
            decision.maximum_tokens
        }
    } else {
        decision.target_tokens
    })
}

pub(super) async fn run_compaction(
    state: &Arc<AppState>,
    request: CompactionRequest<'_>,
) -> Result<CompactionRun, ApiError> {
    let CompactionRequest {
        trace_id,
        client_context,
        key_id,
        account_ids,
        allowed_models,
        service_id,
        source_payload,
        decision,
        summary_model,
        summary_timeout_ms,
        preserve_recent_turns,
        explicit_client_compaction,
    } = request;
    let operation_target = compaction_operation_target(state, source_payload, decision).await?;
    let plan = state
        .tokenizer
        .plan_kiro_compaction(
            source_payload,
            operation_target as usize,
            preserve_recent_turns,
        )
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    let Some(plan) = plan else {
        let original_tokens = state
            .tokenizer
            .estimate_kiro_payload(source_payload)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                    ErrorFormat::Claude,
                )
            })?;
        return Ok(CompactionRun {
            payload: source_payload.clone(),
            stats: crosslane_translate::ContextCompactionStats {
                original_tokens,
                compacted_tokens: original_tokens,
                ..crosslane_translate::ContextCompactionStats::default()
            },
            artifact: None,
            mode: "none",
            summary_model: None,
            summary_input_tokens: None,
            fallback_reason: None,
            iteration_usage: None,
        });
    };

    // Bound each request without discarding source facts before the model can
    // inspect them. An extractive checkpoint here silently lost the middle of
    // long messages even when the later semantic call reported success.
    let summary_context_maximum = conservative_summary_context_maximum(state, summary_model);
    // Leave headroom for tokenizer differences, upstream framing and output.
    // Real DeepSeek rejected a ~162k estimated input despite advertising 164k.
    let summary_chunk_input_limit = compact_target_from_maximum(summary_context_maximum);
    let summary_payload = compaction_summary_payload(source_payload, &plan, summary_model);
    let summary_input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&summary_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let summary_parts = state
        .tokenizer
        .partition_compaction_summary(summary_payload, summary_chunk_input_limit as usize)
        .await;
    // Token limits alone do not bound JSON bytes (whitespace and attachments
    // are typical counterexamples). Never dispatch an oversized summary part.
    let summary_parts = summary_parts.and_then(|parts| {
        let maximum = state.config.current().context.max_upstream_payload_bytes;
        for part in &parts {
            if super::serialized_payload_bytes(part, ErrorFormat::Claude)
                .map_err(|error| error.message)?
                > maximum
            {
                return Err("summary input exceeds the upstream payload byte budget".into());
            }
        }
        Ok(parts)
    });
    let mut fallback_reason = None;
    let mut semantic_summary = None;
    let mut iteration_usage = None;
    if let Ok(summary_parts) = summary_parts {
        let summary_chunk_count = summary_parts.len();
        let summary_concurrency = compaction_summary_concurrency(state);
        let client_requested = explicit_client_compaction
            || decision.reasons.contains(&CompactionReason::ClientTrigger);
        tracing::info!(
            trace_id,
            summary_model,
            summary_input_tokens,
            summary_context_maximum,
            summary_chunk_input_limit,
            summary_chunk_count,
            summary_concurrency,
            "complete compaction summary input partitioned within its model window"
        );
        if summary_chunk_count > summary_concurrency && !client_requested {
            // Each near-window-sized part can consume most of the one global
            // deadline. Starting multiple waves makes timeout-and-discard the
            // expected result and also burns upstream capacity for no usable
            // checkpoint during automatic overflow recovery. Explicit client
            // compaction still requests semantic work and retains its contract.
            fallback_reason = Some("summary_multiple_waves");
            tracing::info!(
                trace_id,
                summary_model,
                summary_chunk_count,
                summary_concurrency,
                summary_timeout_ms,
                "automatic semantic compaction needs multiple dispatch waves; using extractive fallback"
            );
        } else {
            match generate_compaction_summary(
                state,
                Arc::clone(&account_ids),
                trace_id,
                &client_context.cloned().unwrap_or_default(),
                key_id,
                allowed_models,
                service_id,
                summary_model,
                summary_parts,
                summary_timeout_ms,
            )
            .await
            {
                Ok(summary) => semantic_summary = Some(summary),
                Err(error) => {
                    iteration_usage = error.usage;
                    fallback_reason = Some(if error.message.contains("timed out") {
                        "summary_timeout"
                    } else {
                        "summary_upstream_error"
                    });
                    tracing::warn!(
                        trace_id,
                        reason = %sanitize_error_message(&error.message),
                        "semantic compaction request failed; using extractive fallback"
                    );
                }
            }
        }
    } else if let Err(reason) = summary_parts {
        fallback_reason = Some("summary_capacity_insufficient");
        tracing::warn!(
            trace_id, summary_model, summary_input_tokens, summary_context_maximum,
            reason = %sanitize_error_message(&reason),
            "semantic compaction summary cannot fit its bounds; using extractive fallback"
        );
    }

    if let Some(generated) = semantic_summary {
        iteration_usage = Some(generated.usage);
        let mut payload = source_payload.clone();
        match state
            .tokenizer
            .apply_semantic_compaction(
                &mut payload,
                &plan,
                &generated.content,
                operation_target as usize,
            )
            .await
        {
            Ok(mut stats) => {
                finalize_compaction_payload(state, &mut payload, &mut stats, "semantic compaction")
                    .await?;
                if stats.compacted_tokens as u64 <= operation_target
                    && super::serialized_payload_bytes(&payload, ErrorFormat::Claude)?
                        <= state.config.current().context.max_upstream_payload_bytes
                {
                    return Ok(CompactionRun {
                        payload,
                        stats,
                        artifact: Some(CompactionArtifact::Semantic {
                            source_payload: source_payload.clone(),
                            plan,
                            summary: generated.content,
                            usage: generated.usage,
                        }),
                        mode: "semantic",
                        summary_model: Some(summary_model.to_owned()),
                        summary_input_tokens: Some(summary_input_tokens),
                        fallback_reason: None,
                        iteration_usage: Some(generated.usage),
                    });
                }
                fallback_reason = Some("semantic_target_not_reached_after_tool_history_repair");
                tracing::warn!(
                    trace_id,
                    compacted_tokens = stats.compacted_tokens,
                    target_tokens = operation_target,
                    "semantic compaction exceeded the target after tool-history preparation; using extractive fallback"
                );
            }
            Err(error) => {
                fallback_reason = Some("semantic_target_not_reached");
                tracing::warn!(
                    trace_id,
                    reason = %sanitize_error_message(&error),
                    "semantic compaction could not satisfy the target; using extractive fallback"
                );
            }
        }
    }

    let (payload, stats) = extractive_compaction(
        state,
        source_payload,
        operation_target,
        preserve_recent_turns,
        Some(decision),
    )
    .await?;
    if stats.compacted_tokens as u64 > decision.maximum_tokens {
        return Err(upstream_error(
            ExecuteError::ContextLimit(ContextLimitError {
                model: decision.model.clone(),
                input_tokens: stats.compacted_tokens as u64,
                maximum: decision.maximum_tokens,
            }),
            ErrorFormat::Claude,
        ));
    }
    Ok(CompactionRun {
        payload,
        stats,
        artifact: Some(CompactionArtifact::Extractive {
            source_payload: source_payload.clone(),
            preserve_recent_turns,
            usage: iteration_usage,
        }),
        mode: "extractive_fallback",
        summary_model: Some(summary_model.to_owned()),
        summary_input_tokens: Some(summary_input_tokens),
        fallback_reason,
        iteration_usage,
    })
}

pub(super) async fn reapply_compaction(
    state: &Arc<AppState>,
    artifact: &CompactionArtifact,
    decision: &CompactionDecision,
) -> Result<CompactionRun, ApiError> {
    let source_payload = match artifact {
        CompactionArtifact::Semantic { source_payload, .. }
        | CompactionArtifact::Extractive { source_payload, .. } => source_payload,
    };
    let operation_target = compaction_operation_target(state, source_payload, decision).await?;
    let (payload, stats, mode, iteration_usage) = match artifact {
        CompactionArtifact::Semantic {
            source_payload,
            plan,
            summary,
            usage,
        } => {
            let mut payload = source_payload.clone();
            let first = state
                .tokenizer
                .apply_semantic_compaction(&mut payload, plan, summary, operation_target as usize)
                .await;
            let mut stats = match first {
                Ok(stats) => stats,
                Err(error) if operation_target < decision.maximum_tokens => {
                    tracing::warn!(
                        model = %decision.model,
                        target_tokens = operation_target,
                        safe_window = decision.maximum_tokens,
                        reason = %sanitize_error_message(&error),
                        "semantic artifact missed the preferred target; retrying at the safe window"
                    );
                    payload.clone_from(source_payload);
                    match state
                        .tokenizer
                        .apply_semantic_compaction(
                            &mut payload,
                            plan,
                            summary,
                            decision.maximum_tokens as usize,
                        )
                        .await
                    {
                        Ok(stats) => stats,
                        Err(error) => {
                            let actual = state
                                .tokenizer
                                .estimate_kiro_payload(&payload)
                                .await
                                .unwrap_or(decision.maximum_tokens as usize + 1)
                                as u64;
                            tracing::warn!(
                                model = %decision.model,
                                safe_window = decision.maximum_tokens,
                                reason = %sanitize_error_message(&error),
                                "semantic artifact could not fit the resolved context window"
                            );
                            return Err(upstream_error(
                                ExecuteError::ContextLimit(ContextLimitError {
                                    model: decision.model.clone(),
                                    input_tokens: actual,
                                    maximum: decision.maximum_tokens,
                                }),
                                ErrorFormat::Claude,
                            ));
                        }
                    }
                }
                Err(error) => {
                    let actual = state
                        .tokenizer
                        .estimate_kiro_payload(&payload)
                        .await
                        .unwrap_or(decision.maximum_tokens as usize + 1)
                        as u64;
                    tracing::warn!(
                        model = %decision.model,
                        safe_window = decision.maximum_tokens,
                        reason = %sanitize_error_message(&error),
                        "semantic artifact could not fit the resolved context window"
                    );
                    return Err(upstream_error(
                        ExecuteError::ContextLimit(ContextLimitError {
                            model: decision.model.clone(),
                            input_tokens: actual,
                            maximum: decision.maximum_tokens,
                        }),
                        ErrorFormat::Claude,
                    ));
                }
            };
            finalize_compaction_payload(
                state,
                &mut payload,
                &mut stats,
                "reapplied semantic compaction",
            )
            .await?;
            (payload, stats, "semantic", Some(*usage))
        }
        CompactionArtifact::Extractive {
            source_payload,
            preserve_recent_turns,
            usage,
        } => {
            let (payload, stats) = extractive_compaction(
                state,
                source_payload,
                operation_target,
                *preserve_recent_turns,
                Some(decision),
            )
            .await?;
            (payload, stats, "extractive_fallback", *usage)
        }
    };
    if stats.compacted_tokens as u64 > decision.maximum_tokens {
        return Err(upstream_error(
            ExecuteError::ContextLimit(ContextLimitError {
                model: decision.model.clone(),
                input_tokens: stats.compacted_tokens as u64,
                maximum: decision.maximum_tokens,
            }),
            ErrorFormat::Claude,
        ));
    }
    Ok(CompactionRun {
        payload,
        stats,
        artifact: Some(artifact.clone()),
        mode,
        summary_model: None,
        summary_input_tokens: None,
        fallback_reason: None,
        iteration_usage,
    })
}

fn execute_error_message(error: ExecuteError) -> String {
    match error {
        ExecuteError::Pool(error) => error.to_string(),
        ExecuteError::Upstream(error) => error.to_string(),
        ExecuteError::Dispatch(error) => error.error.to_string(),
        ExecuteError::Meter(error) => error.to_string(),
        ExecuteError::ContextLimit(limit) => format!(
            "compaction summary input is too long for {}: {} > {}",
            limit.model, limit.input_tokens, limit.maximum
        ),
        ExecuteError::ModelNotAllowed { provider_id, model } => {
            format!("model {provider_id}/{model} is not allowed for this API key")
        }
        ExecuteError::CrossProviderRoute {
            source_provider,
            target_provider,
            model,
        } => format!(
            "internal {source_provider} request cannot route model {model} to provider {target_provider}"
        ),
    }
}
