# Admin REST API

The API uses the independent HTTP listener, normally
`http://127.0.0.1:7651/admin/v1`, alongside public `/health` and `/ready`.
It remains available without any business service. Business ports do not serve administration.
Resources are grouped as Accounts, Services, Providers, Operations, and APIKeys.
`GET /admin/v1/openapi.json` provides an authenticated OpenAPI 3.1 document.

## Enable and authenticate

Use `crosslane admin token` to manage multiple independent credentials. Commands run over
its existing Unix socket and do not require a pre-existing Bearer token. These credentials
are separate from model-client keys managed by `crosslane apikey`.

For initial setup, configure the independent listener in the file from `crosslane config path`
and restart the daemon:

```toml
[status_http]
enabled = true
host = "127.0.0.1"
port = 7651
```

Create the first token and enable administration using the explicit `--enable-rest` flag:

```bash
crosslane admin token create --name ops --enable-rest
crosslane admin token create --name monitor --role read-only
crosslane admin token list
crosslane admin token show ops
```

`--enable-rest` requires an already enabled/running status listener. It does not open a new
port or reconfigure business services. Without it, creation retains the current admin switch.
Token changes apply to subsequent requests immediately, without restart or a separate reload.

| Command | Purpose |
| --- | --- |
| `admin token create --name NAME` | Create; `--role admin` is the default, `add` is an alias |
| `admin token list` | Metadata and admin/listener enabled state |
| `admin token show ID_OR_NAME` | One token's metadata |
| `admin token rotate ID_OR_NAME` | Return a fresh secret; invalidate the old one, preserving ID/name/role/enabled state |
| `admin token disable ID_OR_NAME` | Disable and retain the record |
| `admin token enable ID_OR_NAME` | Re-enable a disabled record |
| `admin token revoke ID_OR_NAME` | Permanently remove; `delete` and `rm` are aliases |

Multiple `admin` or `read-only` credentials can remain active simultaneously, allowing named
callers to rotate or revoke independently. Commands reject `--provider` and ignore the
`CROSSLANE_PROVIDER` default. Revoking the last token disables Admin REST while retaining public
status and business configuration. Already authenticated requests and accepted batch jobs
are not automatically cancelled by revocation.

Token changes take effect only after the configuration write succeeds. On a write error,
the previous file is verified or restored before returning; a rollback that cannot be verified
is reported explicitly. These failures emit `config.persistence_failed` without logging secrets.

Plaintext is returned only by successful create/rotate, never recoverable through list/show.
Only SHA256 digests are persisted. Neither metadata commands nor their JSON output expose
secrets or hashes. Automation can read `secret` from
`crosslane --json admin token create --name ci`; do not log that creation response.
Audit events record token ID, role and action; authenticated `admin.http.request` events add
`token_id` to distinguish callers.

Send the plaintext token, never its digest or a query-string token:

```bash
BASE=http://127.0.0.1:7651/admin/v1
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/providers"
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/openapi.json"
```

Full access permits all admin operations; read-only permits GET / HEAD and returns `403` for
writes. Missing, invalid, disabled or revoked credentials return `401`; disabled Admin REST
returns `404`. Model-client keys grant no management access. Authentication covers the schema,
unknown routes and unsupported methods. The listener uses HTTP, independently of business TLS;
use an HTTPS reverse proxy or SSH tunnel for remote access.

Legacy manual digest configuration remains supported:

```toml
[admin.rest]
enabled = true
token_sha256 = "REPLACE_WITH_64_HEX_SHA256_DIGEST"
# read_token_sha256 = "REPLACE_WITH_DISTINCT_READ_ONLY_DIGEST"
```

These appear as `legacy-admin` / `legacy-read-only` and support show, rotate, disable, enable
and revoke. Changing a legacy entry materializes only that credential in the named list,
retaining its ID, role and validity except for the requested rotation/revocation; the other
legacy credential is unaffected. New tokens use `[[admin.rest.tokens]]`, supporting up to 128
named tokens in addition to the two legacy compatibility identities.

For manual digest generation:

