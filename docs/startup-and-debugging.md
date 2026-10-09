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

A service accepts only its bound client keys. Generation routes also enforce
a client product User-Agent policy. For another client, an operator can set
`--skip-user-agent-check true` on the service or client key; key authentication
and other service limits remain in force. Check the applicable command help
before changing access:

```bash
crosslane help service edit
crosslane help apikey edit
```

## Models and quotas

```bash
crosslane models list
crosslane model disable MODEL_ID
crosslane model enable MODEL_ID
crosslane pool --model MODEL_ID --explain
crosslane account overage show
```

Use an ID from the model list. Disabled models are unavailable to new requests.
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

### Kiro request size and automatic compaction

Anthropic requests have a 50 MiB HTTP body limit. The translated Kiro payload
defaults to 8 MiB (`context.max_upstream_payload_bytes`). Input windows use the
selected model's metadata when available, with `safe_input_ratio` (default 0.95)
for ordinary requests and `compact_safe_input_ratio` (default 0.99) after compaction.

With `context.auto_compact_on_overflow` enabled (the default), oversized history
is locally compacted before byte-budget rejection, without requiring a client
compact instruction. Token overflows still use semantic summaries with a local
extractive fallback. System instructions, loaded tool definitions, current input,
and the complete tool chain needed by current results remain intact. Indivisible
input that still exceeds a hard limit receives a specific HTTP 400 error.

Tool Search previews the space recoverable from history before loading tools,
then checks the complete continuation including new definitions and results.
Oversized continuations use local compaction without acquiring a second account
slot for a summary. Streaming checkpoints are followed by references for previously
loaded tools so later tool results can still use them. Verified history-prefix
replay caches the latest checkpoint for clients that discard compaction blocks.

Large-text token estimates use bounded UTF-8 chunks with a small boundary
allowance, avoiding tokenizer exceptions on long whitespace spans. Actual messages
and tool JSON are unchanged. `/v1/messages/count_tokens` remains a local estimate
and does not start new compaction or upstream summary requests. Disabling automatic
compaction preserves limit rejection. Compaction logs identify the stage with
`payload_budget_overflow`, `pending_server_tools_payload_overflow`, or
`tool_search_continuation_overflow`.

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
build.

For a full Docker image with this compatibility enabled, build with
`docker build --target runtime-full --build-arg CROSSLANED_FEATURES=crosslaned/sso,crosslaned/crs -t crosslane:crs .`.
The full image enables `sso` without `crs` unless this build argument is set.

With `crs` enabled, Crosslane returns Anthropic-shaped JSON for abnormal upstream HTTP errors,
including HTTP 200 responses whose body is an error object. It keeps the original upstream status
in `error.upstream_status` and includes a stable `error.code` and `request_id` in the body. Generic
upstream 400/413 responses become HTTP 502; actionable request errors remain HTTP 400. For
streaming responses, Crosslane checks up to 64 KiB or five seconds of the initial SSE events
before committing HTTP 200. An error or incomplete stream in that prelude becomes a non-200 JSON
response that CRS can forward. Once assistant content has started, errors remain SSE `event: error`
with a code and request ID; a later transport failure is converted to that event.

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
