use super::*;

fn replay_codec() -> WebSearchReplayCodec {
    WebSearchReplayCodec::from_key([0x6B; 32])
}

fn streamed_values(output: &[String]) -> Vec<Value> {
    output
        .iter()
        .flat_map(|event| event.lines())
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn unnamed_tool_fragments_keep_upstream_identity_before_client_aliases() {
    let mut names = std::collections::HashMap::new();
    let aliases = std::collections::HashMap::from([("client_search".into(), "web_search".into())]);
    let tool_event = |id: &str, name: &str| KiroEvent::ToolUse {
        id: id.into(),
        name: name.into(),
        input_delta: String::new(),
        stop: false,
    };
    let mut client = tool_event("client", "client_search");
    restore_tool_event_name(&mut client, &mut names);
    restore_web_tool_name(&mut client, &aliases);
    let mut server = tool_event("server", "web_search");
    restore_tool_event_name(&mut server, &mut names);

    let mut client_fragment = tool_event("client", "");
    restore_tool_event_name(&mut client_fragment, &mut names);
    assert!(!event_is_web_search(&client_fragment, 1));
    assert!(matches!(client_fragment, KiroEvent::ToolUse { name, .. } if name == "client_search"));
    let mut server_fragment = tool_event("server", "");
    restore_tool_event_name(&mut server_fragment, &mut names);
    assert!(event_is_web_search(&server_fragment, 1));
}

#[test]
fn stream_failure_diagnostics_preserve_error_sources_and_metrics() {
    let error = std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "peer closed early"),
    );
    let mut metrics = UpstreamStreamMetrics::new();
    metrics.observe_chunk(128);
    metrics.observe_event();

    let diagnostics = StreamFailureDiagnostics::from_event_stream(
        "event_stream_eof",
        &error,
        &metrics,
        7,
        600_000,
    );

    assert_eq!(diagnostics.kind, "event_stream_eof");
    assert_eq!(diagnostics.transport_class, "not_applicable");
    assert!(diagnostics.source_chain.contains("peer closed early"));
    assert!(diagnostics.chunk_seen);
    assert_eq!(diagnostics.chunks, 1);
    assert_eq!(diagnostics.bytes, 128);
    assert_eq!(diagnostics.events, 1);
    assert_eq!(diagnostics.buffered_bytes, 7);
    assert_eq!(diagnostics.configured_read_timeout_ms, 600_000);
}

#[test]
fn error_chain_is_single_line_and_bounded() {
    let message = format!("root\n{}", "x".repeat(3_000));
    let error = std::io::Error::other(message);
    let chain = format_error_chain(&error);

    assert!(!chain.contains('\n'));
    assert!(chain.ends_with("..."));
    assert!(chain.chars().count() <= 2_051);
}

