//! Responses web search, executed by the proxy rather than the client.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::BytesMut;
use crosslane_core::client_context::ClientContext;
use crosslane_core::provider::ProviderId;
use crosslane_translate::responses::{is_responses_web_search, RESPONSES_WEB_SEARCH_TOOL};
use crosslane_translate::{responses_to_openai, ResponsesRequest, WebSearchResults};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::provider_gateway::SelectedProviderRoute;
use super::{ApiError, ErrorFormat, ServiceHttpState};
use crate::http::responses::{
    frame_end, resume_responses_request, ResponsesOptions, ResponsesSessionOwner,
};
use crate::meter::AuthenticatedApiKey;
use crate::state::AdmissionGuard;

type CacheKey = (String, Option<String>, String);
type SearchCache = HashMap<CacheKey, (Instant, WebSearchResults)>;
static SEARCH_CACHE: OnceLock<Mutex<SearchCache>> = OnceLock::new();
type ReplayCache = HashMap<CacheKey, (Instant, Value, WebSearchResults)>;
static REPLAYS: OnceLock<Mutex<ReplayCache>> = OnceLock::new();

/// Claude Code invokes this protocol for an actual WebSearch subrequest.
/// Keep partial SSE output, but make a failed Kiro continuation explicit.
pub(super) fn claude_search_response(response: Response) -> Response {
    if !response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.headers.remove("content-length");
    let stream = async_stream::stream! {
        let mut source = body.into_data_stream();
        let mut pending = BytesMut::new();
        let mut completed = false;
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(chunk) => pending.extend_from_slice(&chunk),
                Err(_) => { break; }
            }
            if pending.len() > 16 * 1024 * 1024 { break; }
            while let Some((end, delimiter)) = frame_end(&pending) {
                let frame = pending.split_to(end + delimiter);
                let text = std::str::from_utf8(&frame[..end]).unwrap_or_default();
                let data = text.lines().filter_map(|line| line.strip_prefix("data:").map(str::trim_start)).collect::<Vec<_>>().join("\n");
                if let Ok(mut event) = serde_json::from_str::<Value>(&data) {
                    completed |= event["type"] == "message_stop";
                    if event["type"] == "error" {
                        let error = unsupported_search(event["error"]["message"].as_str().unwrap_or("Kiro search continuation failed"));
                        event["error"]["message"] = json!(error.message);
                        event["error"]["code"] = json!(error.error_code);
                        event["error"]["stage"] = json!(error.error_stage);
                        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("event: error\ndata: {event}\n\n")));
                        return;
                    }
                }
                yield Ok(Bytes::from(frame));
            }
        }
        if !completed {
            let error = unsupported_search("Kiro search continuation stream was interrupted");
            yield Ok(Bytes::from(format!("event: error\ndata: {}\n\n", json!({"type":"error","error":{"type":"invalid_request_error","message":error.message,"code":error.error_code,"stage":error.error_stage}}))));
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

pub(super) fn has_web_search(
    service: &ServiceHttpState,
    key: Option<&AuthenticatedApiKey>,
    body: &[u8],
) -> bool {
    let Ok(mut request) = serde_json::from_slice::<ResponsesRequest>(body) else {
        return false;
    };
    if let Some(previous) = &request.previous_response_id {
        let owner =
            ResponsesSessionOwner::new(service.service.id.clone(), key.map(|key| key.id.clone()));
        if let Some(session) = service.app.responses_sessions.get(previous, &owner) {
            resume_responses_request(&mut request, session);
        }
    }
    request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_responses_web_search))
        || request.input.as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["type"] == "additional_tools"
                    && item["tools"]
                        .as_array()
                        .is_some_and(|tools| tools.iter().any(is_responses_web_search))
            })
        })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle(
    service: ServiceHttpState,
    trace: String,
    path: String,
    headers: HeaderMap,
    route: SelectedProviderRoute,
    key: Option<AuthenticatedApiKey>,
    connection: AdmissionGuard,
    admission: AdmissionGuard,
    mut context: ClientContext,
) -> Pin<Box<dyn Future<Output = Result<Response, ApiError>> + Send>> {
    Box::pin(async move {
        let mut request: ResponsesRequest =
            serde_json::from_slice(&route.body).map_err(|error| invalid(error.to_string()))?;
        let key_id = key.as_ref().map(|key| key.id.clone());
        let owner = ResponsesSessionOwner::new(service.service.id.clone(), key_id.clone());
        let mut conversation = Uuid::new_v4().to_string();
        if let Some(previous) = &request.previous_response_id {
            let session = service
                .app
                .responses_sessions
                .get(previous, &owner)
                .ok_or_else(|| invalid("unknown or expired previous_response_id"))?;
            context.inherit(session.client_context()).map_err(invalid)?;
            if let Some(id) = session.conversation_id() {
                conversation = id.into();
            }
            resume_responses_request(&mut request, session);
        }
        let translated =
            responses_to_openai(&request).map_err(|error| invalid(error.to_string()))?;
        let declarations = translated.tools;
        let searches = declarations
            .iter()
            .filter(|tool| is_responses_web_search(tool))
            .collect::<Vec<_>>();
        if searches.len() != 1 {
            return Err(invalid("declare exactly one web_search tool"));
        }
        let declaration = searches[0].clone();
        validate_search_options(&declaration)?;
        let mut options = ResponsesOptions::new(
            &request,
            declarations.clone(),
            translated.tool_names,
            Some((service.app.responses_sessions.clone(), owner)),
            Some(conversation.clone()),
        );
        options.set_conversation_id(Some(conversation));
        options.set_client_context(&context);
        let streaming = request.stream;
        let mut body =
            serde_json::to_value(&request).map_err(|error| invalid(error.to_string()))?;
        body["previous_response_id"] = Value::Null;
        body["store"] = json!(false);
        body["stream"] = json!(true);
        body["tools"] = json!(declarations
            .into_iter()
            .map(|tool| {
                if is_responses_web_search(&tool) {
                    let definition = translated
                        .request
                        .tools
                        .iter()
                        .find_map(|tool| {
                            let definition = tool.body.get("function")?;
                            (definition["name"] == RESPONSES_WEB_SEARCH_TOOL).then_some(definition)
                        })
                        .expect("normalized search tool");
                    let mut function = definition.clone();
                    function["type"] = json!("function");
                    function
                } else {
                    tool
                }
            })
            .collect::<Vec<_>>());
        if let Some(choice) = &request.tool_choice {
            if is_responses_web_search(choice) {
                body["tool_choice"] = json!({"type":"function","name":RESPONSES_WEB_SEARCH_TOOL});
            } else if choice["type"] == "allowed_tools" {
                let allowed = body["tool_choice"]
                    .as_object_mut()
                    .expect("validated tool choice");
                let allowed = if allowed.contains_key("allowed_tools") {
                    allowed
                        .get_mut("allowed_tools")
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                } else {
                    allowed
                };
                for tool in allowed.get_mut("tools").unwrap().as_array_mut().unwrap() {
                    if is_responses_web_search(tool) {
                        *tool = json!({"type":"function","name":RESPONSES_WEB_SEARCH_TOOL});
                    }
                }
            }
        }
        body["input"] = match request.input {
            Value::String(text) => {
                json!([{"role":"user","content":[{"type":"input_text","text":text}]}])
            }
            Value::Array(items) => json!(items
                .into_iter()
                .filter(|item| item["type"] != "additional_tools")
                .collect::<Vec<_>>()),
            _ => return Err(invalid("input must be a string or array")),
        };
        let pending_search = pending_search_continuation(&body["input"]);
        replay_history(&mut body["input"], &service.service.id, key_id.as_deref())?;
        let (route, key) = if pending_search && route.provider_kind == "copilot" {
            kiro_search_route(&service, &route, key.as_ref(), &body).await?
        } else {
            (route, key)
        };
        body["model"] = json!(route.upstream_model);
        let failure_context = streaming.then(|| {
            (
                service.app.clone(),
                trace.clone(),
                path.clone(),
                super::logged_user_agent(&headers),
                route.upstream_model.clone(),
                Instant::now(),
            )
        });
        let (sender, receiver) = mpsc::channel(32);
        let operation = execute(
            service,
            trace,
            path,
            headers,
            route,
            key,
            connection,
            admission,
            context,
            body,
            declaration,
            streaming.then_some(sender),
        );
        if streaming {
            let operation = async move {
                operation.await.map_err(|mut error| {
                    error.client_status = Some(StatusCode::OK.as_u16());
                    if let Some((state, trace, path, user_agent, model, started)) = failure_context
                    {
                        super::entrypoints::record_failed_request(
                            &state,
                            &trace,
                            &path,
                            &user_agent,
                            &model,
                            started,
                            &error,
                        );
                    }
                    (error.error_code.to_owned(), error.message.to_string())
                })
            };
            Ok(crate::http::responses::hosted_stream(
                operation, receiver, options,
            ))
        } else {
            let response = operation.await?;
            Ok(Json(options.finalize_hosted(response).map_err(invalid)?).into_response())
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    service: ServiceHttpState,
    trace: String,
    path: String,
    headers: HeaderMap,
    mut route: SelectedProviderRoute,
    mut key: Option<AuthenticatedApiKey>,
    connection: AdmissionGuard,
    admission: AdmissionGuard,
    mut context: ClientContext,
    mut body: Value,
    declaration: Value,
    sender: Option<mpsc::Sender<Value>>,
) -> Result<Value, ApiError> {
    let config = service.app.config.current();
    let maximum = config
        .features
        .web_search_max_rounds
        .clamp(1, 20)
        .min(body["max_tool_calls"].as_u64().unwrap_or(20).min(20) as u32);
    let mut remaining = body["max_output_tokens"].as_u64();
    let mut records = Vec::<Value>::new();
    let mut indices = HashMap::<String, usize>::new();
    let mut total_usage = json!({"input_tokens":0,"output_tokens":0,"total_tokens":0});
    let mut searches = 0;
    let mut switched = pending_search_continuation(&body["input"]);
    route.web_search_operation |= switched;
    for _ in 0..=maximum {
        route.body = Bytes::from(body.to_string());
        let response = Box::pin(super::request::handle_openai_route(
            service.clone(),
            trace.clone(),
            path.clone(),
            headers.clone(),
            route.body.clone(),
            route.clone(),
            key.clone(),
            connection.clone(),
            admission.clone(),
            &mut context,
        ))
        .await
        .map_err(|error| {
            if switched {
                unsupported_search(&error.message)
            } else {
                error
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            let bytes = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .map_err(|error| invalid(error.to_string()))?;
            let error: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            return Err(generation_failure(
                error["error"]["message"]
                    .as_str()
                    .unwrap_or("generation failed"),
                switched,
                status,
            ));
        }
        let mut source = response.into_body().into_data_stream();
        let mut pending = BytesMut::new();
        let mut hidden = HashSet::<String>::new();
        let mut round_indices = HashMap::<u64, usize>::new();
        let mut completed = None;
        while let Some(chunk) = source.next().await {
            pending.extend_from_slice(&chunk.map_err(|error| {
                generation_failure(error.to_string(), switched, StatusCode::BAD_GATEWAY)
            })?);
            if pending.len() > 16 * 1024 * 1024 {
                return Err(invalid("generation stream exceeds the event limit"));
            }
            while let Some((end, delimiter)) = frame_end(&pending) {
                let frame = pending.split_to(end + delimiter);
                let text = std::str::from_utf8(&frame[..end])
                    .map_err(|_| invalid("invalid generation stream encoding"))?;
                let data = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let mut event: Value = serde_json::from_str(&data)
                    .map_err(|_| invalid("invalid generation stream JSON"))?;
                match event["type"].as_str().unwrap_or_default() {
                    "response.created" | "response.in_progress" => continue,
                    "response.completed" | "response.incomplete" => {
                        completed = Some(event["response"].clone());
                        continue;
                    }
                    "response.failed" | "error" => {
                        return Err(generation_failure(
                            event["message"]
                                .as_str()
                                .or_else(|| {
                                    event
                                        .pointer("/response/error/message")
                                        .and_then(Value::as_str)
                                })
                                .unwrap_or("generation stream failed"),
                            switched,
                            StatusCode::BAD_GATEWAY,
                        ))
                    }
                    _ => {}
                }
                let item = &event["item"];
                let id = item["id"]
                    .as_str()
                    .or_else(|| event["item_id"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                if item["type"] == "function_call" && item["name"] == RESPONSES_WEB_SEARCH_TOOL {
                    hidden.insert(id.clone());
                }
                if hidden.contains(&id) {
                    continue;
                }
                if event["type"] == "response.output_item.added" {
                    let index = records.len();
                    indices.insert(id.clone(), index);
                    records.push(item.clone());
                    if let Some(old) = event["output_index"].as_u64() {
                        round_indices.insert(old, index);
                    }
                }
                if let Some(index) = event["output_index"]
                    .as_u64()
                    .and_then(|old| round_indices.get(&old))
                {
                    event["output_index"] = json!(index);
                }
                if event["type"] == "response.output_item.done" {
                    if let Some(index) = indices.get(&id) {
                        records[*index] = event["item"].clone();
                    }
                }
                if let Some(sender) = &sender {
                    sender
                        .send(event)
                        .await
                        .map_err(|_| invalid("client disconnected"))?;
                }
            }
        }
        let mut response = completed.ok_or_else(|| {
            generation_failure(
                "generation stream ended before completion",
                switched,
                StatusCode::BAD_GATEWAY,
            )
        })?;
        add_usage(&mut total_usage, &response["usage"]);
        if let Some(budget) = &mut remaining {
            *budget = budget.saturating_sub(
                response["usage"]["output_tokens"]
                    .as_u64()
                    .unwrap_or_default(),
            );
        }
        let output = response["output"]
            .as_array()
            .ok_or_else(|| invalid("generation returned no output array"))?;
        let calls = output
            .iter()
            .filter(|item| {
                item["type"] == "function_call" && item["name"] == RESPONSES_WEB_SEARCH_TOOL
            })
            .cloned()
            .collect::<Vec<_>>();
        let client_calls = output.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            ) && item["name"] != RESPONSES_WEB_SEARCH_TOOL
        });
        body["input"]
            .as_array_mut()
            .unwrap()
            .extend(output.iter().cloned());
        // Never dispatch truncated arguments. The client receives the ordinary
        // incomplete response and can choose its own continuation budget.
        if response["status"] == "incomplete" {
            response["output"] = json!(records);
            response["usage"] = total_usage;
            return Ok(response);
        }
        if !calls.is_empty() && route.provider_kind == "copilot" {
            // Merely advertising search leaves ordinary turns on Copilot.
            // Once it is invoked, both execution and continuation use Kiro.
            (route, key) = kiro_search_route(&service, &route, key.as_ref(), &body).await?;
            body["model"] = json!(route.upstream_model);
        }
        switched |= !calls.is_empty();
        route.web_search_operation |= switched;
        for call in &calls {
            if searches >= maximum {
                return Err(invalid("web search reached the configured execution limit"));
            }
            searches += 1;
            let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap_or("{}"))
                .map_err(|_| invalid("web_search returned invalid query arguments"))?;
            let query = arguments["query"]
                .as_str()
                .filter(|query| !query.trim().is_empty() && query.len() <= 8192)
                .ok_or_else(|| invalid("web_search query is required"))?;
            let id = format!("ws_crosslane_{}", Uuid::new_v4().simple());
            let index = records.len();
            let mut item = json!({"id":id,"type":"web_search_call","status":"in_progress","action":{"type":"search","query":query}});
            if let Some(sender) = &sender {
                sender.send(json!({"type":"response.output_item.added","output_index":index,"item":item})).await.map_err(|_| invalid("client disconnected"))?;
                sender.send(json!({"type":"response.web_search_call.searching","output_index":index,"item_id":id})).await.map_err(|_| invalid("client disconnected"))?;
            }
            let results = search(&service, key.as_ref(), &declaration, query)
                .await
                .map_err(|error| unsupported_search(&error.message))?;
            store_replay(
                &service.service.id,
                key.as_ref().map(|key| key.id.as_str()),
                &id,
                call,
                &results,
            );
            item["status"] = json!("completed");
            item["action"]["sources"] = json!(results
                .results
                .iter()
                .map(|result| json!({"type":"url","url":result.url,"title":result.title}))
                .collect::<Vec<_>>());
            if let Some(sender) = &sender {
                sender.send(json!({"type":"response.web_search_call.completed","output_index":index,"item_id":id})).await.map_err(|_| invalid("client disconnected"))?;
                sender.send(json!({"type":"response.output_item.done","output_index":index,"item":item})).await.map_err(|_| invalid("client disconnected"))?;
            }
            records.push(item);
            body["input"].as_array_mut().unwrap().push(json!({"type":"function_call_output","call_id":call["call_id"],"output":serde_json::to_string(&results).map_err(|error| invalid(error.to_string()))?}));
        }
        if calls.is_empty()
            || client_calls
            || remaining == Some(0)
            || response["status"] == "incomplete"
        {
            response["output"] = json!(records);
            response["usage"] = total_usage;
            return Ok(response);
        }
        if let Some(tokens) = remaining {
            body["max_output_tokens"] = json!(tokens);
        }
        if body["tool_choice"]["name"] == RESPONSES_WEB_SEARCH_TOOL {
            body["tool_choice"] = json!("auto");
        }
    }
    Err(invalid("web search reached the configured execution limit"))
}

async fn search(
    service: &ServiceHttpState,
    key: Option<&AuthenticatedApiKey>,
    declaration: &Value,
    query: &str,
) -> Result<WebSearchResults, ApiError> {
    let config = service.app.config.current();
    if config.features.disable_tools || !config.features.enable_web_tools {
        return Err(invalid("web search is disabled"));
    }
    let mut scoped = service.current_service();
    let backend = scoped
        .web_search_provider
        .clone()
        .unwrap_or_else(|| config.default_provider_for_service(&scoped).into());
    if !config
        .effective_providers()
        .iter()
        .any(|provider| provider.id == backend && provider.kind == "kiro" && provider.enabled)
    {
        return Err(unsupported_search("Kiro search backend is unavailable"));
    }
    if !scoped.allowed_providers.contains(&backend) {
        scoped.allowed_providers.push(backend.clone());
    }
    let ids = service
        .app
        .pool()
        .matching_account_ids(|account| {
            scoped.includes_provider_account(&backend, &account.id, &account.tags)
        })
        .await;
    let mut ordered_ids = ids.iter().cloned().collect::<Vec<_>>();
    ordered_ids.sort();
    let mut cache_options = declaration.clone();
    cache_options
        .as_object_mut()
        .unwrap()
        .remove("external_web_access");
    let cache_key = (
        service.service.id.clone(),
        key.map(|key| key.id.clone()),
        format!("{backend}:{ordered_ids:?}:{query}:{cache_options}"),
    );
    if declaration["external_web_access"] == false {
        let cache = SEARCH_CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        return cache.get(&cache_key).filter(|(time, _)| time.elapsed() < Duration::from_secs(300)).map(|(_, results)| results.clone())
            .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "No cached web search result is available; request external_web_access:true for a live search", ErrorFormat::OpenAi));
    }
    let lease = service
        .app
        .pool()
        .acquire_scoped_excluding("", 0.0, &ids, &HashSet::new())
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error.to_string(),
                ErrorFormat::OpenAi,
            )
        })?;
    let mut search_query = query.to_owned();
    if let Some(domains) = declaration
        .pointer("/filters/allowed_domains")
        .and_then(Value::as_array)
        .filter(|domains| !domains.is_empty())
    {
        search_query.push_str(&format!(
            " ({})",
            domains
                .iter()
                .filter_map(Value::as_str)
                .map(|domain| format!("site:{domain}"))
                .collect::<Vec<_>>()
                .join(" OR ")
        ));
    }
    if let Some(location) = declaration
        .get("user_location")
        .filter(|location| !location.is_null())
    {
        search_query.push_str(&format!(" [approximate user location: {location}]"));
    }
    let mut results = super::execute_kiro_web_search(&service.app, &lease, &search_query)
        .await
        .map_err(|error| {
            ApiError::new(StatusCode::BAD_GATEWAY, error.message, ErrorFormat::OpenAi)
        })?;
    if let Some(domains) = declaration
        .pointer("/filters/allowed_domains")
        .and_then(Value::as_array)
        .filter(|domains| !domains.is_empty())
    {
        results.results.retain(|result| {
            url::Url::parse(&result.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| {
                    domains
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_ascii_lowercase)
                        .any(|domain| host == domain || host.ends_with(&format!(".{domain}")))
                })
        });
    }
    let limit = match declaration["search_context_size"].as_str() {
        Some("low") => 5,
        Some("high") => 20,
        _ => 10,
    };
    results.results.truncate(limit);
    results.query = query.into();
    results.total_results = results.results.len() as u64;
    for result in &mut results.results {
        result.snippet = result.snippet.chars().take(4096).collect();
    }
    let mut cache = SEARCH_CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, (time, _)| time.elapsed() < Duration::from_secs(300));
    if cache.len() >= 256 {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (time, _))| *time)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(cache_key, (Instant::now(), results.clone()));
    Ok(results)
}