```bash
python3 - <<'PYTOKEN'
import hashlib
import secrets
token = secrets.token_urlsafe(32)
print("Bearer token:", token)
print("SHA256:", hashlib.sha256(token.encode()).hexdigest())
PYTOKEN
```

Use `crosslane config reload` after manual digest changes. Listener address/port changes still
require restart. Full help: `crosslane help admin token`; guide: `crosslane guide admin`.

## Accounts

`provider` means a configured instance ID such as `kiro` or `copilot-team-a`, rather than
an arbitrary driver name. Discover instances with `/providers`. Use the returned account ID
in paths and prefer qualified `provider/id` references for service bindings.

| Method and path, relative to `/admin/v1` | Purpose |
| --- | --- |
| `GET /accounts` | Filter, paginate, and group accounts |
| `POST /accounts` | Create an account with provider, authentication type, and initial tags |
| `GET /accounts/{provider}/{account}` | Safe metadata, health, tags, and quota |
| `DELETE /accounts/{provider}/{account}` | Delete; returns `409` while a service uses the account unless `?force=true` |
| `POST /accounts/{provider}/{account}/enabled` | Enable/disable with `{"enabled":true}` or `false` |
| `POST /accounts/{provider}/{account}/tags` | Update with `{"add":["team-a"],"remove":["old"]}` |
| `POST /accounts/{provider}/{account}/refresh` | Refresh credentials and available quota data |
| `POST /accounts/{provider}/{account}/probe` | Probe; may make upstream requests and consume quota |
| `POST /accounts/{provider}/{account}/reset-health` | Reset runtime health |
| `GET /accounts/{provider}/{account}/services` | Show services using the account |

Single-account paths require a configured provider instance ID; `all` is only a list filter.
Deletion returns `data` with `removed` (the canonical account ID), `provider_id`, and `reference`
for both drivers, without returning the removed provider's raw account metadata.

Account control operations and service bindings resolve identities and return metadata from
local snapshots. Account/pool queries and explicit refresh/probe use their usual upstream
paths; authentication during creation still validates against the provider. This keeps a
slow quota endpoint from holding the configuration transaction during enable, tag, or delete.

Create a Kiro API key account:

```bash
curl -X POST "$BASE/accounts" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"provider":"kiro","auth":"api-key","email":"account@example.com","token":"ksk_REPLACE_ME","tags":["team-a"]}'
```

Create a Copilot account using a GitHub token. Upstream validation and token exchange run
before persistence, without holding the configuration lock. If a hot reload replaces that
provider during validation, creation returns `409` without saving the account; retry against
the current provider configuration.

```bash
curl -X POST "$BASE/accounts" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"provider":"copilot-team-a","auth":"token","token":"REPLACE_WITH_GITHUB_TOKEN","label":"team account","tag":"team-a"}'
```

`tags` accepts an array and `tag` accepts a single value; both are combined, trimmed and
deduplicated. Tags are part of the first durable write, preventing temporary membership in an
incorrect service pool. At most 64 tags, each 1–128 characters, are accepted. Kiro's optional
`region` defaults to `us-east-1`; `machine_id` is generated or accepts 64 hexadecimal characters.

Kiro account writes verify durable contents after storage errors. If the complete change was
saved and only stale-delta cleanup failed, the operation succeeds and logs
`account.persistence_committed_after_error`. Partially written changes are rolled back to the
previous account generation; an unverifiable rollback is reported as an error.

Kiro OAuth import uses `auth: "oauth"`, optional `profile_arn`, `machine_id`, `label`, `tags`,
and a `credentials` object:

```json
{
  "provider": "kiro",
  "auth": "oauth",
  "email": "account@example.com",
  "tags": ["team-a"],
  "credentials": {
    "auth_method": "idc",
    "access_token": "REPLACE_ME",
    "refresh_token": "REPLACE_ME",
    "client_id": "REPLACE_ME",
    "client_secret": "REPLACE_ME",
    "region": "us-east-1",
    "expires_at": 2000000000
  }
}
```

