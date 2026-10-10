use super::*;
use axum::body::to_bytes;
use crosslane_translate::responses_to_openai;

fn options_from_body(body: Value) -> ResponsesOptions {
    let request: ResponsesRequest = serde_json::from_value(body).unwrap();
    let translated = responses_to_openai(&request).unwrap();
    ResponsesOptions::new(
        &request,
        translated.tools,
        translated.tool_names,
        None,
        None,
    )
}

fn options() -> ResponsesOptions {
    options_from_body(json!({
        "model":"claude-sonnet-4.5","input":"hello","store":false,
        "tools":[{"type":"namespace","name":"functions","tools":[
            {"type":"function","name":"read_file","parameters":{"type":"object"}},
            {"type":"custom","name":"apply_patch"}
        ]}]
    }))
}

fn stored_options(
    input: String,
    store_control: Option<bool>,
) -> (
    ResponsesOptions,
    ResponsesSessionStore,
    ResponsesSessionOwner,
) {
    let mut body = json!({
        "model":"claude-sonnet-4.5",
        "input":input
    });
    if let Some(store) = store_control {
        body["store"] = json!(store);
    }
    let request: ResponsesRequest = serde_json::from_value(body).unwrap();
    let translated = responses_to_openai(&request).unwrap();
    let owner = ResponsesSessionOwner::new("service-a".into(), Some("key-a".into()));
    let store = ResponsesSessionStore::default();
    let mut options = ResponsesOptions::new(
        &request,
        translated.tools,
        translated.tool_names,
        Some((store.clone(), owner.clone())),
        None,
    );
    options.set_conversation_id(Some("resolved-conversation-id".into()));
    (options, store, owner)
}

fn completed_chat(text: &str) -> Value {
    json!({
        "choices":[{"finish_reason":"stop","message":{"content":text}}],
        "usage":usage_chunk()["usage"]
    })
}

#[test]
fn response_client_attribution_is_owner_provider_scoped_expiring_and_bounded() {
    let store = ResponseClientContexts::default();
    let owner = ResponsesSessionOwner::new("service-a".into(), Some("key-a".into()));
    let context = ClientContext {
        user_ref: Some("hashed-device".into()),
        ..Default::default()
    };
    store.insert(
        owner.clone(),
        "copilot".into(),
        "resp_1".into(),
        context.clone(),
    );
    assert_eq!(
        store.get(&owner, "copilot", "resp_1"),
        Some(context.clone())
    );
    assert!(store.get(&owner, "other-provider", "resp_1").is_none());
    for other in [
        ResponsesSessionOwner::new("service-b".into(), Some("key-a".into())),
        ResponsesSessionOwner::new("service-a".into(), Some("key-b".into())),
    ] {
        assert!(store.get(&other, "copilot", "resp_1").is_none());
    }
    for index in 0..=MAX_RESPONSES_SESSIONS {
        store.insert(
            owner.clone(),
            "copilot".into(),
            format!("resp_{index}"),
            context.clone(),
        );
    }
    assert_eq!(store.entries.lock().unwrap().len(), MAX_RESPONSES_SESSIONS);
    for (_, seen) in store.entries.lock().unwrap().values_mut() {
        *seen = Instant::now() - RESPONSES_SESSION_TTL;
    }
    assert!(store.get(&owner, "copilot", "resp_256").is_none());
    assert!(store.entries.lock().unwrap().is_empty());
}

