use super::*;
use serde_json::json;

fn parse(body: Value) -> ClientContext {
    extract(&HeaderMap::new(), &serde_json::to_vec(&body).unwrap())
}

fn claude(device: &str, session: &str) -> Value {
    json!({"metadata":{"user_id":json!({
        "device_id":device,"account_uuid":"","session_id":session
    }).to_string()}})
}

fn codex(installation: &str, session: &str, thread: &str) -> Value {
    json!({"client_metadata":{
        "x-codex-installation-id":installation,"session_id":session,"thread_id":thread
    }})
}

#[test]
fn claude_old_and_new_formats_group_devices_independently_of_sessions_and_accounts() {
    let device = "a".repeat(64);
    let old = parse(json!({"metadata":{"user_id":format!(
        "user_{device}_account_account-a_session_session-a"
    )}}));
    let new = parse(claude(&device, "session-a"));
    assert_eq!(old, new);
    let old_with_separator = parse(json!({"metadata":{"user_id":format!(
        "user_{device}_account_account-a_session_session_a_session_b"
    )}}));
    assert_eq!(
        old_with_separator,
        parse(claude(&device, "session_a_session_b"))
    );
    let next = parse(claude(&device, "session-b"));
    assert_eq!(new.user_ref, next.user_ref);
    assert_ne!(new.session_ref, next.session_ref);
    assert_ne!(
        new.user_ref,
        parse(claude(&"b".repeat(64), "session-a")).user_ref
    );
    let encoded = serde_json::to_string(&new).unwrap();
    assert!(!encoded.contains(&device) && !encoded.contains("session-a"));
}

#[test]
fn codex_body_identity_has_precedence_and_threads_are_distinct_from_session() {
    let body = codex("install-a", "root-a", "fork-a");
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-installation-id", "header-install".parse().unwrap());
    headers.insert("session-id", "header-session".parse().unwrap());
    headers.insert("thread-id", "header-thread".parse().unwrap());
    let first = extract(&headers, &serde_json::to_vec(&body).unwrap());
    assert_eq!(first, parse(body));
    let fork = parse(codex("install-a", "root-a", "fork-b"));
    assert_eq!(first.user_ref, fork.user_ref);
    assert_eq!(first.session_ref, fork.session_ref);
    assert_ne!(first.thread_ref, fork.thread_ref);
    let next = parse(codex("install-a", "root-b", "root-b"));
    assert_eq!(first.user_ref, next.user_ref);
    assert_ne!(first.session_ref, next.session_ref);
}

#[test]
fn missing_user_identity_stays_unknown_and_request_ids_are_not_sessions() {
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", "request-a".parse().unwrap());
    headers.insert("x-client-request-id", "request-b".parse().unwrap());
    headers.insert("x-session-affinity", "routing-hint".parse().unwrap());
    assert!(extract(&headers, b"{}").is_unknown());
    headers.insert("session-id", "root-a".parse().unwrap());
    headers.insert("thread-id", "thread-a".parse().unwrap());
    let context = extract(&headers, b"{}");
    assert!(context.user_ref.is_none());
    assert!(context.session_ref.is_some() && context.thread_ref.is_some());
    assert!(parse(json!({"metadata":{"user_id":"arbitrary-user"}})).is_unknown());
    assert!(
        parse(json!({"user":"name","safety_identifier":"id","prompt_cache_key":"cache"}))
            .is_unknown()
    );
}

#[test]
fn bad_or_oversized_telemetry_falls_back_without_rejecting_requests() {
    let mut headers = HeaderMap::new();
    headers.insert("session-id", "fallback".parse().unwrap());
    headers.insert("x-codex-installation-id", "install-a".parse().unwrap());
    let fallback = extract(&headers, b"{}");
    for body in [
        json!({"metadata":3,"client_metadata":false}),
        json!({"metadata":{"user_id":"{broken"}}),
        json!({"client_metadata":{"x-codex-installation-id":"x".repeat(257)}}),
        json!({"metadata":{"user_id":"x".repeat(MAX_USER_ID_BYTES + 1)}}),
        json!({"client_metadata":{"padding":"x".repeat(MAX_METADATA_BYTES + 1)}}),
    ] {
        assert_eq!(
            extract(&headers, &serde_json::to_vec(&body).unwrap()),
            fallback
        );
    }
    assert_eq!(extract(&headers, b"invalid json"), fallback);
    assert!(parse(claude("device\ninvalid", "session-a"))
        .user_ref
        .is_none());
}

#[test]
fn verified_continuations_inherit_omissions_and_preserve_explicit_forks() {
    let previous = parse(codex("install-a", "root-a", "thread-a"));
    let mut omitted = ClientContext::default();
    omitted.inherit(&previous).unwrap();
    assert_eq!(omitted, previous);
    let mut headers = HeaderMap::new();
    headers.insert("thread-id", "thread-a".parse().unwrap());
    let mut header_only = extract(&headers, b"{}");
    assert!(!header_only.forks(&previous));
    header_only.inherit(&previous).unwrap();
    assert_eq!(header_only, previous);
    let mut fork = parse(codex("install-a", "root-a", "thread-b"));
    let expected = fork.clone();
    assert!(fork.forks(&previous));
    fork.inherit(&previous).unwrap();
    assert_eq!(fork, expected);
    let mut other = parse(codex("install-b", "root-a", "thread-a"));
    assert!(other.inherit(&previous).is_err());
}

#[test]
fn continuation_controls_are_bounded_and_store_opt_out_is_respected() {
    assert_eq!(
        previous_response_id(br#"{"previous_response_id":"resp_a"}"#).as_deref(),
        Some("resp_a")
    );
    assert!(previous_response_id(
        &serde_json::to_vec(&json!({"previous_response_id":"x".repeat(257)})).unwrap()
    )
    .is_none());
    assert!(!stores_response(br#"{"store":false}"#));
    assert!(stores_response(br#"{"store":true}"#));
    assert!(stores_response(b"{}"));
}
