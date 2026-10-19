# Crosslane operations

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [Project overview](../README.md)

This guide covers deployment, accounts, service access, routine checks, and
backup. Run commands from the repository root with a matching `crosslane` CLI.

## Requirements and configuration

Docker deployment requires Docker Engine and Compose. Native builds support
Linux and macOS with Rust and a C toolchain. Browser-based login also needs
Chrome or Chromium for native builds.

Copy `.env.example` to `.env` for a local installation. Set an absolute
`CROSSLANE_HOME` when starting the daemon and CLI from different directories.
Use `crosslane config path` to find the active configuration, then
`crosslane config reload` after editing it. Keep account credentials out of
shell arguments, logs, and source control.

## Docker

Build the current checkout and install the host CLI command:

```bash
./deploy/docker-setup.sh --build
crosslane health
crosslane ready
```

Alternatively, run `./deploy/docker-setup.sh --image IMAGE` for an available
image. The default host CLI destination is `/usr/local/bin/crosslane`; use the
setup script's `--target PATH` option for another location.

Compose uses host networking. Bind a service to `127.0.0.1` for local access,
or restrict its port and configure TLS before external access. Data is kept in
the `crosslane-data` volume. For daily checks and upgrades:

```bash
docker compose ps
docker compose logs -f crosslaned
./deploy/docker-upgrade.sh
crosslane version
crosslane ready
```

Before an upgrade, stop writes and back up the full data volume. An image
rollback does not reverse data changes. Use `crosslane stop` to stop the
deployment without removing its data; read `crosslane uninstall --help` before
removing it.

## Command navigation and ownership

`-h`, `--help`, and `help` show complete options and examples without a running daemon:

```bash
crosslane service edit -h
crosslane help service edit
crosslane service help edit
crosslane help --all
crosslane help account --all
```

| Command group | Managed resources |
| --- | --- |
| `provider` | Upstream instances, driver settings, default models, and fallback policy |
| `account` | Upstream logins, credentials, health, and tags |
| `service` | Business listeners, allowed providers, client key bindings, and account pools |
| `apikey` | Client credentials, provider/model permissions, limits, and usage |
| `models` / `model-map` | Model discovery and global disabling / client model mapping rules |
| `tasks` / `alert` | Scheduled tasks / shared notification targets and current Kiro events |

Bare top-level groups display help; scripts using `--json` must select an action.
`account overage` retains its legacy default `show` behavior. `tasks list` identifies
manually runnable tasks; `tasks run` help and Shell completion list only those tasks.
The watcher and adaptive admission tasks run only under the scheduler.

Queries use `--provider` to select an instance. Creating or editing a service, client
key, or mapping uses it to set or replace provider permissions. Permission edits
require an explicit option; `CROSSLANE_PROVIDER` never replaces existing permissions.
Global commands and resources addressed directly by ID/name reject provider filtering
and ignore that environment default. Global model disabling/enabling accepts the
compatibility marker `--provider all`. Alert targets are addressed by name; current
account events come from Kiro. Empty edits and conflicting set/clear options fail
before connecting to the daemon, with a link to the relevant command help.

## Accounts

### Kiro

Import a headless API key from the CLI environment or standard input:

```bash
crosslane account add-api-key --email ci@example.com --region us-east-1
crosslane account add-api-key --email ci@example.com --region us-east-1 --key-stdin < /secure/kiro-key
```

The first command reads `KIRO_API_KEY` from the CLI environment. For enterprise
SSO, configure your organization's start URL and use the SSO account command:

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

```bash
crosslane config reload
crosslane account add-sso --email user@example.com --password-stdin < /secure/kiro-sso-password
```

Use `crosslane account add-sso --help` for interactive, batch, and stdin
options. Check accounts with `crosslane account list` and
`crosslane account refresh --help`. Export output is redacted by default;
use secret-bearing exports only in protected backups.

Native OAuth Runtime calls, the ControlPlane catalog, and extension management REST
use separate protocols and identity headers. Management defaults to
`management.{region}.kiro.dev`. Missing-profile discovery successes use an
account-scoped cache of up to 500 entries for 15 minutes. Default Web Search uses
`InvokeMCP` at the Runtime root; explicit `CROSSLANE_MCP_URL` or
`upstream.web_search_endpoint` retains legacy JSON-RPC compatibility.