#[test]
fn stream_failures_keep_real_upstream_status_and_account_scope() {
    for rejection in [
        "CONTEXT_LENGTH_EXCEEDED",
        "INPUT_TOO_LONG",
        "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
        "Input content length exceeds threshold.",
        "The context window has been exceeded",
    ] {
        let failure = classify_stream_failure(
            &format!("Kiro Amazon Q returned Some(400): {rejection}"),
            None,
        );
        assert_eq!(failure.upstream_status, Some(400));
        assert_eq!(failure.error_code, "context_length_exceeded");
        assert_eq!(failure.scope, StreamFailureScope::Client);
        assert!(!failure.account_error());
    }
    let rejected = classify_stream_failure(
        "Kiro Amazon Q returned Some(400): tool schema payload too large",
        None,
    );
    assert_eq!(rejected.upstream_status, Some(400));
    assert_eq!(rejected.error_code, "tool_budget_exceeded");
    assert_eq!(rejected.scope, StreamFailureScope::Client);
    assert!(!rejected.account_error());

    let rejected_5xx = classify_stream_failure(
        "Kiro Amazon Q returned Some(503): tool schema payload too large",
        None,
    );
    assert_eq!(rejected_5xx.upstream_status, Some(503));
    assert_eq!(rejected_5xx.error_code, "tool_budget_exceeded");
    assert!(!rejected_5xx.account_error());

    let unavailable = classify_stream_failure(
        "Kiro Amazon Q returned Some(503): Internal Server Error",
        None,
    );
    assert_eq!(unavailable.upstream_status, Some(503));
    assert_eq!(unavailable.error_code, "upstream_unavailable");
    assert_eq!(unavailable.scope, StreamFailureScope::Upstream);
    assert!(!unavailable.account_error());

    let model_unavailable = classify_stream_failure(
        r#"Kiro Amazon Q returned Some(500): {"message":"Encountered unexpectedly high load when processing the request, please try again.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#,
        None,
    );
    assert_eq!(model_unavailable.upstream_status, Some(500));
    assert_eq!(model_unavailable.error_code, "upstream_model_unavailable");
    assert_eq!(model_unavailable.scope, StreamFailureScope::Model);
    assert!(!model_unavailable.account_error());

    let throttled = classify_stream_failure(
        "Kiro Amazon Q returned Some(429): throttling exception",
        None,
    );
    assert_eq!(throttled.error_code, "upstream_rate_limited");
    assert_eq!(throttled.scope, StreamFailureScope::Model);
    assert!(!throttled.account_error());

    let auth =
        classify_stream_failure("Kiro Amazon Q returned Some(401): unauthorized token", None);
    assert_eq!(auth.error_code, "upstream_authentication_failed");
    assert_eq!(auth.scope, StreamFailureScope::Account);
    assert!(auth.account_error());

    let quota_validation = classify_stream_failure("ValidationException: quota exceeded", None);
    assert_eq!(quota_validation.error_code, "upstream_quota_exhausted");
    assert_eq!(quota_validation.scope, StreamFailureScope::Account);

    let auth_validation =
        classify_stream_failure("ValidationException: authentication failed", None);
    assert_eq!(auth_validation.error_code, "upstream_authentication_failed");
    assert_eq!(auth_validation.scope, StreamFailureScope::Account);
}

#[test]
fn transport_and_event_decode_failures_do_not_penalize_accounts() {
    let transport = StreamFailureDiagnostics {
        kind: "http_body_read",
        transport_class: "decode",
        transport_decode: true,
        ..StreamFailureDiagnostics::default()
    };
    let body_decode = classify_stream_failure("error decoding response body", Some(&transport));
    assert_eq!(body_decode.error_code, "upstream_transport_interrupted");
    assert_eq!(body_decode.error_stage, "upstream_stream_transport");
    assert_eq!(body_decode.scope, StreamFailureScope::Endpoint);
    assert!(!body_decode.account_error());

    let timeout = StreamFailureDiagnostics {
        transport_timeout: true,
        ..transport.clone()
    };
    let idle_timeout = classify_stream_failure("operation timed out", Some(&timeout));
    assert_eq!(idle_timeout.error_code, "upstream_idle_timeout");
    assert_eq!(idle_timeout.scope, StreamFailureScope::Endpoint);
    assert!(!idle_timeout.account_error());

    let event_decode = StreamFailureDiagnostics {
        kind: "event_stream_eof",
        transport_class: "not_applicable",
        ..StreamFailureDiagnostics::default()
    };
    let corrupt = classify_stream_failure("unexpected eof", Some(&event_decode));
    assert_eq!(corrupt.error_code, "upstream_event_stream_corrupt");
    assert_eq!(corrupt.scope, StreamFailureScope::Upstream);
    assert!(!corrupt.account_error());
}