fn validate_search_options(declaration: &Value) -> Result<(), ApiError> {
    if declaration
        .get("search_context_size")
        .is_some_and(|size| !matches!(size.as_str(), Some("low" | "medium" | "high")))
    {
        return Err(invalid("search_context_size must be low, medium, or high"));
    }
    if declaration
        .get("external_web_access")
        .is_some_and(|flag| !flag.is_boolean())
    {
        return Err(invalid("external_web_access must be a boolean"));
    }
    if let Some(domains) = declaration.pointer("/filters/allowed_domains") {
        if !domains.as_array().is_some_and(|domains| {
            domains.len() <= 100
                && domains.iter().all(|domain| {
                    domain.as_str().is_some_and(|domain| {
                        !domain.is_empty()
                            && domain.len() <= 253
                            && domain
                                .bytes()
                                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'-'))
                    })
                })
        }) {
            return Err(invalid("allowed_domains must contain host names"));
        }
    }
    if let Some(location) = declaration
        .get("user_location")
        .filter(|location| !location.is_null())
    {
        if location["type"] != "approximate"
            || !location.as_object().is_some_and(|location| {
                location.iter().all(|(name, value)| {
                    matches!(
                        name.as_str(),
                        "type" | "country" | "region" | "city" | "timezone"
                    ) && value.as_str().is_some_and(|text| text.len() <= 256)
                })
            })
        {
            return Err(invalid(
                "user_location must contain an approximate location",
            ));
        }
    }
    Ok(())
}