`auth_method` is `idc` or `social`; refresh support depends on valid, complete credentials.
This imports credentials rather than starting an interactive OAuth flow. Builds with `sso`
also accept `auth: "sso"`, `email`, `password`, HTTPS `start_url`, optional `region` and `tags`.
Omitted `start_url` uses `[sso]` defaults. The request waits for the existing SSO flow; allow
time for login. Builds without `sso` return `501`. Account creation/deletion updates existing
durable storage and the running account pool.

List filters: `provider`, `tag`, `status` (health), `enabled_only`, `untagged_only`.
`sort` accepts `name`, `id`, or `credit`; the default is natural display-name order.
`tag` and `untagged_only=true` cannot be combined. `offset` defaults to 0, `limit` to 100
(1–1000). Responses include unique-account `total`, `has_more`, `complete`, provider `errors`,
and `groups`. Unique accounts are paginated before grouping by `provider` (default) or `tag`.
Multi-tag accounts appear in each matching tag group; untagged accounts use `tag: null`.
Consequently, summed group counts may exceed the unique-account count.

```bash
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  "$BASE/accounts?provider=kiro&tag=team-a&group_by=tag&offset=0&limit=100"
```

## Bulk creation and automatic SSO

`POST /account-batches` accepts bulk creation. Kiro SSO uses the exact CSV parser and
`account.addSso` entry point used by
`crosslane account add --provider kiro --auth sso --batch accounts.csv`: launch the browser,
complete IAM Identity Center authentication, exchange credentials, validate upstream identity
and quota, then persist the account. Pre-obtained access tokens are unnecessary. It requires
the `sso` feature and the same Chromium environment as the CLI. Tenant-required MFA or other
manual steps follow the existing login flow.

Upload the same local CSV file directly; paths are not read from the server filesystem:

```csv
email,password
user1@example.com,REPLACE_ME
user2@example.com,"REPLACE_WITH_PASSWORD_CONTAINING,COMMA"
```

```bash
curl -X POST \
  "$BASE/account-batches?provider=kiro&auth=sso&tag=team-a&concurrency=4" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: text/csv' \
  --data-binary @accounts.csv
```

Omitted `start_url` uses `[sso].start_url`, matching the CLI. Query parameters can override
URL-encoded `start_url` and `region` (default `us-east-1`). CSV supports a UTF-8 BOM, CRLF,
optional headers, commas and escaped quotes, retaining password whitespace. The two columns
remain `email,password`; query `tag` sets a common tag. JSON supports multiple common tags.

JSON accepts either a `csv` string or an `accounts` array, exclusively:

```json
{
  "provider": "kiro",
  "auth": "sso",
  "start_url": "https://example.awsapps.com/start",
  "tags": ["team-a", "production"],
  "concurrency": 4,
  "accounts": [
    {"email": "user1@example.com", "password": "REPLACE_ME"},
    {"email": "user2@example.com", "password": "REPLACE_ME", "tag": "special"}
  ]
}
```

Array rows also support Kiro API keys, OAuth imports, and Copilot token imports described
above. Row-level `provider`, `auth`, `start_url`, and `region` override common defaults;
row tags combine with common `tags` / `tag`. CSV is only for Kiro SSO; other modes use JSON.

| Method and path | Purpose |
| --- | --- |
| `POST /account-batches` | Submit; returns `202`, `data.id`, and the task URI in `Location` |
| `GET /account-batches` | Retained summaries, without per-row details |
| `GET /account-batches/{batch}` | Progress and ordered per-row outcomes, successful account IDs/references, sanitized errors |
| `POST /account-batches/{batch}/cancel` | Stop queued rows; already-started logins finish and clean up their browsers normally |

```bash
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  "$BASE/account-batches/batch_REPLACE_WITH_RETURNED_ID"
```

All inputs are validated before starting any login. Invalid format/parameters cause no
login side effects. Once started, one account's failure does not prevent later accounts;
successes are retained, matching CLI bulk behavior. Reports include `pending`, `running`,
`succeeded`, `failed`, `cancelled`, and `completed` counts. `rows` retain input order and contain
`account_id`, `reference`, row status and errors. `complete=true` requires all rows to succeed.
Terminal states are `completed`, `completed_with_errors`, and `cancelled`; intermediate states
are `queued`, `running`, and `cancelling`. `finished_at` identifies a terminal report.