#[test]
fn stored_sessions_are_credential_scoped_and_do_not_replay_old_instructions() {
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4.5",
        "instructions":"Old instructions must not persist.",
        "input":"Read the file.",
        "store":true,
        "parallel_tool_calls":false,
        "tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}],
        "tool_choice":{"type":"function","name":"read_file"}
    }))
    .unwrap();
    let owner = ResponsesSessionOwner::new("service-a".into(), Some("key-a".into()));
    let store = ResponsesSessionStore::default();
    assert!(store.insert(
        "resp_1",
        StoredResponsesSession::from_request(
            &request,
            request.tools.clone().unwrap_or_default(),
            owner.clone(),
            Some("response-affinity".into()),
        ),
        &[json!({
            "type":"function_call","call_id":"call_read","name":"read_file","arguments":"{}"
        })],
    ));
    assert!(store
        .get(
            "resp_1",
            &ResponsesSessionOwner::new("service-a".into(), Some("other-key".into())),
        )
        .is_none());
    assert!(store
        .get(
            "resp_1",
            &ResponsesSessionOwner::new("service-b".into(), Some("key-a".into())),
        )
        .is_none());

    let mut continuation: ResponsesRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4.5",
        "instructions":"Replacement instructions.",
        "previous_response_id":"resp_1",
        "input":[{
            "type":"function_call_output","call_id":"call_read","output":"README contents"
        }]
    }))
    .unwrap();
    let session = store.get("resp_1", &owner).expect("owned session");
    resume_responses_request(&mut continuation, session);

    assert_eq!(continuation.input[0]["role"], "user");
    assert_eq!(continuation.input[0]["content"], "Read the file.");
    assert_eq!(continuation.input[1]["type"], "function_call");
    assert_eq!(continuation.input[2]["type"], "function_call_output");
    assert_eq!(continuation.tools.as_ref().unwrap().len(), 1);
    assert_eq!(continuation.parallel_tool_calls, Some(false));
    assert_eq!(
        continuation.tool_choice,
        Some(json!({"type":"function","name":"read_file"}))
    );

    let translated = responses_to_openai(&continuation).expect("combined tool roundtrip");
    assert_eq!(
        translated.request.messages[0].content,
        Some(json!("Replacement instructions."))
    );
    assert!(!translated
        .request
        .messages
        .iter()
        .any(|message| message.content == Some(json!("Old instructions must not persist."))));
}

#[test]
fn additional_tools_are_stored_as_controls_and_inherited_canonically() {
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"gpt-5.6-sol",
        "input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"custom","name":"exec"},
                {"type":"function","name":"wait","parameters":{"type":"object"}}
            ]},
            {"type":"message","role":"user","content":"Run a command."}
        ]
    }))
    .unwrap();
    let translated = responses_to_openai(&request).expect("Responses Lite tools");
    let owner = ResponsesSessionOwner::new("service-a".into(), Some("key-a".into()));
    let store = ResponsesSessionStore::default();
    let mut options = ResponsesOptions::new(
        &request,
        translated.tools,
        translated.tool_names,
        Some((store.clone(), owner.clone())),
        None,
    );
    options.set_conversation_id(Some("response-affinity".into()));
    let response_id = options.id().to_owned();
    json_response(completed_chat("ready"), options).expect("stored response");

    let session = store.get(&response_id, &owner).expect("stored session");
    assert_eq!(session.history.len(), 2);
    assert!(session
        .history
        .iter()
        .all(|item| item["type"] != "additional_tools"));
    assert_eq!(session.tools.len(), 2);

    let mut continuation: ResponsesRequest = serde_json::from_value(json!({
        "model":"gpt-5.6-sol",
        "previous_response_id":response_id,
        "input":"Continue."
    }))
    .unwrap();
    resume_responses_request(&mut continuation, session);
    assert_eq!(continuation.tools.as_ref().map(Vec::len), Some(2));
    assert!(continuation
        .input
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["type"] != "additional_tools"));
    assert_eq!(
        responses_to_openai(&continuation)
            .expect("inherited Responses Lite tools")
            .request
            .tools
            .len(),
        2
    );
}

#[test]
fn a_new_additional_tools_catalog_replaces_inherited_tools() {
    let parent: ResponsesRequest = serde_json::from_value(json!({
        "model":"test","input":"hello",
        "tools":[{"type":"function","name":"old_tool"}]
    }))
    .unwrap();
    let owner = ResponsesSessionOwner::new("service-a".into(), None);
    let session = StoredResponsesSession::from_request(
        &parent,
        parent.tools.clone().unwrap(),
        owner,
        Some("response-affinity".into()),
    );
    let mut continuation: ResponsesRequest = serde_json::from_value(json!({
        "model":"test",
        "input":[
            {"type":"additional_tools","tools":[{"type":"function","name":"new_tool"}]},
            {"role":"user","content":"continue"}
        ]
    }))
    .unwrap();
    resume_responses_request(&mut continuation, session);
    assert!(continuation.tools.is_none());
    let translated = responses_to_openai(&continuation).unwrap();
    assert_eq!(translated.tools.len(), 1);
    assert_eq!(
        translated.request.tools[0].body["function"]["name"],
        "new_tool"
    );
}