### GitHub Copilot

Create a Copilot provider, then authorize an account through GitHub Device
Flow or import an existing token through standard input:

```bash
crosslane provider add --id copilot --kind copilot
crosslane account add --provider copilot --auth device-flow
crosslane account add --provider copilot --token-stdin < /secure/github-token
crosslane account list --provider copilot
```

Choose an account method that your GitHub organization permits. `--provider`
takes the instance ID selected with `provider add --id`.
On a headless server, select GitHub password login with
`account add --provider copilot --headless`, or use
`--username <GitHub-username-or-email> --password-stdin`. An email must be verified
and belong to the authorized account; Crosslane checks it through the GitHub API
before saving the account. Azure SSO still requires the full GitHub username in
`--username` and the Azure email in `--sso-username`. MFA codes or push approval
still require the account owner.
Within a service account pool, Copilot selects accounts by active concurrency
and recent use. Configure `weight_active`, `weight_idle`, and `idle_window_ms`
under `[provider.pool.balance]` (defaults: `0.8`, `0.2`, `300000`). Copilot does
not currently expose a reliable per-account remaining-credit balance, so Kiro
credit scoring does not apply.

`account list` includes Copilot's plan, API token deadline, quota reset date,
observed AIU per account, and the upstream monthly `credits_used` value separately.
Token-based quotas display AI credits, preferring `credits_used` for consumption
and fractional `quota_remaining` for the balance. Legacy quotas retain request
units. Business placeholders (`unlimited: true`, zero entitlement and balance)
display unknown totals and balances rather than `0/0` or `100% remaining`.
JSON responses add `details.quota_summary` with personal scope, unit, used,
total, remaining, remaining percentage, unlimited flag, and cache status; unknown
quantities are null. Raw `details.info.quota_snapshots` remain available.
`account show ID --provider copilot` also displays
GitHub access/refresh token deadlines, the quota query time, and the AIU tracking
period; `--json` retains the safe upstream quota fields. An API token deadline
is automatically renewed and is not a subscription expiry date. Missing
deadlines or billing data remain unknown. Quota queries reuse successes and
failures for five minutes; failed queries retain the previous snapshot without
changing account health.

AIU totals start with the first billed response observed by this installation,
include both JSON and streaming responses, and survive restarts and client-key
usage resets. They use the upstream `copilot_usage.total_nano_aiu` amount
(1 AIU = 1,000,000,000 nano AIU); they do not represent GitHub-wide account
usage or remaining organization credits. An unlimited personal quota, including
Business responses containing zero limits and `credits_used`, does not establish
the organization's balance. Kiro credits and Copilot AIU remain separate.

## Services and client access

A new installation has no business listener. Create one and save the client
API key printed by the command:

```bash
crosslane service create --name main --host 127.0.0.1
crosslane service list
crosslane ready
```

To make a service use only Copilot, specify its provider and default provider:

```bash
crosslane service create --name copilot --host 127.0.0.1 --provider copilot --default-provider copilot
```

A service exposing both Kiro and Copilot automatically balances unqualified
requests that do not match an explicit model mapping:

```bash
crosslane service create --name mixed --host 127.0.0.1 --provider kiro --provider copilot --default-provider kiro
```

Both the service and client key must permit each provider. Routing checks the
service's account membership and usable model caches for the same model,
including equivalent aliases, then selects the provider with lower concurrent
load relative to account capacity. Equal pressure rotates between providers.
Automatic routing does not estimate Copilot credits or change model versions;
each provider retains its own account scheduler. `default_provider` is the
initial rotation preference and the compatibility default for explicit mappings
or requests without a discovered route. Account bindings do not expand access.

Connection failures, upstream account authentication/quota failures, throttling,
and service errors before response delivery can fail over to another compatible
provider. Failed routes cool down per service and model, starting at five
seconds and increasing to sixty seconds after consecutive failures. One recovery
request is admitted after cooldown; success restores balancing. Client request,
permission, and client-key quota errors are not retried across providers. Once a
stream starts, existing error events are preserved and subsequent requests avoid
the failed provider; emitted content is never replayed. Messages, Chat
Completions, Responses, and `count_tokens` share this routing entry point.
Batched Chat requests keep their native semantics; each candidate expanded by
Kiro participates in automatic routing independently.
Token counting has independent circuit state: a local Kiro count cannot reset
generation failures. Each automatic attempt has a five-minute deadline before
returning a response. Configure `[provider.routing].failover_timeout_ms` with
`crosslane provider edit copilot --failover-timeout-ms 300000`, or set it to `0`
to disable this deadline. Started streams retain their existing idle
timeouts and are not cut off by the attempt deadline.