#[test]
fn classified_stream_errors_include_stable_code_and_request_id_without_done() {
    let claude = classified_stream_error(
        &StreamProtocol::Claude,
        "Kiro Amazon Q returned Some(503): Internal Server Error",
        "req_test",
        None,
    );
    assert!(claude.contains("\"code\":\"upstream_unavailable\""));
    assert!(claude.contains("\"request_id\":\"req_test\""));
    #[cfg(feature = "crs")]
    assert!(claude
        .contains("Upstream request failed (code: upstream_unavailable, request ID: req_test)"));

    let openai = classified_stream_error(
        &StreamProtocol::OpenAi,
        "Kiro Amazon Q returned Some(503): Internal Server Error",
        "req_test",
        None,
    );
    assert!(openai.contains("\"code\":\"upstream_unavailable\""));
    assert!(openai.contains("\"request_id\":\"req_test\""));
    assert!(!openai.contains("[DONE]"));
    assert!(!openai.contains("Upstream request failed"));

    let internal = stream_error(
        &StreamProtocol::Claude,
        "req_internal",
        "Tool Search operation limit was reached",
    );
    assert!(internal.contains("\"code\":\"upstream_unavailable\""));
    assert!(internal.contains("\"request_id\":\"req_internal\""));
    assert!(!internal.contains("\"code\":null"));
    assert!(!internal.contains("\"request_id\":null"));
}

#[test]
fn replay_encryption_failures_are_proxy_scoped() {
    let details = classify_stream_failure(WEB_SEARCH_REPLAY_FAILURE_MESSAGE, None);
    assert_eq!(details.upstream_status, None);
    assert_eq!(details.error_code, "proxy_internal_error");
    assert_eq!(details.error_stage, "response_assembly");
    assert_eq!(details.scope, StreamFailureScope::Proxy);
    assert!(!details.account_error());

    let event = stream_error(
        &StreamProtocol::Claude,
        "req_replay",
        WEB_SEARCH_REPLAY_FAILURE_MESSAGE,
    );
    assert!(event.contains("\"code\":\"proxy_internal_error\""));
    assert!(event.contains("\"request_id\":\"req_replay\""));
}

#[test]
fn thinking_signature_stays_on_the_same_block_and_message_starts_once() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let mut output = state.event(&KiroEvent::Reasoning {
        content: "think".into(),
        signature: Some("real-signature".into()),
        redacted_content: None,
    });
    output.extend(state.event(&KiroEvent::AssistantResponse {
        content: "answer".into(),
    }));
    let joined = output.join("");
    assert_eq!(joined.matches("event: message_start").count(), 1);
    assert_eq!(joined.matches("signature_delta").count(), 1);
    let signature = joined.find("signature_delta").expect("signature");
    let stop = joined.find("content_block_stop").expect("stop");
    assert!(signature < stop);
    assert!(joined.contains("\"index\":0"));
}

#[test]
fn unsigned_tagged_thinking_streams_as_literal_text() {
    let mut filter = ThinkingContentFilter::new(true);
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let mut output = Vec::new();
    for chunk in ["<thin", "king>hidden", "</thinking>Hello"] {
        for event in filter.push(KiroEvent::AssistantResponse {
            content: chunk.into(),
        }) {
            output.extend(state.event(&event));
        }
    }
    for event in filter.finish() {
        output.extend(state.event(&event));
    }
    let joined = output.join("");
    let values = streamed_values(&output);

    assert!(!joined.contains("thinking_delta"));
    assert!(!joined.contains("signature_delta"));
    // Tagged text now streams incrementally instead of buffering an entire
    // reasoning block. SSE fragment boundaries are not part of its content.
    let text: String = values
        .iter()
        .filter(|value| value["delta"]["type"] == "text_delta")
        .filter_map(|value| value["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "<thinking>hidden</thinking>Hello");
}

#[test]
fn disabled_thinking_streams_only_visible_text() {
    let mut filter = ThinkingContentFilter::new(false);
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let mut events = filter.push(KiroEvent::AssistantResponse {
        content: "<thinking>tagged secret</thinking>Hello".into(),
    });
    events.extend(filter.push(KiroEvent::Reasoning {
        content: "native secret".into(),
        signature: None,
        redacted_content: None,
    }));
    events.extend(filter.finish());
    let output = events
        .iter()
        .flat_map(|event| state.event(event))
        .collect::<Vec<_>>();
    let joined = output.join("");
    let values = streamed_values(&output);

    assert!(!joined.contains("thinking_delta"));
    assert!(!joined.contains("signature_delta"));
    assert!(!joined.contains("secret"));
    assert!(values.iter().any(|value| {
        value["delta"]["type"] == "text_delta" && value["delta"]["text"] == "Hello"
    }));
}

#[test]
fn finish_does_not_fabricate_a_thinking_signature() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let started = state.event(&KiroEvent::Reasoning {
        content: "think".into(),
        signature: None,
        redacted_content: None,
    });
    let decoded = DecodedResponse {
        reasoning: "think".into(),
        ..DecodedResponse::default()
    };
    let finished = stream_finish(
        &StreamProtocol::Claude,
        &mut state,
        &decoded,
        0,
        "model",
        Some(100),
        10,
        ThinkingOutputFormat::Claude,
        false,
    );
    let joined = [started, finished].concat().join("");
    assert_eq!(joined.matches("event: message_start").count(), 1);
    assert_eq!(joined.matches("\"type\":\"thinking\"").count(), 1);
    assert_eq!(joined.matches("signature_delta").count(), 0);
}

#[test]
fn compaction_stream_block_is_emitted_before_text() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let mut output = state.compaction("summary");
    output.extend(state.event(&KiroEvent::AssistantResponse {
        content: "answer".into(),
    }));
    let joined = output.join("");

    assert_eq!(joined.matches("event: message_start").count(), 1);
    assert!(joined.contains("compaction_delta"));
    assert!(
        joined.find("compaction_delta").expect("compaction")
            < joined.find("text_delta").expect("text")
    );
}