Each batch accepts 1–1000 accounts and at most 2 MiB. Submission returns immediately.
Concurrency defaults to 1, accepts 1–8, and shares a global eight-account creation limit with
single REST creation. At most four unfinished batches are accepted; overflow returns `503`.
Single-account creation also returns `503` immediately when all creation slots are occupied,
leaving management requests available while accepted batches wait for their turn.
Shared login defaults are validated before expansion: `region` accepts 1–64 bytes without
whitespace, and `start_url` is limited to 8192 bytes, including the configured SSO default.
Up to 32 reports are retained, with finished reports expiring after one hour. Submission and
cancellation require full access; a read-only token may query reports.

Tasks and pending passwords are process-local. Reports and audit logs never store CSV,
passwords or tokens. Restart clears tasks and pending inputs without automatic replay;
successful accounts and tags remain durable. Disconnecting after submission does not cancel
an accepted task; use its ID to query progress.

## Services

| Method and path | Purpose |
| --- | --- |
| `GET /services` | List; optional `provider` filter |
| `POST /services` | Create a business listener and client key |
| `GET /services/{service}` | Configuration, running state, grouped effective pool, account quotas, key metadata |
| `PATCH /services/{service}` | Persist and start/stop with `{"enabled":true}` or `false` |
| `DELETE /services/{service}` | Delete; remove exclusive client keys and retain shared keys |
| `GET /services/{service}/accounts` | Effective account pool |
| `POST /services/{service}/accounts` | Add manual bindings and clear their exclusions |
| `DELETE /services/{service}/accounts` | Remove manual bindings and exclude accounts; does not delete accounts |
| `GET /services/{service}/api-keys` | Key metadata without plaintext secrets |

`service` accepts ID or name. An exact ID takes precedence over another service's name;
use that other service's own ID to address it unambiguously. CLI service commands use the
same rule, and edit results retain the selected stable ID after a rename.
Writes resolve the input selector inside the configuration transaction. Pool and key reads
verify returned service IDs, returning `404` if a stale ID would match a neighbor's name.
Detail/pool queries accept `group_by=provider|tag` and optional
`provider`. Pools respect allowed providers, tags, manual additions, and exclusions. They do
not inherit public health's untagged-only filter. A service without `account_tag` uses the
global pool, including tagged accounts. Multiple service tags match any selected tag.

Service mutations use the latest on-disk configuration and preserve pending client-key
changes. Both the running configuration and the persisted restart configuration are checked;
automatic ports reserve the running and pending status-listener ports. Write failures verify
or restore the previous configuration file; unverified rollback is reported explicitly.

```bash
curl -X POST "$BASE/services" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"team-a","host":"127.0.0.1","providers":["kiro"],"default_provider":"kiro","tags":["team-a"]}'
```

`providers` aliases `allowed_providers`; `tags` aliases `account_tag`, which also accepts
a string. Omitted ports are allocated automatically, skipping the enabled status port.
Optional fields include `port`, `skip_user_agent_check`, `api_key_name`, and `api_key_format`
(`sk`, `simple`, `token`). The `201` response contains `data.service` and the newly generated
`data.api_key.key`. **Save that plaintext client key: it is returned only at creation.**
Subsequent GETs never retrieve it. Admin Bearer tokens and model-client keys are independent.
Failed listener creation/enabling rolls back without leaving a partial service or key.

Binding and unbinding both take:

```json
{"accounts":["kiro/acc_12345678","copilot-team-a/copilot_ACCOUNT_ID"]}
```

Accepts 1–1000 references; their providers must be allowed for the service. Alternatively,
include top-level `provider` with unqualified account IDs. Unbinding persists exclusions so
global/tag selection cannot immediately add the account back. By default, deletion while any
service uses an account is rejected. Use `?force=true` to unbind it from every service before
deletion, including disabled services and global, tag, and manual pools, for both Kiro and
Copilot. Exclusions remain to prevent a re-imported account with the same ID from silently
rejoining those services. Other accounts and service settings are preserved. Deletion failure
rolls back the unbinding; disk failures that prevent rollback are reported. In-flight requests
are not cancelled.