Qualified IDs such as `kiro/MODEL_ID` and `copilot/MODEL_ID`, and explicit model
mappings retain their provider selection. Model defaults that replace a request
with a different model keep their original route; defaults for the same model
or an equivalent alias still allow balancing. Responses continuations retain the
provider that created `previous_response_id`, including for anonymous clients.
Explicit cross-provider mappings still require the source provider's
`allow_cross_provider_fallback`; automatic balancing uses the providers already
allowed by the service and key. Correlate `upstream.provider.selected` and
`upstream.provider.failover` log events with `trace_id` to inspect decisions.

A service accepts only its bound client keys. Generation routes also enforce
a client product User-Agent policy. For another client, an operator can set
`--skip-user-agent-check true` on the service or client key; key authentication
and other service limits remain in force. Check the applicable command help
before changing access:

```bash
crosslane help service edit
crosslane help apikey edit
```

## Desktop tools and web search

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

## Models and quotas

```bash
crosslane models list
crosslane model disable MODEL_ID
crosslane model enable MODEL_ID
crosslane pool --model MODEL_ID --explain
crosslane account overage show
```

Use an ID from the model list. Disabled models are unavailable to new requests.
The business endpoints `GET /v1/models` and `GET /models` return the union of
models supported by enabled accounts in the current service's effective pool,
after applying service/API key provider permissions, API key model permissions,
and global model disabling. Tags, manual inclusions, and explicit exclusions
apply to Kiro, Copilot, and mixed-provider lists. A provider with no enabled
account in that pool contributes no models; an empty pool returns an empty list.
Kiro retains its static catalog fallback when an in-pool account has no discovered
catalog or dynamic discovery is disabled. Copilot discovery and cached catalog
aggregation only use in-pool accounts. The CLI model list is a provider-wide
administrative query and does not use a service account pool.

Copilot also accepts equivalent client aliases such as `claude-opus-4-8` for
the discovered ID `claude-opus-4.8`. Explicit model mappings run first, and
permissions are checked against the resolved ID. To route a missing Claude
version to the nearest available version in the same Opus, Sonnet, or Haiku
family, run `crosslane provider edit copilot --enable-model-fallback true`.
This prefers the same major version, then the nearest version, and the newer
version on a tie. For example, missing `claude-opus-4-6` resolves to
`claude-opus-4.8` when that is the nearest available Opus model. Ambiguous IDs,
unknown families, and variants such as `-fast` still require explicit mappings.
Fallback does not bypass disabled models or API key permissions. The provider
switch defaults to false; inspect the result with
`crosslane models resolve claude-opus-4-6 --provider copilot --json`.
Kiro overage is disabled by default. To set a maximum of 500 overage credits
per account, run `crosslane account overage enable --max-credits 500`; use
`crosslane account overage disable` to turn it off. Upstream limits and requests
already in progress can still affect final usage.

## Request latency controls

### Load Claude Code MCP tools on demand

Claude Code can disable tool search when `ANTHROPIC_BASE_URL` points to
Crosslane, loading every MCP tool into the model context on each turn. Enable
it explicitly before starting Claude Code:

```bash
ENABLE_TOOL_SEARCH=true claude
```

Alternatively, merge this field into Claude Code's `~/.claude/settings.json`
and restart the session:

```json
{
  "env": {
    "ENABLE_TOOL_SEARCH": "true"
  }
}
```

Crosslane enables `features.enable_tool_search` by default. Client `ToolSearch`
calls remain client-executed. A `defer_loading` definition enters a Kiro
request only after a history `tool_reference` discovers it or the tool has
been used; unmatched definitions stay out of the upstream context. Previously
used tools and a tool explicitly selected by `tool_choice` remain available.
Long names use upstream aliases and are restored in client responses. If
proxy tool search is explicitly disabled, deferred-tool requests still fail
instead of silently loading the full catalog.

