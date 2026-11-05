# Request processing

[English](request-processing.md) | [简体中文](request-processing.zh-CN.md) | [Project overview](../README.md)

This guide covers cache accounting, tool loading, latency, Kiro text redaction, and
context compaction. For deployment and accounts, see [operations](startup-and-debugging.md);
for protocol adapters, see [client compatibility](client-compatibility.md).

[Cache and usage](#prompt-cache-and-usage) · [Tool loading and latency](#request-latency-controls) · [Secret redaction](#kiro-request-secret-redaction) · [Context compaction](#context-and-automatic-compaction)

## Prompt cache and usage

`features.enable_prompt_cache` defaults to `false`. Use
`crosslane config edit features` to set it to `true`; saving validates and
reloads the configuration. Kiro normally reports credits without token/cache
counts, so Crosslane estimates cache usage locally with the tokenizer used for
input and output usage. This is usage simulation, not a claim that Kiro reused
its backend cache or discounted its credits. Actual reported credit charges
remain independent of these token estimates.

Claude requests require an explicit `cache_control: {"type":"ephemeral"}`
breakpoint or the same control at the request root. OpenAI Chat/Responses use
implicit prefix accounting without requiring this Anthropic extension. Valid
explicit controls are also translated into native Kiro `cachePoint` blocks.
Local entries are isolated by upstream account, resolved model and optional
OpenAI `prompt_cache_key`. They expire after 5 minutes (or 1 hour for an explicit
`ttl=1h` control), refresh on reuse and are lost when crosslaned restarts.
Client TTL values are local policy, not an upstream retention guarantee.

A completed cold request records cache creation. A later request can read the
longest matching prefix within 20 blocks of a breakpoint and creates only the
new suffix. Moving cache markers does not invalidate unchanged content; changing
preceding tools, roles or content does. Failed/cancelled generations and empty
refusals do not seed entries. Prefixes below the local model threshold are
ignored: 512 tokens for Opus 5/5.5 and Sonnet/Haiku 5.5; 2,048 for Opus 4.7;
4,096 for Opus 4.5/4.6 and Haiku 4.5; 1,024 otherwise. These are Crosslane's local
accounting rules. Explicit upstream cache counters, including zero, take
precedence when present.

For Claude Code and `/v1/messages`, `cache_read_input_tokens` and
`cache_creation_input_tokens` are input subsets. `input_tokens` is the remaining
uncached input; their sum equals the total input counted for accounting.
`output_tokens` counts generated output and is not a cache-write counter.
Streaming `message_start` includes the initial local cache plan; final
`message_delta.usage` and non-streaming `usage` contain completed accounting.
Request diagnostics and the `proxy.usage.accounting` log identify token/cache
sources as `estimated`, separately from `credits_source=server`.

For OpenAI Chat Completions, `usage.prompt_tokens_details` contains
`cached_tokens` (reads) and `cache_write_tokens` (writes). Responses returns
the same two fields in `usage.input_tokens_details`. Both are subsets of the input total. Chat streaming
requires `stream_options: {"include_usage": true}` to receive the final usage
chunk; Responses streaming includes usage in its terminal response event.
See the [official OpenAI cache usage documentation](https://developers.openai.com/api/docs/guides/prompt-caching#monitor-cache-performance).

On Kiro routes, `prompt_cache_key` gives both OpenAI endpoints a stable affinity
hint, hashed into the upstream conversation ID, and scopes local cache accounting.
It does not pin an account. Native Kiro cache points still use the `cache_control` extension
described above, without OpenAI retention or breakpoint-lifetime guarantees.
Kiro rejects explicit `prompt_cache_breakpoint` markers and
`prompt_cache_options` that request prewarming, a minimum `ttl`, or `mode=explicit`
with HTTP 400 before generation. A prewarm is never downgraded to a normal
generation. Null/default controls, `prewarm=false`, and legacy
`prompt_cache_retention` hints remain accepted; retention is not guaranteed.
Between native OpenAI Chat and Responses upstreams, protocol conversion preserves
`prompt_cache_options`, `prompt_cache_retention`, and content-level
`prompt_cache_breakpoint` markers when the target can represent them. Prewarming
and a 12-hour minimum lifetime require native Responses and are rejected when
the selected upstream only supports Chat or Messages.

Copilot requests upstream usage for accounting even when the Chat caller omits
or disables `stream_options.include_usage`. The HTTP boundary records those real
tokens, cache counters, and supplier bills before suppressing usage-only chunks
and billing metadata from that caller. Content, errors and `[DONE]` stay intact.
Clients that explicitly request usage still receive it. Messages and Responses
retain their normal usage events.
Internal statistics normalize Messages input as ordinary input plus cache reads
and writes. OpenAI input totals already include those subsets and are preserved.
The public responses retain each protocol's own input-token semantics.

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

### Copilot model discovery and timeouts

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

### Kiro transport and retries

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

## Kiro request secret redaction

### Configuration

Edit `[features]` with `crosslane config edit features`:

```toml
[features]
enable_request_redaction = true
enable_request_redaction_notice = false
# request_redaction_notice_text = "Custom Markdown notice"
```

Saving validates and hot-reloads new requests. Check the running values with
`crosslane config show features --effective`. In-flight tool rounds, retries, and
summaries retain their original policy. Redaction defaults to enabled; disabling
it sends new message, history, tool, and summary values to Kiro unchanged. Old
checkpoint capsules still decode locally. Configuration exports and log sanitization
have their own rules and are not controlled by this switch.

### Coverage and false positives

Redaction runs locally at the Kiro transport boundary, without a model or another
network request. It covers current input, system/history messages, tool arguments
and results, descriptions and schema examples, automatic summaries, retries, and
Kiro MCP search queries. Recognized credentials become request-local placeholders.
Equal values share a placeholder; mappings stay in memory and are not logged or
persisted. Context and byte budgets include the redacted text and reference overhead.

| Input | Handling |
| --- | --- |
| Credential fields, assignments, and known account credentials | Sensitive values and their exact copies are masked, including copies inside larger strings |
| Cloud credentials | Recognizes characteristic AWS, Tencent, Alibaba, Volcengine/Kingsoft, and Google prefixes; opaque secrets need credential names such as `SecretKey`, `AccessKeySecret`, `ak`, or `sk` |
| Authentication syntax | Masks auth headers, cookies, credential URLs, connection-string passwords, private keys, and common curl, HTTPie/xh, wget, database, netrc, Docker/npm, and SDK credential forms |
| Environment references | Preserves recognized variable references, credential-file paths, and public authentication controls; literal defaults and concatenated secrets remain masked |
| JSON and text attachments | Decodes JSON field names and UTF-8 txt/md/csv/html attachments before masking; malformed JSON with sensitive keys is handled conservatively |
| Binary attachments | PDF, Office, and image contents are not parsed or OCRed |

Reference handling follows the declared shell or source language. For example,
POSIX single-quoted `$TOKEN` is a literal, while a supported shell expansion is a
reference. Recognized source-code getters in Go, Java, .NET, Rust, Ruby, PHP, Deno,
Bun, Node, and Python may retain variable names; getter-looking text in literal
credentials or JSON tool parameters does not gain an exemption. No command, getter,
environment expansion, or credential-file read is executed during detection.

Short, common, or example values are not exempt. Ambiguous fields such as
`key: "Enter"`, `cache_key: "product:42"`, and `password: "true"` can be masked.
Arbitrary unlabeled passwords, unknown formats, and encoded or split secrets cannot
be reliably recognized. The rules cannot guarantee zero false positives or misses.

### Tool restoration and checkpoints

The model must copy placeholders exactly into tool arguments. The gateway parses
complete JSON arguments and restores string, number, and boolean values locally,
including fragmented parallel calls. References the model repeats in ordinary answer text are also restored before reaching the client; a reference split across stream chunks is held until complete. Reasoning stays masked so its signature remains valid. Recognized
POSIX shell tools receive context-aware escaping; ordinary HTTP/JSON parameters do
not receive shell quoting. Unchanged command/JSON replay preserves source escaping,
whitespace, and newlines.

Arbitrary heredocs, nested substitutions, PowerShell, and languages embedded in
shell arguments are not generally interpreted for restoration. Use structured
credential parameters or executor environment variables for those cases.

Internal retries, tool continuations, and summaries share the logical request's
mapping. Independent HTTP requests do not. Clients must replay restored tool
parameters or retain checkpoint capsules so a later request can rebuild its mapping.
Changed or unknown placeholders cannot be guessed back into credentials.
Checkpoint capsules carry reversibly encoded credentials; **they are not encryption**.
Treat them as secret-bearing conversation data.

### Optional final-answer notice

`enable_request_redaction_notice` defaults to false and requires a `crs` build.
When enabled, masking the latest user input, tool result, or text attachment in a
successful generation can append a local notice to the final Claude text block.
Historical credentials alone do not trigger it. The complete default Markdown is
visible in the configuration editor; custom text is preserved without substitution,
and an empty/whitespace value suppresses the notice.

The notice is ordinary answer text, uses no model, and contains no credential or
placeholder values. Tool responses, compaction summaries, errors, incomplete or
truncated answers, structured/JSON outputs, OpenAI responses, and other providers
are excluded. Tool IDs, arguments, stop reasons, and reported usage are unchanged.

Complete default notice suffixes and locally recognized custom suffixes are removed
from assistant history before forwarding, and an existing notice is checked before
appending. Custom suffix recognition uses bounded in-memory fingerprints without
storing answer text. Unknown custom suffixes after restart or expiry are retained.
Disabling redaction also disables notices.

## Context and automatic compaction

| Setting or limit | Default |
| --- | --- |
| Incoming Messages HTTP body | 50 MiB |
| `context.max_upstream_payload_bytes` | 8 MiB after Kiro translation |
| `context.safe_input_ratio` | 0.95 of the selected model's input window |
| `context.compact_safe_input_ratio` | 0.99 after compaction |
| `context.auto_compact_on_overflow` | `true` |

The selected model's metadata determines the input window; the configured window
is a fallback. Token counts are local estimates, not an upstream acceptance guarantee.
`/v1/messages/count_tokens` does not generate summaries or start new compaction.

### What can be compacted

When history exceeds the byte or token budget, Crosslane summarizes complete source
records before dispatch. A published checkpoint must cover all discarded records,
including middle facts and later corrections. Failed, timed-out, or oversized
checkpoints return `compaction_failed` (422) while preserving the original context.
Clipped head/tail excerpts are not accepted as completed checkpoints.

System instructions, loaded tools, current input, and its required tool chain remain
intact. If these alone exceed the window, the 400 `context_length_exceeded` error
identifies `current_message` or `required_context`, includes numerical diagnostics,
and sets `retryable: false`. Compacting old history cannot shrink that input.
CRS Console can transport this local error as HTTP 200 while preserving semantic
status 400; see [CRS error transport](client-compatibility.md#claude-relay-service-response-compatibility).

### Recover an oversized turn

1. Stop resending the same oversized paste, attachment, or tool result. Split it or
   paginate the tool output.
2. Keep the session and try Claude Code's `/compact`, or a short follow-up without
   the large input. A client may merge that follow-up into the rejected turn, and
   `/compact` can fail when that turn remains current or there is too little history.
3. If needed, use Esc Esc or `/rewind` in interactive Claude Code to edit only that
   turn, then resend smaller parts while retaining earlier conversation.
4. For `required_context`, reduce mandatory system instructions, loaded tools, or
   other required input before retrying.

### Summary execution and diagnostics

Summary parts preserve source text and speaker labels and must fit both token and
serialized-byte limits. At most 16 initial parts and two concurrent requests share
one operation deadline. One bounded recovery may repartition complete source or
retry only a failed part after narrowly identified transient/format failures.
Successful sibling summaries are retained; partial output is not published. Explicit
refusals, authentication, and quota errors are not retried. Accepted attempts settle
their usage even when the overall operation fails.

With `crs`, upstream occupancy can conservatively calibrate local estimates for an
account/model/conversation/checkpoint, expiring after 30 minutes. It does not change
billed tokens, client output limits, or advertised model windows. Native Kiro
`CONTENT_LENGTH_EXCEEDS_THRESHOLD` errors also enter bounded context recovery;
they do not reveal a precise token limit or mark an account unhealthy.

Claude Code's native final summary consolidates complete chronological checkpoints.
Explicit source values take precedence over another part's local absence claim;
only a later explicit correction supersedes a prior fact. Checkpoints that cannot
fit are semantically consolidated, never clipped. Other server compaction paths
retain upstream consolidation.

Tool Search validates the complete continuation, including new definitions and
results. While holding an account slot, it publishes only a complete verbatim
checkpoint that fits; otherwise it preserves history and rejects the oversized
continuation. Verified history-prefix replay can restore checkpoints omitted by a client.

| Log field or event | Purpose |
| --- | --- |
| `payload_budget_overflow`, `pending_server_tools_payload_overflow`, `tool_search_continuation_overflow` | Identify the admission stage |
| `proxy.compaction.summary_part_retry.started` | Failed-part recovery and its reason |
| `proxy.compaction.summary_merge.completed` | Merge duration and `merge_mode` (`client_final_summary` or `upstream`) |
| `proxy.upstream.context_usage` | Upstream occupancy/counts, separate from local estimates |

Disabling automatic compaction retains limit rejection. Client-requested summaries
can still use their bounded upstream-context recovery path.