#[test]
fn compaction_precedes_resumed_server_events() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let search = ClaudeToolSearchTrace {
        id: "srvtoolu_1".into(),
        name: "tool_search_tool_regex".into(),
        input: json!({"pattern":"github"}),
        references: vec!["mcp__github__list_issues".into()],
        error: None,
        requested_limit: 5,
        matched_count: 1,
        budget_truncated: false,
        emission: ClaudeServerToolEmission::ResultOnly,
    };
    let output = claude_initial_events(
        &mut state,
        Some("summary"),
        &[crate::http::response::ClaudeServerEvent::ToolSearch {
            index: 0,
            preceding_text: String::new(),
        }],
        &[search],
        &[],
    )
    .expect("encode initial events")
    .join("");

    assert!(
        output.find("compaction_delta").expect("compaction")
            < output
                .find("tool_search_tool_result")
                .expect("resumed result")
    );
}

#[test]
fn compaction_prelude_waits_for_the_first_semantic_event() {
    let mut pending = vec!["message_start".into(), "compaction".into()];

    assert!(prepend_pending_initial(&mut pending, Vec::new()).is_empty());
    assert_eq!(pending, ["message_start", "compaction"]);

    let output = prepend_pending_initial(&mut pending, vec!["text".into()]);
    assert_eq!(output, ["message_start", "compaction", "text"]);
    assert!(pending.is_empty());
}

#[test]
fn automatic_compaction_stats_are_emitted_in_the_final_message_delta() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 23_000, replay_codec());
    state.auto_compaction_original_input_tokens = Some(180_000);
    state.compaction_iteration = Some(CompactionIterationUsage {
        input_tokens: 180_500,
        output_tokens: 3_500,
    });
    let decoded = DecodedResponse {
        usage: crosslane_kiro::UsageInfo {
            input_tokens: 47_000,
            output_tokens: 900,
            ..crosslane_kiro::UsageInfo::default()
        },
        ..DecodedResponse::default()
    };
    let output = stream_finish(
        &StreamProtocol::Claude,
        &mut state,
        &decoded,
        0,
        "model",
        Some(100),
        0,
        ThinkingOutputFormat::Claude,
        false,
    )
    .join("");

    assert!(output.contains("\"reason\":\"model_mapping_overflow\""));
    assert!(output.contains("\"original_input_tokens\":180000"));
    assert!(output.contains("\"compacted_input_tokens\":23000"));
    assert!(output.contains("\"type\":\"compaction\""));
    assert!(output.contains("\"input_tokens\":180500"));
    let final_delta = output
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "message_delta")
        .expect("message delta");
    assert_eq!(
        final_delta["usage"]["iterations"][1]["input_tokens"],
        47_000
    );
}

