# Protocol compatibility and model controls

[English](protocol-compatibility.md) | [简体中文](protocol-compatibility.zh-CN.md) | [项目说明 / Overview](../README.md)

This reference describes Claude Messages and OpenAI Chat Completions in the current source. See [Responses and Codex](openai-responses.md) for its tool, state, and streaming contracts. Compatibility covers the behavior below, not the complete hosted APIs.

## Copilot protocol adaptation

Every discovered non-embedding Copilot generation model is exposed through
Messages, Chat Completions and Responses. Selection enforces the service account
pool and each account's actual model catalog. Native endpoints are preferred;
conversion is used when no eligible account supports the caller's protocol.
Public `protocols` includes all three; `capabilities.native_protocols` records
the upstream protocols without changing the internal per-account catalog.

The bridge maps history, system/developer instructions, function calls/results,
parallel tools, images/inline files, text/reasoning deltas, finish reasons and
usage. It handles split UTF-8, CRLF and multiline SSE, stable tool indexes, late
usage and raw `copilot_usage`. Truncated/error streams stay failed. Adapted SSE
events are bounded at 8 MiB and full responses/streams at 64 MiB.
Required numeric usage scaffolding carries `kproxy_usage_source: unreported`
until upstream usage arrives, and is not recorded as provider-reported zero usage.
Refusals retain their content and OpenAI refusal fields. An output-length stop
retains partial tool arguments and the incomplete finish reason; it is not
reported as a transport failure. Responses output items also retain incomplete status.
JSON schemas are mapped between Chat `response_format`, Responses `text.format`
and Messages `output_config.format`, preserving concurrent thinking effort.
Messages adaptation requires a concrete JSON schema for structured output;
OpenAI `json_object` requires a native OpenAI endpoint or an explicit schema.
Schema enforcement remains an upstream model capability, as described in the
[Anthropic structured-output contract](https://platform.claude.com/docs/en/build-with-claude/structured-outputs).

Features requiring upstream state or hosted execution are not emulated:
adapted requests must send complete history instead of `previous_response_id`
or `conversation`, and cannot request background jobs, hosted tools or remote
file IDs. Native requests retain those fields. Conversion does not grant a
model new image/reasoning capabilities or guarantee structured output. Opaque
reasoning replay is not portable across protocols. Known Claude thinking
families use their manual/adaptive dialect; unknown models receive no invented
Claude thinking controls.

Messages token counting stays native where available. Other models return a
local estimate (`estimated: true`, `x-kproxy-token-count-source: estimate`);
estimates do not become provider-reported generation usage.

Reference implementations reviewed for the independent Rust bridge:
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI/tree/main/internal/translator)
(bidirectional translation and stream lifecycle),
[LiteLLM](https://github.com/BerriAI/litellm) (protocol/capability boundaries), and
[copilot-api](https://github.com/ericc-ch/copilot-api) (Copilot Messages bridging).
See also Anthropic's [thinking dialects](https://platform.claude.com/docs/en/build-with-claude/extended-thinking).
Tests use mock upstreams; they do not certify a real Copilot seat or model.

The remaining Kiro-specific conversion rules do not change Copilot native forwarding.

## Kiro client versions and the November 2026 retirement

The received AWS Health notice says IDE versions before `0.11.133` and CLI
versions before `1.28.2` cannot connect starting 2026-11-09. The proxy's previous
IDE identity, `0.7.45`, was below that floor. Its CLI identity, `2.10.0`, already
met the floor and is also updated to a verified stable release.

The following official release baseline was checked on 2026-09-14 using the
[IDE downloads](https://kiro.dev/downloads/),
[CLI stable manifest](https://prod.download.cli.kiro.dev/stable/latest/manifest.json)
and [changelog](https://kiro.dev/changelog/):

| Request path | Client version | SDK / generated service client |
| --- | --- | --- |
| IDE / CodeWhispererStreaming | `1.0.437` | `aws-sdk-js/1.0.39`, `codewhispererstreaming#1.0.39` |
| CLI / V2 generation | `2.21.4` | `aws-sdk-rust/1.3.15`, `codewhispererstreaming/0.1.17975` |
| CLI / Management model catalog | `2.21.4` | `aws-sdk-rust/1.3.15`, `codewhispererruntime/0.1.17975` |

`crates/kproxy-kiro/src/identity.rs` owns all versions and UA construction.
IDE generation, model/subscription/usage queries, profile discovery and MCP web
search share the same version, including the `KiroIDE-<version>-<machine_id>`
used for Social token refresh. CLI generation and Management send both
`User-Agent` and `x-amz-user-agent` with the appropriate service identity.
The existing IDE Windows / CLI macOS platform profiles and per-account machine
IDs are preserved; these profiles do not describe the proxy host OS.

This baseline targets the implemented V2/CodeWhisperer protocol. Newer
KiroRuntimeService and V3/KAS paths need separate protocol integration; changing
the service name in a UA does not migrate the request protocol.

Rebuild and replace the `kproxyd` binary or image, then restart it. Configuration
hot reload cannot update compiled identities. The startup log
`Kiro upstream client identity` must show `ide_version=1.0.437` and
`cli_version=2.21.4`. Follow [startup and debugging](startup-and-debugging.md) to
check readiness, a real generation and applicable token refresh after deployment,
and verify endpoint access against the
[official firewall guide](https://kiro.dev/docs/privacy-and-security/firewalls/).
Regression tests cover version floors, emitted headers, fallback requests and
credential rotation. Local tests do not establish server acceptance after AWS's
retirement date.

## Compatibility maintenance rules

| Category | Policy |
| --- | --- |
| Implemented controls | Validate fields actually read and map them through model metadata. |
| Cache controls | Validate `cache_control` against Claude's `ephemeral` / TTL schema; unknown types are not ignored as compatibility hints. |
| Compatibility hints | Accept/ignore format and strict without hard schema guarantees; unknown extra fields alone do not trigger rejection. |
| Missing execution/data paths | Explicitly reject enabled capabilities such as background jobs and foreign hosted file IDs. |
| Responses history | Skip unconvertible items while requiring effective conversation content; tool-catalog controls are not skippable history. |
| Resources and access | Preserve authentication, allowlists, tool pairing, type checks and schema/attachment/context/byte limits. |

Before adding rejection conditions, check the corresponding pinned reference paths
below and identify a conversion requirement, observed upstream limitation or resource/security
boundary. Apart from the explicitly validated cache controls, missing native
mappings or official enum membership alone do not justify rejecting previously
accepted hints. Do not copy mappings observed to fail upstream.
Ignored-control diagnostics use `proxy.compatibility.controls_ignored` and fixed field
names, without schemas or request values.

Model identifiers are limited to 256 bytes without whitespace/control characters;
Unicode, aliases, `/`, `:` and `[1m]` suffixes remain accepted. Invalid values fail 400
before account selection and appear only as `[invalid-model]` in logs. This is a
resource limit, not a model enum. Keep non-null real format-schema/task-budget
regressions across Messages aliases, counting and JSON/SSE; assert exclusion from
Kiro payloads instead of replacing the fields with null to bypass validation.

## Model discovery response contracts

OpenAI and Anthropic both use `GET /v1/models` on their respective API hosts, so
the proxy exposes that shared route and its `/models` alias. It selects the
response protocol from the authenticated client headers. Codex receives the [OpenAI model-list
shape](https://developers.openai.com/api/reference/resources/models/methods/list):
the top-level `object: "list"` and model `id`, `object`, `created`, and `owned_by`
fields. Anthropic-only metadata is not added to this response.

Claude Code receives the [Anthropic model-list
shape](https://platform.claude.com/docs/en/api/models/list). It supports `limit`
(default 20, range 1–1000), `after_id`, and `before_id`, and returns `data`,
`first_id`, `has_more`, and `last_id`. Each model contains `type`, `id`,
`created_at`, `display_name`, `capabilities`, `max_input_tokens`, and `max_tokens`;
OpenAI-only fields and the Kiro description extension are omitted. Kiro discovery
does not provide release timestamps or an Anthropic capability matrix, so
`created_at` uses the protocol-permitted Unix epoch and `capabilities` is `null`.
Dynamic `tokenLimits.maxInputTokens` and `maxOutputTokens` map to the two Anthropic
limit fields; static/configured fallback models report `null` rather than inventing
limits. Cursor IDs are the client-visible IDs, including the `anthropic.` prefix
used for non-Claude Kiro models. Dynamic discovery preserves Kiro's returned
order instead of replacing it with alphabetical order; fallback discovery keeps
the curated catalog order because Kiro does not provide release timestamps.

When neither a Codex/Claude Code User-Agent nor `anthropic-version` identifies the
protocol, an authenticated request receives a hybrid response. It combines the
OpenAI `object` field and per-model `object`, `created`, and `owned_by` fields with
Anthropic pagination and per-model metadata. Hybrid IDs use the Claude-compatible
discovery form so non-Claude Kiro models remain visible; the request resolver also
accepts these IDs on OpenAI generation endpoints. Hybrid requests return the full
list by default and honor `limit`, `after_id`, and `before_id` when supplied. API-key
authentication still runs first. Model discovery uses User-Agent only for response
negotiation, so its client admission check does not reject missing or unrecognized
agents; generation endpoints keep the configured admission policy.

This compatibility covers listing. The separate OpenAI/Anthropic retrieve-model
route (`GET /v1/models/{model_id}`) is not implemented.

## Tool call arguments

Native upstream tool calls, including MCP calls, share the same JSON validation
rules in Claude/OpenAI, streaming/non-streaming, and buffered/unbuffered responses.
Tool names are not used to infer read or write operations. Empty arguments become
`{}`; valid JSON is accepted even when the upstream omits the tool stop event.
Non-empty malformed JSON fails explicitly instead of guessing missing arguments.

## Protocol field normalization

Claude `count_tokens` accepts enabled thinking without an output budget; it does
not compare `budget_tokens` with a synthetic generation limit. Generation still
validates the budget. Missing `tool_result.content` represents an empty result;
`document.citations: null` means omission. Empty document titles use a neutral
native name. Client `search_result` blocks, including tool results, preserve their
source, title and text as labeled source data. They do not acquire Anthropic
citation-index guarantees.

Chat accepts nullable `stream`, `tools`, `tool_calls`, `stream_options.include_usage` and function
`strict`. Assistant refusal blocks and the Chat `refusal` field are preserved as
history text. Legacy `functions`, `function_call` and `role: function` are
normalized into paired Kiro tool calls. Legacy-only requests receive
`function_call` messages/deltas and finish reasons. Missing/blank tool descriptions
receive a neutral default: real Kiro accepts the declaration but rejects replay
of the call when its description is empty.
Null or empty modern tool lists do not override legacy function controls or
change the legacy response format.

Chat `stop` uses the shared incremental stop filter, including matches across
stream chunks. `n` (1–128) executes sequential independent Kiro requests, returns
all indexed choices and sums their usage. Streaming emits one final usage chunk
when requested and one `[DONE]`. Every candidate uses ordinary admission,
authentication and quota accounting; cancelling the stream stops further calls.
The next candidate waits for the previous stream's accounting and admission
release. Retained request bodies stay within the shared memory budget, and
failures after SSE starts are recorded with the committed HTTP status.

OpenAI Chat `file` and Responses `input_file`, including tool outputs, accept
inline base64/data URLs and public file URLs and map to native Kiro documents.
They share Claude's media, size and SSRF protections. Foreign hosted `file_id`
values still require a Files service this proxy does not provide.
If a filename or MIME type does not identify the format, PDF signatures and UTF-8
text can identify supported inline content, including extensionless filenames.

The image-count limit is 100, with unchanged byte/memory limits. Real Kiro accepted
100 small valid PNGs. Six small text documents still produced an upstream 400,
so the five-document bound remains. Kiro accepted a zero-token setting but still
generated text; `max_tokens: 0` cache warming remains unsupported. Background jobs,
Conversations API state, Responses context-management execution/hosted tools,
exact prefill, log probabilities and hard JSON Schema guarantees are separate
capabilities; accepting their shapes cannot create those guarantees. Empty Claude
web-search domain lists are no-ops; non-empty filters still need a compatible
search executor.

## Claude Code MCP Tool Search

Claude Code loads every MCP schema up front when `ANTHROPIC_BASE_URL` points to
a third-party proxy unless Tool Search is explicitly enabled. For large MCP
catalogs, start Claude Code with:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:5580 ENABLE_TOOL_SEARCH=auto claude
```

`kiro-proxy` accepts Anthropic `defer_loading`, regex/BM25 Tool Search, and
`tool_reference` history blocks. Because Kiro has no native Tool Search server
tool, the proxy executes the search locally and continues the same response.
The official Tool Search input contains `pattern` or `query` plus an optional
`limit` from 1 to 10,000 (default 5). `kiro-proxy` honors that requested limit
and then packs results against the remaining tool-count, tool-token, context,
and payload-byte budgets; there is no fixed five-tool working set. Deferred
definitions remain outside the Kiro context and payload until discovered. The
catalog index and searches run on blocking workers rather than HTTP runtime
threads.

The generated `[context]` configuration also bounds the loaded working set with
`max_loaded_tools` (default and proxy ceiling 512), deferred Tool
Search working-set definitions with `max_tool_input_tokens`, and the serialized
Kiro request with `max_upstream_payload_bytes`. Ordinary requests without Tool
Search are not subject to that 32k working-set budget: their definitions remain
part of the model's total input-token estimate and are still bounded by the
context window, tool count, and payload size. Truly oversized requests fail
locally instead of producing an opaque upstream error. HTTP
`413/request_too_large` is reserved for an actual inbound body over 50 MiB;
tool, context, and translated-payload budget errors use 400 so Claude Code does
not misreport them as a 32 MB attachment failure.

## Automatic compaction and model windows

Mapping-aware context compaction for Claude Messages is enabled by default with
`context.auto_compact_on_overflow`. Before deciding whether to compact or reject
input, the proxy selects an account and resolves its actual model, including
account-dependent mappings, weighted choices, aliases, and defaults. It uses that
model's safe window and reuses the selection when no compaction is needed. If upstream still
returns `prompt is too long` or `context length exceeded`, the proxy compacts
against a conservative window and retries only once. A semantic-summary request
uses complete source material. Oversized input is split losslessly at UTF-8/token
boundaries before summarization; each bounded chunk is summarized independently
and the summaries are assembled in order. No extractive truncation precedes the
semantic summary, and every chunk is checked against the summary input window. The account
slot is released before summarization so a single-concurrency account cannot
block its own summary request. The same compaction artifact may also be reapplied
once if dispatch after summarization resolves to a smaller window. OpenAI Chat Completions, Responses with `truncation: disabled`, and context growth
after a Tool Search response has started retain hard context-limit errors because they cannot safely return a leading Claude
`compaction` boundary. A summary timeout releases the main request immediately;
late accounting is allowed only for a bounded grace period, after which the
summary stream is canceled and any already decoded usage is settled.

Summary waiting defaults to 120 seconds (`context.compaction_summary_timeout_ms`);
upgrades preserve explicit settings, including existing `30000`, `60000`, or `150000`
values. Claude Code receives a complete compaction start block because its
accumulator can ignore compaction deltas; other SDKs retain the standard delta
stream. A timeout emits one fallback warning, with subsequent accounting at INFO
under the same trace.
`credits_source=estimated` identifies a local estimate, not confirmed upstream
charges. Daily logs persist in the data volume across container replacements.

### Summary resources and windows

One main request starts at most one semantic-summary operation, with at most 16
chunks and concurrency 2, subject to account limits and one shared timeout. Any
failed chunk prevents complete summary success. The single context replan/retry
reapplies `CompactionArtifact`, possibly shortening its checkpoint or retaining
fewer recent rounds, without another summary operation. The main request retains
its actual mapped model; source names and `[1m]` suffixes do not enlarge its capacity.
The compaction target normally reserves 25% of the safe window. Protected content
may relax the target if it still fits the hard maximum; exceeding that maximum
fails. Input and output follow separate model limits, without an assumed shared window.

Defaults come from [ContextConfig](../crates/kproxy-core/src/config.rs):

```toml
[context]
max_input_tokens = 200000
safe_input_ratio = 0.95
compact_safe_input_ratio = 0.99
auto_compact_on_overflow = true
max_tool_input_tokens = 32000
max_loaded_tools = 512
max_upstream_payload_bytes = 8388608
compaction_summary_model = ""
compaction_summary_timeout_ms = 120000
compaction_preserve_recent_turns = 3
```

`max_input_tokens` is a fallback when metadata is absent, not a universal model
capacity. An empty summary model reuses the resolved model for initial compaction;
an explicit model must be schedulable. Recent complete user/assistant rounds default
to 3, capped at 64 and ultimately constrained by the window. `count_tokens` applies
existing boundaries/edits without a summary call. Prompt Cache profiles follow the
final payload; Claude cache TTL does not control Kiro lifetime, and compaction cannot guarantee a cache hit.

### Explicit Claude Code summaries and preservation

Manual `/compact` has a special path only for a recognized Claude Code User-Agent,
valid input and a matching known summary instruction in the final standalone text
block. Complete preceding blocks become source material. Original system content
and loaded tool definitions/schemas/examples become complete tagged JSON source
records, with executable tools removed. Undiscovered deferred tools stay outside
context. Ordinary quoted instructions, other SDKs and explicit `tool_choice: any/tool`
do not activate this exception. This path remains available when ordinary automatic
compaction is disabled. A successful summary does not shrink system/tool definitions
the client sends again next turn.

Ordinary generation protects system prefixes, the current turn, tool schemas and
use/result pairing. Oversized protected content produces matching local estimates
in HTTP `error.context` and log `diagnostics.context_overflow`: total, protected prefix,
tools, current message, history, structural overhead, incompressible minimum and
model maximum. Source text is excluded. The CLI shows `context_tokens`; inspect it
alongside `models resolve` and `logs trace`.

### Replay, fallback and accounting

The `compaction` block must lead the response. It is held until upstream output can
commit, preserving pre-response retries. Claude Code receives a complete checkpoint
start + stop (a 2.1.260 regression is recorded); other SDKs receive null start + complete
delta + stop. Do not duplicate content or overwrite it with an empty delta. The next
request applies the boundary and discards earlier history. Client upgrades require
real accumulator validation, not merely a successful first response.

Failure, timeout, insufficient quota or empty/invalid summaries restore the original
payload before extractive fallback. Input still exceeding the window fails.
Semantic summaries can lose information; extractive fallback loses more. Summary
hash caching and rolling cross-chunk merging are not implemented. Repeated summaries
by the client can introduce further information loss.

Summaries independently use the pool, reservations, meter and `/internal/compact`
statistics. Claude's top-level tokens describe main generation; `usage.iterations`
separates summary and sampling. Main generation can exhaust quota after a successful
summary; the operations are not atomic. `credits_source=server/estimated` distinguishes
upstream data from estimates. Failed summaries without output or usage are not
charged from input estimates alone. Timeout stops waiting and dispatching;
background settlement has a bounded grace period before canceling the stream and
settling decoded usage. One WARN records timeout fallback; subsequent settlement
uses INFO with the same trace. `compaction_mode` distinguishes semantic/extractive_fallback.

See [request preparation](../crates/kproxyd/src/http/handlers/request.rs),
[summary execution](../crates/kproxyd/src/http/handlers/compaction.rs),
[summary planning](../crates/kproxy-translate/src/tokenizer/compaction.rs) and
[Claude Code recognition](../crates/kproxy-translate/src/context/claude_code.rs).
Mock regressions and the real-client command are in [CONTRIBUTING.md](../CONTRIBUTING.md#validation).

## Tool Search continuation limits

`features.tool_search_max_rounds` defaults to 4 and is hard-clamped to 8. If
that per-request server loop is exhausted, the response uses Claude's
`pause_turn` continuation state instead of converting a valid server call into
an HTTP 5xx. `features.tool_search_max_operations` defaults to 32 (valid range
1–256) and bounds aggregate searches across resumed calls and all internal
rounds; excess calls receive an in-band `unavailable` Tool Search result.
`features.enable_tool_search=false` is a rollback switch: native Tool Search
requests are then rejected explicitly instead of expanding deferred tools into
the upstream payload. Request logs retain catalog/working-set sizes, search
limits and truncation, client/upstream status, and a stable error code. Error
responses keep the normal Claude/OpenAI body and expose diagnostics through
`request-id`, `x-kproxy-error-code`, `x-kproxy-error-stage`,
`x-kproxy-upstream-status`, and `x-kproxy-account-error` headers.

## Web Search

For Claude's native `web_search` server tool, Kiro first decides whether to
search and chooses the query. The proxy then calls Kiro's `/mcp` JSON-RPC
`web_search`, returns the real result as a tool result to the same model turn,
and lets Kiro synthesize the final answer. It does not search the first user
message eagerly or present a raw search summary as model output. Streaming and
non-streaming Claude responses use `server_tool_use` and
`web_search_tool_result`; search failures are in-band tool errors and do not
cool down or ban the account. Parallel searches are preserved. In mixed
server/client-tool turns the server call remains pending until the client tool
results are returned, matching Anthropic's continuation protocol. Results carry
proxy-owned AES-256-GCM replay content so later turns can restore snippets;
modified records are rejected before entering model context. Anthropic-owned
opaque values remain accepted but are not decrypted locally. Final text carries
a structured `web_search_result_location` citation only when it actually
includes the exact result URL. The proxy
safety limit defaults to 20 searches; an explicit `max_uses` above that
configured limit is rejected instead of being silently clamped. Claude Web
Fetch is rejected explicitly until a compatible server-side executor is
implemented.

The default MCP URL is `https://runtime.{region}.kiro.dev/mcp`. Override it with
`upstream.web_search_endpoint` (the `{region}` placeholder is supported) or the
temporary `KPROXY_MCP_URL` environment variable. The default
`upstream.web_search_timeout_ms` is 60,000. Every MCP request carries Kiro's
required `x-amzn-kiro-profile-arn` header. When an imported account has no
profile ARN, the proxy discovers it through `ListAvailableProfiles`, collapses
concurrent discovery for the same token, and persists the result. The documented
credential flows cover enterprise SSO and headless API keys; low-level compatibility
branches do not add personal/social OAuth login flows.
Domain/location filters and code-execution callers still require compatible
executor support. Strict and eager-streaming flags are accepted as ignored
compatibility hints. The
proxy-generated encrypted fields
are explicitly proxy-owned and are not claimed to be interoperable with
Anthropic's hosted-search ciphertext.

## Documents, context editing, and compatibility boundaries

Claude `document` blocks support `base64`, `text`, HTTP(S) `url`, and `content`
sources, including documents inside tool results. Custom content is flattened
in order into a text document; embedded images are hoisted to the same Kiro
message's image list with numbered markers. This does not preserve Anthropic's
custom citation-chunk semantics. Each request accepts at most five documents
(4,500,000 decoded bytes each) and 100 images (5 MiB each). Remote media is
restricted to public addresses, revalidates DNS on every redirect, ignores
environment proxies, and checks file signatures against media types. Kiro
citations, web sources, and license details remain visible as References rather
than fabricating Claude source-document character, page, or block coordinates.

`clear_tool_uses` honors trigger, keep, clear_at_least, exclude_tools, and either
a boolean or tool-name list for clear_tool_inputs. `clear_thinking` can retain
the selected number of turns or all thinking. Generation reports executed edits
in `context_management.applied_edits`; activation and cleared-token statistics
use local estimates, while `count_tokens` reports estimated input sizes before
and after editing. Generation clears discarded history before fetching the
remaining remote attachments.

Compatibility follows the practical behavior of jwadow/kiro-gateway,
hj01857655/kiro-account-manager, and chaogei/Kiro-account-manager rather than
requiring exact Claude/OpenAI feature equivalence. Additive request/message/tool
fields, tool strict hints, and Claude/OpenAI format hints remain permissive.
Accepted format hints do not provide native structured-output guarantees. See the
[pinned references](#pinned-references-and-regressions) and [maintenance rules](#compatibility-maintenance-rules) for sources and scope.

Adjacent same-role messages are merged. Assistant prefill, `max_tokens=0` cache
warming, and Anthropic Files API `file_id` sources remain outside the implemented
generation/data-resolution paths and are still rejected.
Protocol compatibility is resolved before
the first upstream call, without a time-based rejection cache or field-removal
probes. Cache markers contain only `type: default`; Claude cache TTL preferences
do not control Kiro's cache lifetime. An absent or null `cache_control` means
omission at every supported request/message/tool/content location. Cache objects
follow the [Claude API schema](https://platform.claude.com/docs/en/api/messages/create):
`type` must be `ephemeral`; `ttl` may be omitted (default `5m`) or set to `5m` or
`1h`. Missing, null, empty, non-string, or unknown types and invalid TTLs return
400 rather than being ignored or rewritten. Additional fields are not forwarded
to Kiro and do not imply support for their semantics. Chat Completions cache
extensions share this object validation; message-level `cache_control` remains a
proxy extension.

Claude Messages and its counting aliases also enforce the
[documented cache rules](https://platform.claude.com/docs/en/build-with-claude/prompt-caching):
at most four breakpoints, with `1h` preceding `5m` in `tools → system → messages`
order. Automatic caching finds the last eligible block, merges an existing marker
with the same TTL, and rejects a conflicting TTL. When an eligible target exists
but four explicit breakpoints already occupy the slots, automatic caching is
rejected; when no target exists, it is skipped. Thinking, redacted thinking, and
empty text cannot have explicit cache markers. Nested cache objects in tool
results, content documents, and search results are validated too. These input
checks do not promise Claude's cache lifetime, hit boundaries, or billing on Kiro.
Claude historical thinking is omitted from Kiro
request history without disabling current-generation thinking; Responses plaintext
reasoning summaries are preserved in assistant history. Document context
is preserved as separate JSON-labelled message text rather than a document field.
Generation controls use the explicit mapping in
[chaogei/Kiro-account-manager](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/translator.ts), with explicit Claude effort support and without its speculative missing-metadata thinking fallback:

| Client control | Kiro/proxy handling |
| --- | --- |
| `max_tokens`, `temperature`, `top_p` | Map to `inferenceConfig.maxTokens/temperature/topP`; explicit zero sampling values survive. If neither OpenAI output-limit field is provided, `maxTokens` stays omitted; no 8192 default is sent. Model-specific rejection is still possible. |
| OpenAI `max_completion_tokens` | Maps to `inferenceConfig.maxTokens` and takes precedence over `max_tokens`; the effective limit also drives continuation budgets, credit estimates and finish reasons. |
| Claude `top_k` | Accepted but omitted with a debug diagnostic, regardless of model metadata. This is a gateway compatibility policy, not a claim that Kiro universally rejects it. |
| Claude `stop_sequences` / Chat `stop` | Enforced locally in streaming and non-streaming responses; no native `stopSequences` is sent. This does not guarantee a server-side generation/cost limit. |
| Thinking / effort | Recognized effort metadata chooses `thinking: adaptive` + `output_config.effort`, or `reasoning.effort`. Missing, incomplete, or unrecognized metadata omits the entire `additionalModelRequestFields` field, never sending `{}`, `null`, or speculative adaptive thinking. |
| Claude `output_config.effort` | Explicit effort takes precedence over the thinking budget and is mapped through the selected model's Kiro metadata. Effort-only system messages apply from the next user turn and survive compaction and internal continuations. Kiro receives the effective request-level effort; Anthropic's per-message cache semantics are not guaranteed. |
| Claude `output_config.format` / `task_budget` | Accepted but omitted from Kiro input, with a debug diagnostic containing only field names. No JSON/Schema or task-budget guarantee is added; `output_config.effort` is still mapped independently. |
| OpenAI `response_format` / Responses `text.format` | Accepted but omitted from Kiro input, matching the permissive reference behavior; no JSON/Schema guarantee or schema-driven retries are added. |
| Tool `strict`, Claude `eager_input_streaming` | Accepted as hints; normal Kiro tool schemas and existing streaming behavior are retained. |
| Service tier, additive fields, unused stream hints | Accepted without forwarding them as speculative Kiro fields. Used values such as `include_usage` retain type validation. |
| OpenAI `reasoning_effort` | `none` uses the existing disabled-thinking path. Other values take precedence over `thinking.budget_tokens`; otherwise the default is high. Unsupported values select the last advertised effort, without sorting or nearest-rank matching. |
| `thinking.display` | The output_config dialect always sends summarized; the reasoning dialect omits display. Without recognized metadata, the entire extension is omitted. Client display does not override these upstream shapes. |
| `thinking.budget_tokens` | Maps directly to low (≤4000), medium (≤16000), high (≤64000), or xhigh; it is not an exact native thinking-token cap. |
| `thinking: disabled` | Omits thinking controls and suppresses returned reasoning; omission does not guarantee that a model with thinking enabled by default stops thinking internally. |

Unspecified effort defaults to `high`, not the JSON Schema default, matching the
reference's runtime metadata extraction. Thinking is no longer automatically
disabled on tool-result or control follow-ups. Both legacy `adaptive_thinking`
and `max_thinking_budget_tokens` settings remain readable but no longer affect
generation controls. An explicitly configured operator `model_thinking_mode`
deny rule remains enforced; an allow rule cannot create upstream parameter support.

Unsupported thinking controls degrade to the upstream model's default behavior;
the request still runs, but its requested thinking mode/budget is not guaranteed.
The debug decision reason is `ModelControlsUnavailable`. This concerns parameter
support, not whether the underlying model can reason. In an AmazonQ Haiku 4.5
probe, even an empty extension was rejected, while omission succeeded. No model-name
blacklist, prompt-tag simulation, or field-removal retry is used for this fallback.

Claude Code's system `<env>` block supplies Kiro `envState` (working directory
and operating system). It is preserved across internal continuations; the
proxy never substitutes its own host environment. XML tool-call recovery only
accepts complete, valid calls to tools declared in the current round, at a line
start outside Markdown code fences/indented code and tagged thinking. Inline
examples, unknown tools, and malformed XML remain ordinary response text.
Recovered string parameters retain their schema-declared type and whitespace,
including local references and composed schemas. Unresolved or over-budget
schema references leave the XML as text; recovery is not a full schema validator.

Alignment covers outbound generation parameters. Request validation, local stop
filtering, bounded internal continuations, and response protection remain:
`display: omitted` still hides returned reasoning text locally while retaining
native signatures, without changing the reference's upstream display strategy.
Original controls are kept in non-serialized metadata and rebuilt for each actual
model, including HTTP/stream fallbacks and internal continuations. When OpenAI
`max_tokens` is omitted, 8192 is used only for credit estimation; it neither
limits internal continuations nor causes a `length` finish reason. Explicit
output limits remain enforced, and no default `maxTokens` is sent upstream.
Ordinary validation failures and
invalid signatures are returned without learning or probing protocol capabilities;
the existing authentication, transient-failure, and context-overflow handling is
unchanged.

## Pinned references and regressions

These historical snapshots do not establish current behavior in those projects; this repository's code and tests define its behavior.

| Reference | Pinned snapshot |
| --- | --- |
| jwadow/kiro-gateway | [Claude models · a5292ca](https://github.com/jwadow/kiro-gateway/blob/a5292ca04c7c6231e0b47673ac3f981f5a706e1e/kiro/models_anthropic.py) |
| hj01857655/kiro-account-manager | [Gateway models · c5c4776](https://github.com/hj01857655/kiro-account-manager/blob/c5c477647f8cba4c9b9f07e8fb41e403672adf36/src-tauri/src/gateway/models.rs) |
| chaogei/Kiro-account-manager | [Translator · 447adcd](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/translator.ts) |
| d-kuro/kirocc | [Runtime / effort · e0850b1](https://github.com/d-kuro/kirocc/tree/e0850b11e1a61cbb84aa55739a41b21d4a5c5fcf) |
| ZyphrZero/kiro.rs | [Tool recovery · f357292](https://github.com/ZyphrZero/kiro.rs/tree/f3572929fbc2c0c090c29b13a7c285d1b2777dcd) |
| pinealctx/kiro-gateway | [Model aliases · 9f614d4](https://github.com/pinealctx/kiro-gateway/tree/9f614d4b270e8e80e13c7d3603cbb1b090f327c4) |

```bash
cargo test -p kproxy-translate --test compatibility_controls --locked
cargo test -p kproxy-translate --test claude_gateway_controls --locked
cargo test -p kproxyd --test end_to_end compatibility_controls:: --locked
cargo test -p kproxyd --test end_to_end claude_gateway:: --locked
```