Ordinary built-in tools, system instructions, and conversation history still
consume context. Compare `loaded_tool_count`, `deferred_tool_count`,
`loaded_tool_bytes`, and `input_tokens` in `proxy.request.prepared`, then
measure both first-byte and complete response times. Counts cover definitions
in this request, not the full MCP catalog retained by Claude Code. See the
[Claude Code MCP documentation](https://code.claude.com/docs/en/mcp#configure-tool-search)
for the client switch.

Copilot model lookups reuse an enabled account's catalog up to
`provider.models.max_stale_ms` while scheduled or explicit refreshes update it.
A missing catalog on another account does not block requests with a usable
snapshot. Cold or fully expired catalogs still require discovery; expired
models cannot authorize or schedule a request. Refresh concurrency is controlled
by `provider.settings.model_refresh_concurrency` (default: 32, no more than the
enabled account count). Large pools can raise it, for example to 64; lower it if
the upstream rate limits discovery. It must be positive and does not make
requests with usable snapshots wait for discovery.

```toml
[provider.settings]
model_refresh_concurrency = 32
```

Copilot accepts these positive millisecond values in the corresponding
`[provider.settings]` table:

```toml
stream_read_timeout_ms = 600000
oauth_refresh_timeout_ms = 30000
account_queue_wait_timeout_ms = 30000
```

The stream timeout bounds continuous inactivity, including waiting for response
headers, and permits longer streams that keep making progress. The OAuth
deadline covers headers and body; the queue deadline cancels admission with a
local `503` / `server_overloaded`, without a supplier status or an account-health
penalty.

Kiro releases the previous transport slot before retrying or continuing a
stream. Local slot exhaustion returns `503` / `server_overloaded` without
account switching or an account-health penalty. Before the first semantic
output, retries use `upstream.max_retries`; account count cannot increase this
budget. A hollow continuation after tool results retains its separate bounded
semantic retry. Valid text from a tool-result continuation streams immediately.
Completed tool arguments are sent after validation without an extra delay;
`features.tool_call_buffer_delay_ms` remains accepted for old configurations but
is ignored and defaults to zero.

These controls cover Anthropic Messages, OpenAI Chat Completions, and Responses.
Authentication, client checks, model permissions, and the 50 MiB request-body
limit retain their existing rules. Tool argument validation and protocol-specific
completion/failure event formats are preserved. With `crs`, streaming Messages
service errors still use HTTP 200 SSE and carry the actual status in
`error.status`, for example 503 for local overload.

## Logs and troubleshooting

```bash
crosslane logs show --tail 100
crosslane logs follow --level warn
crosslane logs trace <TRACE_ID>
crosslane status --since 30m
crosslane stats --detail --since 1h
```

Business request logs include the client `user_agent` when supplied. Use the
`x-trace-id` response header or `request-id` to find request logs. Check the
response status, error code, and trace when investigating failures. A stream can fail after
HTTP 200, so inspect its final event.
`upstream.account.selected` and completion events include `provider_id` and
`account_id`; use `trace_id` to follow account retries for one request.

`http.response.completed` records one final transport timing for every HTTP
request, including streamed responses, rejected requests, and dropped bodies.
All timings are in milliseconds:

| Field / event | Measurement |
| --- | --- |
| `duration_ms` | From receiving request headers until the response body finishes or is dropped; includes request upload and all provider attempts |
| `response_ready_ms` | From request arrival until response headers are ready; the existing `http.response.ready.duration_ms` has this meaning |
| `first_byte_ms` | From request arrival until the first nonempty response data frame is handed to the HTTP transport; absent for bodies with no data |
| `response_body_ms` | From response headers becoming ready until the body finishes or is dropped |
| `response_bytes` | Bytes handed to the HTTP transport before completion or interruption |
| `http.request.body_read.body_read_ms` | Time reading the incoming request body; `body_bytes` counts received bytes even if reading fails |
| `proxy.stream.preflight.duration_ms` | Time spent in CRS stream preflight (only in builds with `crs`) |

The completion `outcome` is `completed`, `cancelled`, or `body_error`. It describes
HTTP body delivery, so an HTTP 200 SSE error still needs its provider/stream error
event checked. The first data frame can be an SSE prelude or keepalive, rather than
the first model token; delivery timings do not measure client receipt or rendering.
Existing `proxy.*` completion events and `crosslane stats` retain their operation
timings, which can start after request upload and can include internal subrequests.
Use the final HTTP event when investigating total request latency. Large
`body_read_ms` values or HTTP 408 `request body read timed out` indicate a delay
before model dispatch; compare client upload and the network/proxy path before
tuning provider timeouts. CRS preflight can retain the initial SSE prelude for up
to five seconds while waiting for content or an early error.
Incoming body reads have a 15-second idle timeout for each chunk; an upload that
keeps making progress can take longer than 15 seconds in total.

### Kiro request size and automatic compaction

Anthropic requests have a 50 MiB HTTP body limit. The translated Kiro payload
defaults to 8 MiB (`context.max_upstream_payload_bytes`). Input windows use the
selected model's metadata when available, with `safe_input_ratio` (default 0.95)
for ordinary requests and `compact_safe_input_ratio` (default 0.99) after compaction.

With `context.auto_compact_on_overflow` enabled (the default), history that exceeds
byte or token budgets is summarized semantically before dispatch. A completed
checkpoint covers all discarded source records, including middle facts and later
corrections. Summary timeouts, capacity limits, upstream failures and checkpoints
that cannot fit return `compaction_failed` (422) without replacing the original
context. Crosslane never promotes a clipped head/tail excerpt to a completed
compaction boundary. System instructions, loaded tools, current input and its
required tool chain remain intact. If this indivisible input exceeds the window,
the 400 context error identifies `current_message` or `required_context` and
recommends reducing/splitting input or paginating results; compacting older history
cannot solve that condition.

For a single oversized paste, attachment or tool result, the error message itself
includes recovery steps so Claude Code can display them: stop resending the same
large input, split it into smaller parts or paginate tool results. Keep the same
session and try `/compact`, or send a short follow-up without the large input to
allow automatic compaction. A short follow-up may be merged with the rejected
input, and immediate `/compact` can fail while that input remains the current
turn or there are too few conversation groups to compact. If either still fails,
use Esc Esc or `/rewind` in interactive Claude Code to edit only the oversized
turn and resend it in smaller parts; earlier conversation need not be cleared.
For `required_context`, first reduce the required system instructions, enabled
tool definitions or other mandatory input; compacting history cannot shrink
them. These locally diagnosed Messages errors retain semantic status 400, `context_length_exceeded`, numeric
diagnostics and `retryable: false`. Compactable history overflow and upstream
context errors keep their native wording so client automatic compaction continues
to work. With `crs`, local context errors use HTTP 200 error JSON for non-streaming
transport, retaining 400 in `error.status` and `x-crosslane-error-status` so CRS
Console preserves recovery details. Streaming requests retain SSE error events.

Summary budgets use the selected model's window, with the configured window as a
fallback only. The latest user turn is a partitionable source record rather than
instruction overhead repeated in every part. Every part fits both estimated token
and serialized byte limits; at most 16 initial parts and two concurrent requests
share one operation deadline. A native upstream context rejection allows one repartition of
the full source into smaller parts within that same deadline and usage accounting.
If an upstream summary stream reports temporary service unavailability, or a large
source part returns an incomplete summary wrapper, only that failed source part is
retried once, using smaller parts for a large input. Small malformed summaries fail
explicitly. Successful sibling summaries are retained, partial output is discarded, and every attempt is
accounted. Child requests run sequentially within the existing concurrency limit
and deadline. `proxy.compaction.summary_part_retry.started` identifies this recovery
and its reason. Failure metadata distinguishes upstream stream errors from summary
validation and records wrapper counts without logging summary content.
`proxy.upstream.context_usage` logs upstream context percentages or native input
counts separately from local estimates; content-dependent tokenizer differences
must not be interpreted as a fixed byte or token limit.

Tool Search checks the complete continuation including new definitions and
results. While holding an account slot, it may publish only a complete verbatim
checkpoint that fits the budget. Otherwise it preserves history and rejects the
oversized continuation instead of discarding source facts or opening another
summary request. Verified history-prefix replay restores complete checkpoints for
clients that discard compaction blocks.

An ordinary Kiro response with no assistant output is retried once, then returned
as an explicit error if still empty. Valid text, tool calls, reasoning and terminal
semantics remain supported. Streaming retries do not replay visible output or
publish duplicate compaction boundaries. With `features.buffer_tool_calls=false`,
ordinary tool argument fragments still stream immediately; an error after visible
output ends that stream without replaying the upstream call.
Nonstreaming rounds release the completed upstream connection slot before
retrying or continuing server tools, including with a one-connection pool.

Large-text token estimates use bounded UTF-8 chunks with a small boundary
allowance, avoiding tokenizer exceptions on long whitespace spans. Actual messages
and tool JSON are unchanged. `/v1/messages/count_tokens` remains a local estimate
and does not start new compaction or upstream summary requests. Disabling automatic
compaction preserves limit rejection. Compaction logs identify the stage with
`payload_budget_overflow`, `pending_server_tools_payload_overflow`, or
`tool_search_continuation_overflow`.

Kiro's `CONTENT_LENGTH_EXCEEDS_THRESHOLD` and `Input content length exceeds threshold`
errors also trigger context recovery, even when local estimates fit the advertised
window. Automatic recovery retries once; client-requested summaries can recover
with automatic compaction disabled. Summary recovery partitions the complete
source and preserves the client's summary instruction. The upstream error does
not report an exact token limit and is not treated as an account failure.

Claude Code's native summary request reconciles complete, chronologically ordered
source checkpoints during its final generation. Crosslane also places these records
next to the final summary instruction, after the source history. Native final summaries omit optional verbatim duplicates
already covered by the checkpoint. This avoids a duplicate upstream
consolidation request. Explicit values from any part take precedence over another
part's local absence claims; only explicit later corrections or changed decisions
supersede earlier facts. These checkpoints are never clipped. If they cannot fit
the target or a smaller resolved window, Crosslane semantically consolidates the
saved parts within the summary input, time and usage limits. Other server compaction
requests keep their existing upstream consolidation. Logs use
`proxy.compaction.summary_merge.completed` with `merge_mode` (`client_final_summary`
or `upstream`) and `duration_ms`; upstream merges also log
`proxy.compaction.summary_merge.started`. Single-part summaries remain unchanged.

### Anonymous client and conversation attribution

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

### Claude Relay Service response compatibility

Build `crosslaned` with `cargo build -p crosslaned --bin crosslaned --release --features crs`
when its Claude Messages endpoint is used as a Claude Relay Service Console/CCR upstream. This Cargo
feature applies to every Claude Messages listener in that binary. It is disabled in the default
Cargo build.

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
With `crs` enabled it returns HTTP 200, `text/event-stream` and `event: error`;
without the feature it returns HTTP 400 JSON.

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

For Claude `stream: true` with `crs`, supplier errors use HTTP 200 and an SSE
`event: error`, preserving the original status in `error.status`,
`error.upstream_status` and `x-crosslane-error-status`. Early errors initialize a
zero-usage empty text block before the error, preventing Claude Code from falling
back to non-streaming and losing details through Console sanitization. Client
authentication, method, and HTTP body-limit failures retain their HTTP statuses.
Once a successful SSE response has started, subsequent errors remain in the
committed stream. Request statistics record failure status separately from the
HTTP 200 client transport. Crosslane checks up to 64 KiB or five seconds of initial
SSE events and enriches errors after assistant content starts.

| Symptom | First checks |
| --- | --- |
| `health` fails | Daemon/container status and logs |
| `ready` fails | Service, account, model, and quota status |
| 401 or client rejected | Bound client key and User-Agent policy |
| 503 | Account availability, quota, and concurrency |
| Stream stops early | Final stream event and trace logs |
| Context limit | Actual selected model and request size |

## Backup and recovery

Back up before upgrades or configuration changes that may affect stored state.
For the standard Docker deployment, stop writes and copy the data directory:

```bash
umask 077
backup_dir="$HOME/crosslane-backup-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$backup_dir"
docker compose stop crosslaned
docker compose cp crosslaned:/var/lib/crosslane/. "$backup_dir/"
docker compose start crosslaned
```

Verify the backup and keep it protected because it contains credentials.
For native deployments, stop the daemon and back up both the configuration
and data locations shown by `crosslane config path`. Restore into an
isolated directory or volume first, preserving ownership and permissions;
then check `ready`, accounts, services, and a real request before returning
traffic.