#[test]
fn tool_search_stream_uses_server_blocks_and_references() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let output = state.tool_search(&ClaudeToolSearchTrace {
        id: "srvtoolu_1".into(),
        name: "tool_search_tool_regex".into(),
        input: json!({"pattern":"github"}),
        references: vec!["mcp__github__list_issues".into()],
        error: None,
        requested_limit: 5,
        matched_count: 1,
        budget_truncated: false,
        emission: ClaudeServerToolEmission::Complete,
    });
    let joined = output.join("");
    assert_eq!(joined.matches("event: message_start").count(), 1);
    assert!(joined.contains("server_tool_use"));
    assert!(joined.contains("tool_search_tool_result"));
    assert!(joined.contains("tool_reference"));
    assert!(joined.contains("mcp__github__list_issues"));
}

#[test]
fn web_search_stream_uses_native_server_result_blocks() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let trace = ClaudeWebSearchTrace::success(
        "srvtoolu_web".into(),
        "rust async",
        crosslane_translate::WebSearchResults {
            query: "rust async".into(),
            total_results: 1,
            results: vec![crosslane_translate::WebSearchResult {
                title: "Tokio".into(),
                url: "https://tokio.rs".into(),
                snippet: "runtime".into(),
                published_date: None,
            }],
        },
    );
    let mut output = state.web_search(&trace).expect("encrypt replay data");
    output.extend(state.event(&KiroEvent::AssistantResponse {
        content: "Tokio uses an async runtime: https://tokio.rs".into(),
    }));
    output.extend(
        state
            .citations(&[trace], "Tokio uses an async runtime: https://tokio.rs")
            .expect("encrypt citation data"),
    );
    let joined = output.join("");
    assert_eq!(joined.matches("event: message_start").count(), 1);
    assert!(joined.contains("server_tool_use"));
    assert!(joined.contains("web_search_tool_result"));
    assert!(joined.contains("https://tokio.rs"));
    assert!(!joined.contains("snippet"));
    assert!(joined.contains("citations_delta"));
    assert!(joined.contains("encrypted_index"));
}

#[test]
fn targetless_citations_are_visible_in_claude_and_openai_streams() {
    let event = KiroEvent::Citations {
        citations: vec![crosslane_kiro::KiroCitation {
            text: Some("Example source".into()),
            link: "https://example.com/source".into(),
            target: json!({}),
            kind: "web".into(),
        }],
    };

    let mut claude = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    let claude_output = claude.event(&event).join("");
    assert!(claude_output.contains("text_delta"));
    assert!(claude_output.contains("Example source"));
    assert!(claude_output.contains("https://example.com/source"));

    let mut openai = ClaudeState::new("chat_test".into(), "model".into(), 10, replay_codec());
    let openai_output = openai_event(
        &event,
        &mut openai,
        1,
        "model",
        ThinkingOutputFormat::Openai,
        &std::collections::HashMap::new(),
    )
    .join("");
    assert!(openai_output.contains("Example source"));
    assert!(openai_output.contains("https://example.com/source"));
}

#[test]
fn kiro_citation_stream_preserves_sources_without_inventing_document_ranges() {
    let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
    state.event(&KiroEvent::AssistantResponse {
        content: "answer".into(),
    });
    let output = state
        .event(&KiroEvent::Citations {
            citations: vec![crosslane_kiro::KiroCitation {
                text: Some("answer".into()),
                link: "https://example.com/source".into(),
                target: json!({"range":{"start":0,"end":6}}),
                kind: "document".into(),
            }],
        })
        .join("");
    assert!(output.contains("https://example.com/source"));
    assert!(!output.contains("citations_delta"));
    assert!(!output.contains("char_location"));
}

#[test]
fn stream_finish_honors_pause_turn_override() {
    let mut state = ClaudeState::new("msg_pause".into(), "model".into(), 10, replay_codec());
    let decoded = DecodedResponse {
        stop_reason: Some("pause_turn".into()),
        ..DecodedResponse::default()
    };
    let output = stream_finish(
        &StreamProtocol::Claude,
        &mut state,
        &decoded,
        0,
        "model",
        Some(100),
        0,
        ThinkingOutputFormat::Claude,
        false,
    )
    .join("");
    assert!(output.contains("\"stop_reason\":\"pause_turn\""));
}