#[test]
fn responses_store_by_default_and_explicit_false_opts_out() {
    let (default_options, store, owner) = stored_options("hello".into(), None);
    let response_id = default_options.id().to_owned();
    let response = json_response(completed_chat("stored"), default_options)
        .expect("default response should be stored");
    assert_eq!(response["store"], true);
    let session = store
        .get(&response_id, &owner)
        .expect("default response session");
    assert_eq!(session.conversation_id(), Some("resolved-conversation-id"));

    let (stateless_options, store, owner) = stored_options("hello".into(), Some(false));
    let response_id = stateless_options.id().to_owned();
    let response = json_response(completed_chat("not stored"), stateless_options)
        .expect("stateless response should still complete");
    assert_eq!(response["store"], false);
    assert!(store.get(&response_id, &owner).is_none());
}

#[test]
fn storage_overflow_fails_instead_of_completing_an_unstored_response() {
    let oversized = "x".repeat(MAX_RESPONSES_SESSION_BYTES + 1);
    let (options, _, _) = stored_options(oversized.clone(), Some(true));
    assert_eq!(
        json_response(completed_chat("too large"), options),
        Err(RESPONSES_STORE_FAILED)
    );

    let (options, _, _) = stored_options(oversized, Some(true));
    let mut stream = ResponsesStream::new(options);
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"content":"too large"}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    assert_eq!(events[0]["response"]["store"], true);
    let failed = events.last().unwrap();
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["store"], false);
    assert_eq!(failed["response"]["error"]["code"], "response_store_failed");
    assert!(!events.iter().any(|event| matches!(
        event["type"].as_str(),
        Some("response.completed" | "response.incomplete")
    )));
}

fn chunk(delta: Value) -> Value {
    json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
}

fn usage_chunk() -> Value {
    json!({"choices":[],"usage":{
        "prompt_tokens":31,"completion_tokens":12,"total_tokens":43,
        "prompt_tokens_details":{"cached_tokens":8},"completion_tokens_details":{"reasoning_tokens":5}
    }})
}

fn events(frames: &[String]) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .map(|data| {
                    let value: Value = serde_json::from_str(data).expect("Responses event JSON");
                    assert!(
                        frame.starts_with(&format!("event: {}\n", value["type"].as_str().unwrap()))
                    );
                    value
                })
        })
        .collect()
}

#[test]
fn mixed_stream_has_stable_ids_order_complete_items_and_usage() {
    let mut stream = ResponsesStream::new(options());
    let id = stream.options.id().to_owned();
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"reasoning_content":"Read "}))));
    frames.extend(stream.chunk(chunk(json!({"reasoning_content":"first."}))));
    frames.extend(stream.chunk(chunk(json!({"content":"Checking 文件."}))));
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[
        {"index":0,"id":"call_read","type":"function","function":{"name":"functions.read_file","arguments":"{\"path\":"}}
    ]}))));
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[
        {"index":1,"id":"call_patch","type":"custom","custom":{"name":"functions.apply_patch","input":"*** Begin Patch\n"}}
    ]}))));
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[
        {"index":0,"function":{"arguments":"\"README.md\"}"}},
        {"index":1,"custom":{"input":"*** End Patch"}}
    ]}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence as u64);
    }
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[0]["response"]["id"], id);
    assert_eq!(events[0]["response"]["status"], "in_progress");
    let complete = events.last().unwrap();
    assert_eq!(complete["type"], "response.completed");
    let response = &complete["response"];
    assert_eq!(response["id"], id);
    assert_eq!(response["status"], "completed");
    assert_eq!(response["usage"]["input_tokens"], 31);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        8
    );
    assert_eq!(
        response["usage"]["output_tokens_details"]["reasoning_tokens"],
        5
    );
    let output = response["output"].as_array().unwrap();
    assert_eq!(output.len(), 4);
    assert_eq!(output[0]["summary"][0]["text"], "Read first.");
    assert_eq!(output[1]["content"][0]["text"], "Checking 文件.");
    assert_eq!(output[2]["call_id"], "call_read");
    assert_eq!(output[2]["name"], "read_file");
    assert_eq!(output[2]["namespace"], "functions");
    assert_eq!(output[2]["arguments"], "{\"path\":\"README.md\"}");
    assert_eq!(output[3]["type"], "custom_tool_call");
    assert_eq!(output[3]["input"], "*** Begin Patch\n*** End Patch");
    for (index, item) in output.iter().enumerate() {
        let added = events
            .iter()
            .find(|e| e["type"] == "response.output_item.added" && e["output_index"] == index)
            .unwrap();
        let done = events
            .iter()
            .find(|e| e["type"] == "response.output_item.done" && e["output_index"] == index)
            .unwrap();
        assert_eq!(added["item"]["id"], item["id"]);
        assert_eq!(done["item"], *item);
    }
    let summary_done = events
        .iter()
        .position(|e| e["type"] == "response.reasoning_summary_text.done")
        .unwrap();
    let text_delta = events
        .iter()
        .position(|e| e["type"] == "response.output_text.delta")
        .unwrap();
    assert!(summary_done < text_delta);
    assert!(stream.finish().is_empty());
    assert!(stream.fail("late", "late error").is_empty());
}