fn add_usage(total: &mut Value, usage: &Value) {
    if let Some(fields) = usage.as_object() {
        if !total.is_object() {
            *total = json!({});
        }
        for (name, value) in fields {
            if let Some(number) = value.as_u64() {
                total[name] = json!(total[name]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_add(number));
            } else if value.is_object() {
                add_usage(&mut total[name], value);
            }
        }
    }
}

fn store_replay(
    service: &str,
    key: Option<&str>,
    id: &str,
    call: &Value,
    results: &WebSearchResults,
) {
    let mut cache = REPLAYS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, (time, _, _)| time.elapsed() < Duration::from_secs(30 * 60));
    if cache.len() >= 256 {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (time, _, _))| *time)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        (service.into(), key.map(str::to_owned), id.into()),
        (Instant::now(), call.clone(), results.clone()),
    );
}

fn replay_history(input: &mut Value, service: &str, key: Option<&str>) -> Result<(), ApiError> {
    let Some(items) = input.as_array_mut() else {
        return Ok(());
    };
    let mut replayed = Vec::new();
    let cache = REPLAYS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for item in items.iter() {
        if item["type"] == "web_search_call"
            && item["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("ws_crosslane_"))
        {
            let (time, call, results) = cache
                .get(&(
                    service.into(),
                    key.map(str::to_owned),
                    item["id"].as_str().unwrap().into(),
                ))
                .filter(|(time, _, _)| time.elapsed() < Duration::from_secs(30 * 60))
                .ok_or_else(|| {
                    invalid(
                        "unknown or expired web_search_call replay for this service and API key",
                    )
                })?;
            let _ = time;
            replayed.push(call.clone());
            replayed.push(json!({"type":"function_call_output","call_id":call["call_id"],"output":serde_json::to_string(results).map_err(|error| invalid(error.to_string()))?}));
        } else {
            replayed.push(item.clone());
        }
    }
    *items = replayed;
    Ok(())
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message, ErrorFormat::OpenAi)
}