#[test]
fn claude_stream_reports_stop_sequences_and_cache_usage() {
    let mut state = ClaudeState::new("msg_stop".into(), "model".into(), 100, replay_codec());
    state.cache_creation_input_tokens = 5;
    state.cache_read_input_tokens = 20;
    let mut filter = StopSequenceFilter::new(&["<END>".into()]);
    let first = client_visible_event(
        &KiroEvent::AssistantResponse {
            content: "hello <E".into(),
        },
        &mut filter,
    )
    .expect("visible prefix");
    assert_eq!(
        first,
        KiroEvent::AssistantResponse {
            content: "hello ".into()
        }
    );
    assert!(client_visible_event(
        &KiroEvent::AssistantResponse {
            content: "ND>ignored".into(),
        },
        &mut filter,
    )
    .is_none());

    let mut decoded = DecodedResponse {
        text: "hello <END>ignored".into(),
        usage: crosslane_kiro::UsageInfo {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 20,
            cache_write_tokens: 5,
            ..crosslane_kiro::UsageInfo::default()
        },
        ..DecodedResponse::default()
    };
    decoded.stop_at_sequence("hello ".into(), "<END>".into());
    let output = stream_finish(
        &StreamProtocol::Claude,
        &mut state,
        &decoded,
        123,
        "model",
        Some(100),
        10,
        ThinkingOutputFormat::Claude,
        false,
    );
    let values = streamed_values(&output);
    let start = values
        .iter()
        .find(|value| value["type"] == "message_start")
        .expect("message start");
    assert_eq!(start["message"]["usage"]["input_tokens"], 75);
    assert_eq!(start["message"]["usage"]["cache_creation_input_tokens"], 5);
    assert_eq!(start["message"]["usage"]["cache_read_input_tokens"], 20);
    let delta = values
        .iter()
        .find(|value| value["type"] == "message_delta")
        .expect("message delta");
    assert_eq!(delta["delta"]["stop_reason"], "stop_sequence");
    assert_eq!(delta["delta"]["stop_sequence"], "<END>");
    assert_eq!(delta["usage"]["input_tokens"], 75);
    assert_eq!(delta["usage"]["cache_creation_input_tokens"], 5);
    assert_eq!(delta["usage"]["cache_read_input_tokens"], 20);
}

#[test]
fn stream_stop_position_does_not_cross_non_text_boundaries() {
    let mut filter = StopSequenceFilter::new(&["END".into()]);
    assert_eq!(filter.push("E"), "");
    assert_eq!(filter.finish(), "E");
    assert_eq!(filter.push("ND"), "ND");

    let mut decoded = DecodedResponse {
        text: "END".into(),
        ..DecodedResponse::default()
    };
    apply_stream_stop(&mut decoded, &filter);
    assert_eq!(decoded.text, "END");
    assert!(decoded.stop_sequence.is_none());

    assert_eq!(filter.push(" END ignored"), " ");
    decoded.text.push_str(" END ignored");
    apply_stream_stop(&mut decoded, &filter);
    assert_eq!(decoded.text, "END ");
    assert_eq!(decoded.stop_sequence.as_deref(), Some("END"));
}

#[test]
fn openai_stream_reports_length_and_detailed_usage() {
    let mut state = ClaudeState::new(
        "chatcmpl-length".into(),
        "model".into(),
        100,
        replay_codec(),
    );
    let decoded = DecodedResponse {
        stop_reason: Some("max_tokens".into()),
        usage: crosslane_kiro::UsageInfo {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            reasoning_tokens: 7,
            ..crosslane_kiro::UsageInfo::default()
        },
        ..DecodedResponse::default()
    };
    let output = stream_finish(
        &StreamProtocol::OpenAi,
        &mut state,
        &decoded,
        123,
        "model",
        Some(50),
        50,
        ThinkingOutputFormat::Claude,
        true,
    );
    let values = streamed_values(&output);
    assert_eq!(values[0]["choices"][0]["finish_reason"], "length");
    assert_eq!(values[1]["choices"], json!([]));
    assert_eq!(
        values[1]["usage"]["prompt_tokens_details"]["cached_tokens"],
        20
    );
    assert_eq!(
        values[1]["usage"]["completion_tokens_details"]["reasoning_tokens"],
        7
    );
}

