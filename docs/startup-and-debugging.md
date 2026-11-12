# Crosslane operations

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [Project overview](../README.md)

This guide covers deployment, accounts, services, models, routine troubleshooting,
and backup. Run commands from the repository root with a CLI matching the daemon.

| Task | Guide |
| --- | --- |
| Deploy or upgrade | [Requirements](#requirements-and-configuration), [Docker](#docker) |
| Add accounts or create services | [Accounts](#accounts), [Services and client access](#services-and-client-access) |
| Manage models and quotas | [Models and quotas](#models-and-quotas) |
| Troubleshoot or restore data | [Logs](#logs-and-troubleshooting), [Backup and recovery](#backup-and-recovery) |
| Configure caching, tool loading, redaction, or compaction | [Request processing](request-processing.md) |
| Connect clients, web search, or CRS | [Client compatibility](client-compatibility.md) |
| Automate administration or manage Kiro subscriptions | [Admin REST](admin-rest.md), [Kiro Enterprise administration](kiro-enterprise-admin.md) |

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

`-h`, `--help`, and `help` show command options and examples without a running daemon.
The Docker wrapper still needs Docker and the local deployment image.

```bash
crosslane service edit -h
crosslane help service edit
crosslane service help edit
crosslane help --all
crosslane help account --all
```

`help --all` lists command paths, aliases, and scopes. To see the options for a
specific action, use its path with `help`, such as `crosslane help service edit`.

| Command group | Managed resources |
| --- | --- |
| `provider` | Upstream instances, driver settings, default models, and fallback policy |
| `account` | Upstream logins, credentials, health, and tags |
| `service` | Business listeners, allowed providers, client key bindings, and account pools |
| `apikey` | Client credentials, provider/model permissions, limits, and usage |
| `models` / `model-map` | Model discovery and global disabling / client model mapping rules |
| `admin` | Admin tokens and Kiro enterprise AWS credentials, users, and subscriptions |
| `tasks` / `alert` | Scheduled tasks / shared notification targets and current Kiro events |

Command groups that require an action display help when invoked on their own.
Scripts using `--json` must select an action in those groups. `account overage`
and `logs admin` default to `show`. `tasks list` identifies manually runnable tasks.
`tasks run` help and Shell completion list those tasks.
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
SSO, run `crosslane config edit sso` and set your organization's start URL.

```toml
[sso]
start_url = "https://example.awsapps.com/start"
login_timeout_secs = 120
```

`login_timeout_secs` sets how long to wait for the browser's authorization
callback. It takes a positive integer in seconds and defaults to 120. Saving in
the CLI editor reloads the setting for subsequent login attempts.

```bash
crosslane config show sso --effective
crosslane account add-sso --email user@example.com --password-stdin < /secure/kiro-sso-password
```

Use `crosslane account add-sso --help` for interactive, batch, and stdin
options. Check accounts with `crosslane account list` and
`crosslane account refresh --help`. Export output is redacted by default;
use secret-bearing exports only in protected backups.

Kiro batch login uses its own two-column `email,password` CSV, supporting quoted
fields and a UTF-8 BOM. `crosslane account add --provider kiro --auth sso --batch
accounts.csv` submits the same IAM credentials as single-account login. Per-account
progress goes to stderr; `--json` keeps one final result on stdout. Copilot Device
Flow and its GitHub/Azure credential columns are separate from Kiro login.
If the identity provider requires a credential update, background login reports
the specific blocker immediately. Complete that organization sign-in step, then
retry with the current login password. Crosslane does not change organization
passwords; `add-sso --headful` remains available for manual browser verification.

For Kiro endpoint and protocol details, see [client compatibility](client-compatibility.md#kiro-endpoints-and-upstream-refusals).

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

## Independent status HTTP

Process status endpoints use a dedicated listener, independent of API proxy services and their
TLS, client keys, and User-Agent policies. Add this section to the file shown by
`crosslane config path`:

```toml
[status_http]
enabled = true
host = "127.0.0.1"
port = 7651
```

The listener is disabled by default. When enabled, its default address is `127.0.0.1:7651`.
Use `crosslane config edit status-http` or `config edit admin` to edit this listener and
its optional REST authentication together; missing defaults are supplied for older files.
Changes to this section apply through hot reload; `config reload` also applies manual edits.
A failed bind retains the current listener and runtime configuration. Business services keep running.
Change `host` to the appropriate listening address for remote
probes. Docker host networking needs no additional port mapping.

| Method and path | Purpose |
| --- | --- |
| `GET /` | Process name, version, and status |
| `GET /health` | Liveness, untagged account counts, and each service's name, enabled flag, and listener running flag |
| `GET /ready` | Required background tasks, metering recovery, and the listener and effective account pool of every enabled service; `200` when ready, `503` otherwise |

```bash
curl http://127.0.0.1:7651/health
curl -i http://127.0.0.1:7651/ready
```

These endpoints read local snapshots without contacting upstreams or refreshing credentials, and
require no client key or product-specific User-Agent. With no proxy services, `/health` still
returns `200`, while `/ready` returns `503` with reasons. For both Kiro and Copilot, health counts
include only accounts whose own `tags` array is empty, including disabled and unavailable accounts,
with separate counts for each health state. Shared accounts are counted once. A service without
`account_tag` also uses only untagged accounts, matching the health scope apart from manual additions.
Public health responses omit credits, service IDs, provider metadata, bindings, and raw errors.
Each `services` entry contains only `name`, `enabled`, and `running`. Readiness still evaluates
each service's actual provider scope, tags, manual bindings, and exclusions, so tagged service
pools remain eligible even when health reports zero untagged accounts.

After migration, `/`, `/health`, and `/ready` on business service ports return `404`; move existing
HTTP probes to the dedicated status port. Automatic service port allocation skips the enabled
status port. Model protocol endpoints, model discovery, and the client compatibility endpoint
`/api/event_logging/batch` remain on business ports. CLI management continues over the Unix socket.

## Services and client access

The independent listener can also expose `/admin/v1` with separate Bearer authentication
for account/provider/tag management, service lifecycle, grouped pools and quotas.
See [Admin REST API](admin-rest.md) for configuration and endpoints.

### Create a listener

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

### Provider routing and failover

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

### Client authentication

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

## Logs and troubleshooting

```bash
crosslane logs show --tail 100
crosslane logs follow --level warn
crosslane logs trace TRACE_ID
crosslane logs admin show --tail 100
crosslane logs admin show --request-id admin_0123456789abcdef0123456789abcdef
crosslane logs files --admin
crosslane status --since 30m
crosslane stats --detail --since 1h
```

Admin events go to separate `crosslaned-admin-DATE-LEVEL.log` files; Service and
other daemon events remain in `crosslaned-DATE-LEVEL.log`. A custom `[log].file_path`
keeps both files in the same directory and adds `-admin` to the Admin base name.
Both use the same level, rotation, and retention settings. `logs admin show`
reads recent records from the Admin files; `logs admin` retains the same query. Both
can filter by the response's `x-request-id` (`admin_...`). Admin's `--level` selects
the physical log level, and HTTP audit entries, including 4xx/5xx responses, are
written at `info`. `logs files --admin` and `logs path --admin` show their locations.
Earlier versions' Admin events remain in their original files.

`logs show` and `logs follow` filter recent Service requests by exact provider
instance ID, exact `account_id`, and HTTP status. To see all providers, leave
`--provider` and `CROSSLANE_PROVIDER` unset. With `--json` they emit one JSON object
per request (NDJSON), including the `account_id` value to use with `--account`.
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

| Symptom | First checks |
| --- | --- |
| `health` fails | Daemon/container status and logs |
| `ready` fails | Service, account, model, and quota status |
| 401 or client rejected | Bound client key and User-Agent policy |
| 503 | Account availability, quota, and concurrency |
| Stream stops early | Final stream event and trace logs |
| Context limit | Actual selected model and request size |

For context limits and compaction, see [request processing](request-processing.md#context-and-automatic-compaction).
For tools, web search, and CRS error delivery, see [client compatibility](client-compatibility.md).
For management errors and response export, see [Admin REST](admin-rest.md#save-and-inspect-json-responses).

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
