# Client compatibility

[English](client-compatibility.md) | [简体中文](client-compatibility.zh-CN.md) | [Project overview](../README.md)

This guide covers tools, web search, conversation attribution, CRS transport, and Kiro
upstream refusals. For listeners and access controls, see [operations](startup-and-debugging.md#services-and-client-access);
for caching, redaction, and compaction, see [request processing](request-processing.md).

[Tool protocols](#tool-protocols) · [Web search](#web-search) · [Attribution](#anonymous-client-and-conversation-attribution) · [CRS](#claude-relay-service-response-compatibility) · [Kiro endpoints and refusals](#kiro-endpoints-and-upstream-refusals)

## Tool protocols

Copilot Responses streams retain the first upstream response ID and one item ID
per output index across deltas, completion events, and the final output snapshot.
This lets desktop clients reconcile streamed text with completed messages without
showing the answer twice when Copilot changes its opaque IDs between events.
Message phases, tool call IDs, citations, and `copilot_usage` remain unchanged.

For Claude upstreams that reject root `oneOf`, `allOf`, or `anyOf` tool schemas,
Crosslane moves those constraints into an equivalent conditional subschema,
preserving required branches, local definitions, and closed object constraints.
Responses namespace function/custom tools use unique upstream names and restore their
original namespace and name in client output. The adapter bounds namespace and
leaf name independently to 128 characters. If their flattened internal name is
longer, it uses a request-scoped alias consistently in declarations, history and
tool choice; streamed and non-streamed output retain the original fields.
Claude Code's `thinking.display: "updates"` is adapted to upstream `summarized`
while retaining thinking settings;
`omitted` continues to hide thinking output.
Kiro tool calls are checked against the originating request's tool declarations
before reaching the client, in both streaming and non-streaming responses.
Known aliases restore the original name; a hash-looking name explicitly declared
by the client remains valid. Unknown names fail with `upstream_tool_name_mismatch`
(HTTP 502 for non-streaming, an error event for streaming), without rotating
accounts or emitting the unknown tool call. The `proxy.tool_name.mapping_miss`
log records the trace/request IDs, call ID and a bounded tool name, without
tool arguments or schemas. Proxy-managed Tool Search/Web Search retain their
server-tool handling.
Copilot Messages requests omit the unsupported Anthropic `safeguards` extension,
including on token-counting requests, so newer Claude Code requests are accepted.

## Web search

Copilot handles ordinary generation even when a Responses catalog advertises
`web_search`. An actual search call switches both search and the turn's following
generation to Kiro, with no Copilot fallback. Claude Code's server-side WebSearch
subrequests also take this route. Add to the existing Copilot `[[proxy_service]]`:

```toml
web_search_provider = "kiro" # Kiro provider instance ID
```

This grants only search and its continuation. Service account tags, explicit
account inclusions/exclusions, and client model permissions still apply. Kiro
services default to their own provider when this field is omitted. Run
`crosslane config reload` after editing. Missing Kiro configuration/accounts,
search failures, and failed Kiro continuations report `web_search is not supported`.

Responses include `web_search_call`, real source links and URL citations, with
streaming events and replayable tool history. `external_web_access:false` uses
only the service/key's five-minute search cache and reports a cache miss. Codex
clients needing live search can set `web_search = "live"`. Search replay is scoped
to the same service/key, lasts at most thirty minutes, and expires on restart.
Claude server-side search supports either `allowed_domains` or `blocked_domains`,
but not both. Domains include their subdomains; search paths support prefixes and
`*` wildcards. Use ASCII host names without schemes, ports, credentials, or host
wildcards. The proxy adds `site:` / `-site:` query constraints and filters returned
URLs before model continuation, citations, and encrypted replay. No matches yield
an empty search result. `user_location` is still rejected explicitly.

## Anonymous client and conversation attribution

Client context is available in every build, independently of the `crs` feature.
Claude Code's old and JSON-encoded `metadata.user_id` formats provide a device ID
and session ID. Codex's `client_metadata` provides `x-codex-installation-id`,
`session_id`, and `thread_id`; matching HTTP headers are accepted as fallbacks.
Body telemetry takes precedence. No CRS code changes are required for Console
and OpenAI Responses relays that preserve these fields.

The `proxy.client.context` event and request-log `diagnostics.client_context`
contain hashed `user_ref`, `session_ref`, and `thread_ref`, plus client kind and
identity source. A user reference groups one device/installation across sessions;
it is not a person or CRS key. Missing identity remains unknown. Malformed or
oversized telemetry is ignored, and identifiers are never added to model input.

Generation and token counting use the same conversation/replay scope, isolated
by service, authenticated Crosslane key, device, and session/thread. Compaction
replay still validates the full history prefix. Owned Responses continuations
inherit omitted telemetry; explicit forks keep separate upstream conversations.
A conflicting known device produces a request error. Attribution for provider
Responses is retained in memory for up to 30 minutes (256 entries), after a
successful response; `store: false` disables retention. Existing account pool
selection, quotas, and content-based token-count caching retain their policies.

## Claude Relay Service response compatibility

Build `crosslaned` with `cargo build -p crosslaned --bin crosslaned --release --features crs`
when its Claude Messages endpoint is used as a Claude Relay Service Console/CCR upstream. This Cargo
feature applies to every Claude Messages listener in that binary. It is disabled in the default
Cargo build.

Console compatibility is the default. For a listener used only by CCR, set
`crs_relay = "ccr"` inside its existing `[[proxy_service]]` entry. This preserves
HTTP error statuses so Claude Code can perform native parameter recovery.
Omit the setting or use `"console"` for the Console transport described below.
The setting does not change HTTP behavior in builds without `crs`.

Full Docker images enable all Cargo features, including `sso` and `crs`, by default.
Use `./deploy/docker-setup.sh --build` to build and deploy locally, or
`docker build --target runtime-full -t crosslane:full .` to build an image only.
To select a feature subset, pass `--build-arg CROSSLANED_FEATURES=crosslaned/sso`
to `docker build`; this disables `crs`.

New builds report compiled features with `crosslaned --version`; inside Docker,
run `docker exec <container> crosslaned --version` and check `crs=true`.
For older builds, directly probe the Crosslane listener (bypassing CRS):

```bash
curl -i "$CROSSLANE_URL/v1/messages" \
  -H "x-api-key: $CROSSLANE_API_KEY" \
  -H 'User-Agent: claude-cli/2.1.295 (external, cli)' \
  -H 'Content-Type: application/json' --data '{"stream":true}'
```

This authenticated probe fails local validation without calling an upstream.
With Console compatibility it returns HTTP 200, `text/event-stream` and `event: error`;
CCR compatibility and builds without the feature return HTTP 400 JSON.

### Errors and streaming transport

Supplier error statuses are preserved for Kiro and Copilot, including 401, 402,
403, 413, 503, 504, and 529. Non-streaming and non-CRS responses use the original
HTTP status; CRS-compatible Claude streams carry it in their SSE error payload.
Error classification and credential redaction do not map supplier statuses.
Transport or protocol failures without a supplier HTTP error still use the
proxy's own error status.

With `crs` enabled, errors include Anthropic-shaped JSON with the sanitized cause,
action hint, failure stage, request ID, `error.status`, and the known supplier
status in `error.upstream_status`. Context errors retain token diagnostics and
compaction-retry results. No CRS code changes are required. CRS Console replaces
non-2xx JSON bodies with fixed messages. Local `context_length_exceeded` errors
at `context_validation` therefore use HTTP 200 with `type: "error"` JSON for
non-streaming Claude Messages. Semantic status 400 remains in `error.status` and
`x-crosslane-error-status`, and caching is disabled. The body contains no successful
message, stop reason, or usage: clients must inspect `type` / `error` rather than
treating HTTP 200 alone as generation success. Statistics retain failure status
400 and separately record client transport status 200. This exception applies only
to local context errors with `crs`; non-CRS builds, OpenAI endpoints, client
authentication, and supplier errors retain their original HTTP statuses. Console
may still sanitize other non-streaming errors; CCR preserves their original bodies.

For Claude `stream: true` with Console compatibility, supplier errors use HTTP 200 and an SSE
`event: error`, preserving the original status in `error.status`,
`error.upstream_status` and `x-crosslane-error-status`. Early errors initialize a
zero-usage empty text block before the error, preventing Claude Code from falling
back to non-streaming and losing details through Console sanitization. Client
authentication, method, and HTTP body-limit failures retain their HTTP statuses.
Once a successful SSE response has started, subsequent errors remain in the
committed stream. Request statistics record failure status separately from the
HTTP 200 client transport. Crosslane checks up to 64 KiB or five seconds of initial
SSE events and enriches errors after assistant content starts.

Console compatibility retries an explicit HTTP 400 rejection of
`output_config.effort` once, before output, using the same account and removing
only that field. Thinking history, signatures, and tool arguments stay intact.
CCR preserves HTTP 400 for client recovery. Other HTTP 400 errors do not trigger
this compatibility retry. Recovery freezes the retry to the original provider
instance and account, retaining service exclusions and provider permissions;
it cannot expand a tagged or explicitly bound pool into the global pool.

SSE JSON strings use ASCII Unicode escapes on the CRS hop to protect Chinese
text and tool arguments from per-chunk UTF-8 decoding. Decoded values are
unchanged; escaping increases wire size. An event exceeding 64 KiB skips only that event's diagnostic parsing;
inspection resumes at the next event. A native Messages EOF without a completion signal
or with open content blocks records a 502 and emits an error, without fabricating
successful completion or closing an unfinished tool. Closed blocks plus a stop
reason but no `message_stop` are treated separately.

Observable downstream cancellations record 499 and settle only received usage;
unknown tokens and supplier charges stay unreported. Crosslane cannot infer a
cancellation that Console does not propagate. Automatic routing can switch only
before content or a tool starts. `failover_timeout_ms` remains a separate deadline
for each automatic provider attempt; zero disables only that provider's attempt
deadline. Pinned routes do not gain this deadline, and no implicit request-wide
deadline is added. Candidate and response preflight share one five-second window
instead of waiting twice. Adding
`Retry-After` in Crosslane cannot restore a header that CRS discards.
Discarded automatic candidates retain their observed usage in their actual
provider, account and model totals, while the final outcome counts as one client
request. Internal summary requests keep their separate accounting.

### Tool history and checkpoints

With `crs`, late `tool_addition` / `tool_removal` declarations update the current
`tools[]` catalog in message order, supporting references and new custom tool
definitions. Their original positions become text notices with cache markers
retained. Tool call IDs, arguments, thinking and signatures stay intact. Historical
references preceding an explicit removal become text records retaining the original
reference data; official search results retain call pairing and active references.
Removed tools stay outside the current catalog. New references after removal still
fail validation. Generation and count_tokens share this projection. Unknown
references, conflicting definitions and unsupported definition kinds fail
explicitly; normal history, capacity and schema checks still apply. Builds
without `crs` keep their existing behavior.

The latest non-empty assistant `compaction` block resets the tool-change history.
Its `tool_changes` restores the checkpoint's tool state before later changes
apply; discarded earlier changes and references cannot alter or reject the active
catalog. Null or missing compaction content remains a no-op. The checkpoint block
and its signature are preserved by this one-pass legacy catalog projection.
Explicit `tool_choice=none` retains completed tool pairs and the schemas needed
to interpret them as source data, even when tool features are disabled. These
declarations cannot activate Tool Search or Web Search, resume pending calls, or
publish callable tools. Generation and token counting use the same tool policy.

## Kiro endpoints and upstream refusals

OAuth Runtime generation, the ControlPlane model catalog, and extension management
REST use separate protocols. Management defaults to `management.{region}.kiro.dev`;
Runtime uses `runtime.{region}.kiro.dev`. Headless API keys retain their CLI-compatible
protocol and do not use OAuth Profile discovery.

| Override | Effect |
| --- | --- |
| `CROSSLANE_MANAGEMENT_URL` | Management host override, including catalog and Profile/usage/subscription requests; supports `{region}` |
| `CROSSLANE_AMAZONQ_URL` | Legacy extension-management override when the management override is absent |
| `CROSSLANE_RUNTIME_URL` | Runtime override, also used by default native MCP |
| `CROSSLANE_MCP_URL` or `upstream.web_search_endpoint` | Explicit legacy JSON-RPC search transport |

Default web search uses native `KiroRuntimeService.InvokeMCP` at the Runtime root.
Leave both explicit search URL overrides unset to select it; setting a root URL
there still selects the legacy protocol. Successful missing-Profile discovery is
cached per account/token/region for 15 minutes, up to 500 entries; failures are not cached.
Generation prefers Runtime by default, including when `preferred_endpoint` is omitted:

```toml
[upstream]
preferred_endpoint = "runtime"
```

Set `"auto"` to select by account type and cached endpoint results, or set
`"amazonq"` or `"codewhisperer"` to prefer a legacy endpoint.
`agent_mode` controls the task mode sent to Kiro and defaults to `"vibe"`.
Its `"auto"` setting uses `vibe` on Runtime and sends `spec` in the header for
legacy AmazonQ and CodeWhisperer requests.

These overrides apply to proxy-account traffic. They do not change the fixed,
validated AWS endpoints used by [Kiro Enterprise administration](kiro-enterprise-admin.md).

### Refusals and empty output

Native Runtime provides terminal metadata for Claude 5 OAuth tool requests. In
`crs` builds with `preferred_endpoint = "auto"`, eligible requests can also select
native Runtime directly.
Kiro `CONTENT_FILTERED` metadata becomes native Messages `stop_reason: "refusal"`
and `stop_details`, including `reasoning_extraction` when reported. Uncommitted tool
calls are cancelled; received usage still settles. Claude 5 tool calls wait for
terminal confirmation even when `features.buffer_tool_calls=false`.

A refusal can arrive with HTTP 200 before a tool executes. Inspect the final stream
event and refusal details rather than treating HTTP 200 or partial tool arguments
as success. Protocol adaptation does not establish why Kiro classified a task or
make the upstream accept it. Temporary model access restrictions are distinct from
one task's refusal; local `health`/`ready` cannot establish model availability or a
recovery time.

Ordinary Kiro responses with no assistant output receive one bounded retry, then
fail explicitly if still empty. Already-visible streaming content is not replayed.
With `crs`, the Claude retry can clarify in tool descriptions that the model returns
structured calls for local execution; names, schemas, arguments, and conversation
text are retained, and descriptions exceeding 1,024 UTF-8 bytes are left unchanged.