#[test]
fn web_search_stream_separates_pending_and_resumed_protocol_phases() {
    let mut pending_state =
        ClaudeState::new("msg_pending".into(), "model".into(), 10, replay_codec());
    let pending = pending_state
        .web_search(&ClaudeWebSearchTrace::pending(
            "srvtoolu_pending".into(),
            json!({"query":"rust"}),
        ))
        .expect("encode pending search")
        .join("");
    assert!(pending.contains("server_tool_use"));
    assert!(!pending.contains("web_search_tool_result"));

    let mut resumed_state =
        ClaudeState::new("msg_resumed".into(), "model".into(), 10, replay_codec());
    let resumed = resumed_state
        .web_search(
            &ClaudeWebSearchTrace::success(
                "srvtoolu_pending".into(),
                "rust",
                crosslane_translate::WebSearchResults::default(),
            )
            .result_only(),
        )
        .expect("encode resumed search")
        .join("");
    assert!(!resumed.contains("server_tool_use"));
    assert!(resumed.contains("web_search_tool_result"));
}

#[test]
fn finalized_mcp_tool_inputs_match_streaming_and_nonstreaming_output() {
    let name = "mcp__relayer__memory_list_editable_atoms";
    for protocol in [StreamProtocol::Claude, StreamProtocol::OpenAi] {
        for buffered in [false, true] {
            for stop in [false, true] {
                for input in [
                    "",
                    " \n\t",
                    "{}",
                    r#" {"raw":"preserve me","items":[1,2],"spaces":"  ","unicode":"空白"} "#,
                ] {
                    let mut decoded = DecodedResponse::default();
                    let mut state =
                        ClaudeState::new("request".into(), "model".into(), 0, replay_codec());
                    let identities = std::collections::HashMap::new();
                    let mut output = Vec::new();
                    let mut events = vec![KiroEvent::ToolUse {
                        id: "call".into(),
                        name: name.into(),
                        input_delta: String::new(),
                        stop: false,
                    }];
                    // Include whitespace-only fragments both before the JSON
                    // and inside a string, where they must not be discarded.
                    for fragment in input.chars().map(|ch| ch.to_string()) {
                        events.push(KiroEvent::ToolUse {
                            id: "call".into(),
                            name: name.into(),
                            input_delta: fragment,
                            stop: false,
                        });
                    }
                    if stop {
                        events.push(KiroEvent::ToolUse {
                            id: "call".into(),
                            name: name.into(),
                            input_delta: String::new(),
                            stop: true,
                        });
                    }
                    for event in events {
                        if !buffered {
                            output.extend(stream_event(
                                &protocol,
                                &mut state,
                                &event,
                                1,
                                "model",
                                ThinkingOutputFormat::Openai,
                                &identities,
                            ));
                        }
                        decoded.push(event).unwrap();
                    }
                    decoded.finalize_tool_inputs().unwrap();
                    if buffered {
                        let tool = &decoded.tools["call"];
                        output.extend(stream_event(
                            &protocol,
                            &mut state,
                            &KiroEvent::ToolUse {
                                id: tool.id.clone(),
                                name: tool.name.clone(),
                                input_delta: tool.input.clone(),
                                stop: true,
                            },
                            1,
                            "model",
                            ThinkingOutputFormat::Openai,
                            &identities,
                        ));
                    }
                    output.extend(stream_finish(
                        &protocol,
                        &mut state,
                        &decoded,
                        1,
                        "model",
                        Some(256),
                        0,
                        ThinkingOutputFormat::Openai,
                        true,
                    ));
                    let values = streamed_values(&output);
                    let (arguments, nonstream_input) = match &protocol {
                        StreamProtocol::Claude => {
                            let arguments = values
                                .iter()
                                .filter_map(|event| {
                                    event.pointer("/delta/partial_json").and_then(Value::as_str)
                                })
                                .collect::<String>();
                            let response = decoded.claude_json(
                                "request",
                                "model",
                                256,
                                0,
                                None,
                                &replay_codec(),
                            );
                            (arguments, response["content"][0]["input"].clone())
                        }
                        StreamProtocol::OpenAi => {
                            let arguments = values
                                .iter()
                                .filter_map(|event| {
                                    event
                                        .pointer("/choices/0/delta/tool_calls")
                                        .and_then(Value::as_array)
                                })
                                .flatten()
                                .filter_map(|tool| {
                                    tool.pointer("/function/arguments").and_then(Value::as_str)
                                })
                                .collect::<String>();
                            let response = decoded.openai_json(
                                "request",
                                "model",
                                1,
                                Some(256),
                                0,
                                ThinkingOutputFormat::Openai,
                                &identities,
                            );
                            let input = serde_json::from_str(
                                response["choices"][0]["message"]["tool_calls"][0]["function"]
                                    ["arguments"]
                                    .as_str()
                                    .unwrap(),
                            )
                            .unwrap();
                            (arguments, input)
                        }
                    };
                    let streamed_input: Value = if matches!(protocol, StreamProtocol::Claude)
                        && arguments.is_empty()
                    {
                        // Anthropic initializes the tool block with input: {}.
                        json!({})
                    } else {
                        serde_json::from_str(&arguments).unwrap_or_else(|error| panic!("buffered={buffered}, stop={stop}, input={input:?}, arguments={arguments:?}: {error}"))
                    };
                    let expected: Value =
                        serde_json::from_str(&decoded.tools["call"].input).unwrap();
                    assert_eq!(
                        streamed_input, expected,
                        "buffered={buffered}, stop={stop}, input={input:?}"
                    );
                    assert_eq!(nonstream_input, expected);
                }
            }
        }
    }
}