fn generation_failure(message: impl Into<String>, searched: bool, status: StatusCode) -> ApiError {
    let message = message.into();
    if searched {
        unsupported_search(message)
    } else {
        ApiError::new(status, message, ErrorFormat::OpenAi)
    }
}

fn pending_search_continuation(input: &Value) -> bool {
    input.as_array().is_some_and(|items| {
        for item in items.iter().rev() {
            if item["role"] == "user" || item["type"] == "message" && item["role"] == "assistant" {
                return false;
            }
            if item["type"] == "web_search_call"
                && item["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("ws_crosslane_"))
                || item["type"] == "function_call" && item["name"] == RESPONSES_WEB_SEARCH_TOOL
            {
                return true;
            }
        }
        false
    })
}

pub(super) async fn kiro_search_route(
    service: &ServiceHttpState,
    original: &SelectedProviderRoute,
    key: Option<&AuthenticatedApiKey>,
    body: &Value,
) -> Result<(SelectedProviderRoute, Option<AuthenticatedApiKey>), ApiError> {
    let config = service.app.config.current();
    let mut scope = service.current_service();
    let backend = scope
        .web_search_provider
        .clone()
        .unwrap_or_else(|| config.default_provider_for_service(&scope).into());
    if !config
        .effective_providers()
        .iter()
        .any(|provider| provider.id == backend && provider.kind == "kiro" && provider.enabled)
    {
        return Err(unsupported_search("Kiro search backend is unavailable"));
    }
    if !scope.allowed_providers.contains(&backend) {
        scope.allowed_providers.push(backend.clone());
    }
    let ids = service
        .app
        .pool()
        .matching_account_ids(|account| {
            scope.includes_provider_account(&backend, &account.id, &account.tags)
        })
        .await;
    if ids.is_empty() {
        return Err(unsupported_search(
            "no Kiro account is authorized for this service",
        ));
    }
    let mut route = original.clone();
    route.provider_id = ProviderId::parse(&backend)
        .map_err(|_| unsupported_search("invalid Kiro search backend"))?;
    route.provider_kind = "kiro".into();
    route.original_model = original
        .original_model
        .split_once('/')
        .map_or(original.original_model.as_str(), |(_, model)| model)
        .into();
    route.upstream_model = route.original_model.clone();
    route.mapping_rule = None;
    route.lock_kiro_mapping = false;
    route.enable_model_fallback = false;
    route.web_search_operation = true;
    route.account_pool = Some(scope);
    route.account_ids = Some(std::sync::Arc::new(ids));
    let mut body = body.clone();
    body["model"] = json!(route.original_model);
    route.body = Bytes::from(body.to_string());
    let key = key.cloned().map(|mut key| {
        // Authorization for the requested Copilot model was checked before
        // entering this operation. Preserve its model masks for the Kiro
        // continuation instead of granting unrestricted Kiro generation.
        key.allowed_models = key
            .allowed_models
            .iter()
            .map(|mask| {
                mask.strip_prefix(&format!("{}/", original.provider_id))
                    .map_or_else(|| mask.clone(), |model| format!("{backend}/{model}"))
            })
            .collect();
        key
    });
    tracing::info!(event="proxy.web_search.provider_switched", from_provider=%original.provider_id, provider_id=%backend, "web search and its continuation switched to Kiro");
    Ok((route, key))
}

pub(super) fn unsupported_search(reason: impl AsRef<str>) -> ApiError {
    let mut error = ApiError::new(
        StatusCode::BAD_REQUEST,
        format!(
            "web_search is not supported: {}",
            crosslane_translate::sanitize_error_message(
                reason
                    .as_ref()
                    .strip_prefix("web_search is not supported: ")
                    .unwrap_or(reason.as_ref())
            )
        ),
        ErrorFormat::OpenAi,
    );
    error.error_code = "unsupported_tool_protocol";
    error.error_stage = "web_search";
    error
}