#[test]
fn stream_seeds_complete_reasoning_text_for_codex_rendering() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    assert!(stream
        .chunk(chunk(json!({"reasoning_content":"..."})))
        .is_empty());
    assert!(stream
        .chunk(chunk(json!({"reasoning_content":"Read "})))
        .is_empty());
    assert!(stream
        .chunk(chunk(json!({"reasoning_content":"first."})))
        .is_empty());
    frames.extend(stream.chunk(chunk(json!({"content":"Done."}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());

    let events = events(&frames);
    let added = events
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.added" && event["item"]["type"] == "reasoning"
        })
        .expect("reasoning item should be published");
    assert_eq!(added["item"]["summary"][0]["text"], "Read first.");
    let part = events
        .iter()
        .find(|event| event["type"] == "response.reasoning_summary_part.added")
        .expect("reasoning summary part should be published");
    assert_eq!(part["part"]["text"], "Read first.");
    assert!(!events
        .iter()
        .any(|event| event["type"] == "response.reasoning_summary_text.delta"));
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["summary"][0]["text"],
        "Read first."
    );
}

#[test]
fn hollow_reasoning_markers_do_not_create_output_items() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    for marker in [" \n", "...", "…", "<!-- -->"] {
        assert!(stream
            .chunk(chunk(json!({"reasoning_content":marker})))
            .is_empty());
    }
    frames.extend(stream.chunk(chunk(json!({"content":"Visible answer."}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    assert!(!events.iter().any(|event| {
        event["type"] == "response.output_item.added" && event["item"]["type"] == "reasoning"
    }));
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "Visible answer."
    );

    let response = json_response(
        json!({"choices":[{"finish_reason":"stop","message":{
            "reasoning_content":"<!-- -->\n…", "content":"Visible answer."
        }}],"usage":usage_chunk()["usage"]}),
        options(),
    )
    .expect("response should be encoded");
    assert_eq!(response["output"].as_array().unwrap().len(), 1);
    assert_eq!(response["output"][0]["type"], "message");
}

#[test]
fn explicit_none_suppresses_reasoning_in_both_modes() {
    let options_with_none = || {
        options_from_body(json!({
            "model":"claude-sonnet-4.5", "input":"hello", "store":false,
            "reasoning":{"effort":"high","summary":"none"}
        }))
    };
    let mut stream = ResponsesStream::new(options_with_none());
    let mut frames = stream.start();
    assert!(stream
        .chunk(chunk(json!({"reasoning_content":"Hidden summary."})))
        .is_empty());
    frames.extend(stream.chunk(chunk(json!({"content":"Visible answer."}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    assert!(!events.iter().any(|event| {
        event["type"] == "response.output_item.added" && event["item"]["type"] == "reasoning"
    }));

    let response = json_response(
        json!({"choices":[{"finish_reason":"stop","message":{
            "reasoning_content":"Hidden summary.", "content":"Visible answer."
        }}],"usage":usage_chunk()["usage"]}),
        options_with_none(),
    )
    .expect("response should be encoded");
    assert_eq!(response["output"].as_array().unwrap().len(), 1);
    assert_eq!(response["output"][0]["type"], "message");
}

#[test]
fn nonstream_response_preserves_text_alongside_tools_and_reasoning() {
    let chat = json!({"choices":[{"finish_reason":"tool_calls","message":{
        "content":"Checking.","reasoning_content":"Read first.","tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"functions.read_file","arguments":"{}"}},
            {"id":"call_2","type":"custom","custom":{"name":"functions.apply_patch","input":"a patch"}}
        ]}}],"usage":usage_chunk()["usage"]});
    let response = json_response(chat, options()).expect("response should be encoded");
    let output = response["output"].as_array().unwrap();
    assert_eq!(output.len(), 4);
    assert_eq!(output[1]["content"][0]["text"], "Checking.");
    assert_eq!(output[2]["call_id"], "call_1");
    assert_eq!(output[3]["input"], "a patch");
    assert_eq!(response["object"], "response");
    assert_eq!(response["usage"]["total_tokens"], 43);
    assert_eq!(response["store"], false);
    assert!(response.get("choices").is_none());
}

#[test]
fn output_limit_is_incomplete_in_both_modes() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"content":"partial"}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"length"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.incomplete");
    assert_eq!(
        last["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    let response = json_response(
        json!({"choices":[{"finish_reason":"length","message":{"content":"partial"}}],"usage":usage_chunk()["usage"]}),
        options(),
    )
    .expect("response should be encoded");
    assert_eq!(response["status"], "incomplete");
    assert_eq!(response["output"][0]["status"], "incomplete");
}

#[test]
fn failed_stream_never_completes_a_partial_tool_call() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"functions.read_file","arguments":"{"}}]}))));
    frames.extend(stream.chunk(json!({"error":{"type":"server_error","code":"upstream_timeout","message":"upstream timed out"}})));
    frames.extend(stream.finish());
    let events = events(&frames);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.failed");
    assert_eq!(last["response"]["error"]["code"], "upstream_timeout");
    assert_eq!(last["response"]["output"][0]["status"], "incomplete");
    assert!(!events.iter().any(|e| matches!(
        e["type"].as_str(),
        Some("response.completed" | "response.output_item.done")
    )));
}