```bash
curl -X DELETE 'http://127.0.0.1:7651/admin/v1/accounts/kiro/acc_12345678?force=true' \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

The CLI equivalent is `crosslane account rm acc_12345678 --provider kiro --force --yes`;
multiple account references are supported. `--force` controls unbinding; bypassing confirmation
still requires `--yes`. Tag changes that remove an account from a bound tag pool continue to
require explicit unbinding first.

## Quota semantics

Account projections never expose access/refresh tokens, client secrets, passwords,
machine identifiers, or raw provider details. Unknown `quota` quantities remain `null`.

| Provider | `quota.unit` | `remaining` |
| --- | --- | --- |
| Kiro | `kiro_credits` | Effective configured limit minus usage, clamped at zero; includes allowed overage capacity |
| Copilot token billing | `ai_credits` | Personal upstream balance, preferring fractional `quota_remaining` |
| Copilot legacy quota | `premium_requests` | Personal premium request balance |

Kiro reads the local usage cache; `quota.updated_at` is its update time. Refresh the account
or run `status_check` to update it. For Kiro API keys, refresh only updates usage and does not
attempt to renew the key. Copilot reuses the existing five-minute quota cache;
`updated_at` is the latest quota query time, and `cached=true` indicates an older snapshot
retained after a failed query. Unlimited/zero placeholders do not establish organization
balances: `total` and `remaining` remain `null`.

Copilot `local_aiu_usage` separately tracks locally observed billing, retaining exact integer
`total_nano_aiu` (1 AIU = 1,000,000,000 nano AIU), `billed_requests`, `first_recorded_at`, and
`last_recorded_at`. It is `null` before billing has been observed. Different quota units are
not summed. Failed refresh/probe operations return `502`; successful action responses add
`operation` with applicable `refreshed`, `persisted`, and `usage_refreshed` flags.

## Other management groups

| Group | Method and path | Purpose |
| --- | --- | --- |
| Providers | `GET /providers`, `GET /providers/{provider}` | Configured instances, runtime state, capabilities |
| Providers | `GET /models` | Model catalog; optional `provider` |
| Operations | `GET /runtime/status` | Full management status; optional `provider` |
| Operations | `GET /runtime/stats` | Usage; `provider`, `since_secs` or `start_secs` / `end_secs`, `detail` |
| Operations | `GET /runtime/tasks` | Background task state |
| Operations | `POST /runtime/tasks/{task}/run` | Run an existing task; optional `provider` where the task supports it |
| APIKeys | `GET /api-keys` | Client-key metadata and usage; optional `provider` |
| APIKeys | `POST /api-keys/{key}/reset-usage` | Reset client-key usage by ID or name; preserves cumulative account AIU |

Client-key selectors also prefer an exact ID before a name alias. This applies to REST usage
resets and CLI show/edit/enable/disable/limit/delete commands. Deletion removes only the one
selected key; it never removes a second key whose name matches the selected ID.

Discover task names with `/runtime/tasks`, for example `status_check`, `token_refresh`,
and `model_cache_refresh`. Public `/health` still only reports untagged account counts and
brief service state, without quotas or service IDs.

## Responses and audit

Successes use `{"data":...}`, `201` for single resource creation, `202` for batch submission,
and `200` otherwise. OpenAPI returns its
standard root object. Errors use `{"error":{"code":"...","message":"..."}}`:
`400` invalid input, `401` unauthenticated, `403` read-only restriction, `404` missing/disabled,
`405` unsupported method, `409` binding/duplicate conflicts or a provider replaced during
account validation, `413` oversized body,
`502` failed upstream action, `503` temporarily unavailable, `501` unsupported build feature.
Upstream error messages pass through the existing credential sanitizer.

Bodies are limited to 2 MiB and authenticated concurrency to 16 requests; overflow returns
`503`. Responses include `Cache-Control: no-store` and `x-request-id: admin_...`.
The `admin.http.request` audit event records request ID, method, route template, role, status,
and duration. It excludes Bearer tokens, bodies, query strings, and raw account path parameters.