#[test]
fn openai_tool_chunks_keep_request_id_and_stable_per_tool_indices() {
    let mut state = ClaudeState::new(
        "chatcmpl-request".into(),
        "model".into(),
        10,
        replay_codec(),
    );
    let first = openai_event(
        &KiroEvent::ToolUse {
            id: "tool-a".into(),
            name: "one".into(),
            input_delta: "{".into(),
            stop: false,
        },
        &mut state,
        123,
        "model",
        ThinkingOutputFormat::Openai,
        &std::collections::HashMap::new(),
    );
    let second = openai_event(
        &KiroEvent::ToolUse {
            id: "tool-b".into(),
            name: "two".into(),
            input_delta: "{}".into(),
            stop: true,
        },
        &mut state,
        123,
        "model",
        ThinkingOutputFormat::Openai,
        &std::collections::HashMap::new(),
    );
    let third = openai_event(
        &KiroEvent::ToolUse {
            id: "tool-a".into(),
            name: "one".into(),
            input_delta: "}".into(),
            stop: true,
        },
        &mut state,
        123,
        "model",
        ThinkingOutputFormat::Openai,
        &std::collections::HashMap::new(),
    );
    assert!(first[0].contains("\"id\":\"chatcmpl-request\""));
    assert!(first[0].contains("\"index\":0"));
    assert!(second[0].contains("\"index\":1"));
    assert!(third[0].contains("\"index\":0"));
    let finished = stream_finish(
        &StreamProtocol::OpenAi,
        &mut state,
        &DecodedResponse::default(),
        123,
        "model",
        Some(100),
        1,
        ThinkingOutputFormat::Openai,
        false,
    );
    assert!(finished[0].contains("\"id\":\"chatcmpl-request\""));
    assert!(!finished[0].contains("\"usage\""));

    let with_usage = stream_finish(
        &StreamProtocol::OpenAi,
        &mut state,
        &DecodedResponse::default(),
        123,
        "model",
        Some(100),
        1,
        ThinkingOutputFormat::Openai,
        true,
    );
    assert_eq!(with_usage.len(), 3);
    assert!(with_usage[1].contains("\"choices\":[]"));
    assert!(with_usage[1].contains("\"usage\""));
}