#[test]
fn incomplete_response_keeps_the_status_of_already_finished_items() {
    let mut stream = ResponsesStream::new(options());
    let mut frames = stream.start();
    frames.extend(stream.chunk(chunk(json!({"content":"An initial observation."}))));
    frames.extend(stream.chunk(chunk(json!({"reasoning_content":"Checking the details."}))));
    frames.extend(stream.chunk(chunk(json!({"content":"A partial answer"}))));
    frames.extend(stream.chunk(json!({"choices":[{"delta":{},"finish_reason":"length"}]})));
    frames.extend(stream.chunk(usage_chunk()));
    frames.extend(stream.finish());
    let events = events(&frames);
    let response = &events.last().unwrap()["response"];
    assert_eq!(response["status"], "incomplete");
    assert_eq!(response["output"][0]["status"], "completed");
    assert_eq!(response["output"][2]["status"], "incomplete");
    for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        let done = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done" && event["output_index"] == index
            })
            .unwrap();
        assert_eq!(done["item"], *item);
    }
}

#[tokio::test]
async fn adapter_handles_fragmented_utf8_crlf_multiple_frames_and_keepalives() {
    let wire = format!(
        ": ping\r\n\r\ndata: {}\r\n\r\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"content":"你好 🦀"})),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        usage_chunk()
    );
    // Split inside every multibyte code point and every SSE separator.
    let pieces = wire
        .as_bytes()
        .chunks(1)
        .map(|piece| Ok::<_, Infallible>(Bytes::copy_from_slice(piece)))
        .collect::<Vec<_>>();
    let response = stream_response(
        Response::new(Body::from_stream(futures::stream::iter(pieces))),
        options(),
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let output = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(output.contains(": keepalive\n\n"));
    assert!(!output.contains("[DONE]"));
    let frames = output
        .split("\n\n")
        .filter(|frame| frame.starts_with("event:"))
        .map(|frame| format!("{frame}\n\n"))
        .collect::<Vec<_>>();
    let events = events(&frames);
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "你好 🦀"
    );
}

#[tokio::test]
async fn adapter_reports_truncated_and_invalid_streams_as_failed() {
    for wire in [
        format!("data: {}\n\n", chunk(json!({"content":"partial"}))),
        "data: {bad json}\n\n".into(),
        "data: [DONE]\n\n".into(),
        "data: {\"choices\":".into(),
    ] {
        let response = stream_response(Response::new(Body::from(wire)), options());
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("event: response.failed\n"));
        assert!(!body.contains("event: response.completed\n"));
    }
}

#[tokio::test]
async fn hosted_failure_retains_already_delivered_partial_text() {
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let operation = async move {
        for event in [
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"partial","type":"message","role":"assistant","status":"in_progress","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"partial","part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"partial","delta":"A partial answer"}),
        ] {
            sender.send(event).await.unwrap();
        }
        Err((
            "unsupported_tool_protocol".into(),
            "web_search is not supported: continuation interrupted".into(),
        ))
    };
    let response = hosted_stream(operation, receiver, options());
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let terminal: Value = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "response.failed")
        .unwrap();
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "A partial answer"
    );
    assert_eq!(terminal["response"]["output"][0]["status"], "incomplete");
    assert!(!body.contains("response.completed"));
}
