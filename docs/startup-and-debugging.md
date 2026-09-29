# Deployment, operations and CLI migration

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [Project overview](../README.md)

This guide follows current source, including unreleased changes after `v0.2.4`.
Start with the project README for initial setup. This reference covers environment,
accounts, persisted state, Docker/systemd, CLI migration and recovery. Run commands
from the repository root; `kproxy` means a matching native binary or the installed Docker wrapper.

- [Environment](#2-environment-loading), [accounts](#4-add-or-import-accounts), [configuration](#6-configuration-and-hot-reload)
- [Docker](#8-docker-compose), [systemd](#9-systemd), [backup and recovery](#back-up-and-restore)
- [CLI migration](#migration-from-the-old-cli), [logs](#7-logs-and-trace-ids), [troubleshooting](#troubleshooting)

## 1. Prerequisites

Native builds need rustup, a C toolchain and a linker; the repository pins Rust 1.97.1.
Docker deployments need Engine and the Compose plugin, with host networking enabled
when using Docker Desktop. Default builds include Chromium SSO; `--no-default-features`
disables browser login. Build/test commands are in [CONTRIBUTING.md](../CONTRIBUTING.md);
editor debugging settings are in [.vscode](../.vscode).

## 2. Environment loading

Copy the example before starting a local daemon:

```bash
cp .env.example .env  # First setup only; preserve an existing .env
```

`kproxyd` loads `.env` before parsing startup arguments. `kproxy` first parses
local navigation such as help, guides, completion, and version; it loads `.env`
before reparsing and running any business command. Both search from the current
directory upward, which lets commands launched from a workspace subdirectory
reuse the repository-level file.

Environment precedence is:

1. Variables already present in the process environment.
2. Values loaded from the nearest `.env` found while searching upward.
3. `config.toml` values for settings that have a persistent equivalent.
4. Built-in application defaults.

An existing process variable is never overwritten by `.env`. A missing `.env`
is allowed; a malformed or unreadable file fails daemon startup and business
commands, while local CLI navigation remains available.

The example uses `KPROXY_HOME=.kproxy-dev` to isolate development files. The most
important process-level variables are:

| Variable | Purpose |
| --- | --- |
| `KPROXY_HOME` | Places configuration, data, logs, and the generated admin socket below one directory. |
| `KPROXY_HTTP_PORT` | Overrides a configured proxy service port that matches the `server.port` default; it does not create a service. |
| `KPROXY_DISABLE_HTTP=1` | Prevents configured proxy services from listening while leaving their configuration and the Unix administration socket intact. |
| `KPROXY_ADMIN_SOCKET` | Overrides the socket used by the `kproxy` CLI; it does not reconfigure `kproxyd`. |
| `KPROXY_CODEWHISPERER_URL` | Overrides the CodeWhisperer upstream URL for integration tests or controlled proxies. |
| `KPROXY_AMAZONQ_URL` | Overrides the Amazon Q upstream URL for integration tests or controlled proxies. |
| `RUST_LOG` | Sets tracing filters for console and application diagnostics. |
| `RUST_BACKTRACE` | Enables Rust backtraces when set to `1` or `full`. |

Use an absolute `KPROXY_HOME` when launching from different directories. Loading
the same `.env` does not rebase relative paths: `.kproxy-dev` is relative to the
process working directory. CLI socket precedence is `--socket`, process environment,
`.env`, then configuration/defaults. The [environment template](../.env.example)
also documents MCP/runtime/management overrides and CLI credential input.

Use `config.toml` instead of `.env` for persistent service, pool, model, API-key,
TLS, notification, and logging configuration.

## 3. Start locally with native binaries

```bash
cargo build --workspace --locked
cargo run -p kproxyd
```

In another terminal at the same root, run `cargo run -p kproxy -- health`.
For later commands, use `./target/debug/kproxy` or put that directory in `PATH`.
Use `cargo build --release --locked` for optimized binaries. Only one daemon may
own a socket: stale sockets are cleaned up, but reachable sockets are preserved.
`KPROXY_DISABLE_HTTP=1` disables business listeners while retaining their configuration and the admin socket.

## 4. Add or import accounts

Import enterprise SSO credentials from a JSON file or stdin:

```bash
kproxy account import --file accounts.json --tag team-a
cat accounts.json | kproxy account import --stdin --tag team-a --tag prod
```

`id`, `machine_id`, and `created_at` may be omitted; the CLI generates them.
Repeat `--tag` or use comma-separated values to merge tags into every account in
this import; per-account `tags` already present in JSON are preserved.
To tag accounts that already exist, pass multiple IDs or emails to one command:

```bash
kproxy account tag --add test acc_2c36cfad acc_332c7cb2 acc_41c6e3ad
```

Every target is validated before the batch is saved: if any account does not
exist or needs a tag change while bound to a proxy service, none are changed.
The existing single-account form remains available.

The JSON below is a shape example, not usable credentials. Replace tokens and
`expires_at` (Unix seconds) with values issued by the upstream provider.

```json
[
  {
    "email": "user@example.com",
    "credentials": {
      "access_token": "...",
      "refresh_token": "...",
      "client_id": "...",
      "client_secret": "...",
      "region": "us-east-1",
      "expires_at": 1893456000,
      "auth_method": "idc"
    }
  }
]
```

### Kiro headless API keys and regional runtime

Set `KIRO_API_KEY` securely in the CLI's environment, then import it without
putting the secret in a command argument:

```bash
kproxy account add-api-key --email ci@example.com --region us-east-1 --tag ci
# Alternatively read the key from standard input:
kproxy account add-api-key --email ci@example.com --region eu-central-1 --key-stdin < /secure/kiro-key
```

The key is stored as `credentials.access_token` with `auth_method: "api_key"`
and `expires_at: 0`. Do not attach OAuth refresh/client secrets or a profile ARN.
Keys use the regional `runtime.{region}.kiro.dev` service, `TokenType: API_KEY`,
and no OAuth refresh or token-refresh alerts. Revoked keys require manual rotation;
re-importing does not overwrite an existing account. API-key model discovery
uses the static catalog because the management API requires an OAuth profile.
Without discovered effort metadata, thinking controls retain the conservative
omission policy in the [protocol reference](protocol-compatibility.md).

Kiro key echoes are redacted from normalized errors and upstream error formatting,
including background diagnostics; raw credentials must still not be shared.

OAuth defaults retain the existing regional Q/CodeWhisperer routes; set
`upstream.preferred_endpoint = "runtime"` to prefer regional Kiro runtime and
management RPC. GovCloud and API-key credentials use runtime without a legacy
endpoint fallback. GovCloud profile discovery cannot substitute a commercial
Builder ID profile. Test/deployment overrides `KPROXY_RUNTIME_URL` and
`KPROXY_MANAGEMENT_URL` accept `{region}`. Setting `KIRO_API_KEY` on the daemon
alone does not import an account.

Account exports contain credentials by default. Use `--redact` before sharing
diagnostic output:

```bash
kproxy --json account export --redact
```

### Enterprise SSO authentication

The default `kproxyd` build and Docker Compose enable all features, including the
Chromium-based enterprise IAM Identity Center login. First set a global start
URL with `kproxy config edit`:

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

Manual account additions can then omit `--start-url`:

```bash
printf '%s\n' "$PASSWORD" | kproxy account add-sso \
  --email user@example.com \
  --tag team-a \
  --password-stdin

kproxy account add-sso --batch accounts.csv -c 1 --tag team-a

# Read explicitly from stdin for pipelines and automation:
kproxy account add-sso --batch - -c 1 < accounts.csv
```

Use `--start-url` to override the global value for one login. When a smaller
binary without browser SSO is explicitly desired, build with
`cargo build --workspace --no-default-features` or select Docker's
`runtime-slim` target.
The Docker host wrapper automatically recognizes a readable host CSV and
streams it into the container through stdin, without copying or retaining a
password file. A container path is still read normally when no host file with
the same name exists. Passwords are accepted only from stdin or a two-column
CSV file. In batch mode, every `--tag` supplied on the command is applied to all
accounts in that batch. Add
`--headful` when MFA or an upstream page change requires manual interaction.
Every login uses a dedicated incognito Chromium context and temporary profile,
which are destroyed before the next account is processed. Before saving an
account, kproxy records Kiro's stable user ID and refuses to register that same
identity under another email. Kiro display names are diagnostic only because
IAM Identity Center names do not always match login email addresses.
This flow does not add support for non-enterprise or non-SSO accounts.

## 5. Create and verify a proxy service

A new daemon creates no business listener. `health` checks liveness; `ready` checks
accounts, listeners, meter recovery and background-task heartbeats. Neither proves
that upstream generation works. Save the client API key returned by service creation:

```bash
kproxy service create --name main --host 127.0.0.1 --port 5580
kproxy service list
kproxy service apikeys main
kproxy ready
kproxy models list
```

Select an account pool by one or more tags when creating a service. Multiple
tags form a union: an account with any selected tag is included. A service
without `--account-tag` continues to use the global account pool. Tag accounts
before they become bound to a service; if you created the global-pool `main`
service above, exclude these accounts from it first:

```bash
kproxy service remove-account main alice@example.com bob@example.com
kproxy account tag alice@example.com --add team-a
kproxy account tag bob@example.com --add team-b
kproxy service create --name team-a --host 127.0.0.1 --port 5581 --account-tag team-a team-b
kproxy service accounts team-a
kproxy account services alice@example.com

# Explicitly include an account with any tag, or exclude one from this service:
kproxy service add-account team-a carol@example.com
kproxy service remove-account team-a alice@example.com
```

The effective pool is “any selected tag matches + explicit additions - exclusions”; exclusion
wins. Without a service tag, the base set contains every account. Health,
readiness, model discovery, initial dispatch, retries, and compaction summaries
all stay within the service's effective pool. A Kiro account that belongs to any
service cannot have its tags changed or be deleted. Copilot also protects bound
accounts from deletion and tag changes that remove them from an existing pool;
adding labels to a global/manual pool is allowed. Inspect bindings with
`account services`, then unbind with `service remove-account` when required.
Stop a service before changing its pool tag:

```bash
kproxy service disable team-a
kproxy service edit team-a --account-tag team-b team-c  # or --clear-account-tag for the global pool
kproxy service enable team-a
```

Omitting `--host` defaults to `0.0.0.0`. `service apikey <service>` (alias of
`service apikeys`) displays full keys by default, including JSON output. Use
`--redact` to hide them; the old `--show-secret` flag is still accepted.
Services accept only bound API keys. Messages defaults
to Claude Code admission; Chat/Responses default to Codex. To allow another client,
use `service edit` or `apikey edit` with `--skip-user-agent-check true`. Key authentication,
quota and concurrency limits still apply. Request examples are in the [README](../README.md)
and [Responses guide](openai-responses.md).

## 6. Configuration and hot reload

### Files and persistence

Use `.env` for startup-path selection and temporary process overrides. Use
`config.toml` for persistent service, pool, model, API-key, TLS, logging, and
notification settings. See [`.env.example`](../.env.example) for every supported
example variable and its purpose.

Set `KPROXY_HOME` to place configuration, data, logs, and the administration socket
under one directory. Without `KPROXY_HOME`, XDG locations are used:

| File | Default location | Notes |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/kproxy/` | Human-edited daemon configuration. |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Contains credentials; created with mode `0600`. |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Daily credit accounting, reset on UTC boundaries. |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Persisted aggregate request statistics. |
| `stats-history/` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | One-minute request aggregates split into bounded UTC hourly shards. |
| `alert-incidents.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Persisted alert incident suppression; include it in backups. |
| `web-search-replay.key` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | AES-256-GCM replay key; created with mode `0600` and never overwritten. |
| `admin.sock` | `${XDG_RUNTIME_DIR}/kproxy/` or `/run/kproxy/` | Local administration plane. |
| Logs | `${XDG_DATA_HOME:-~/.local/share}/kproxy/logs/` | Split by UTC date and severity. |

On first startup, missing files are created without overwriting existing data.
Valid configuration changes are hot-reloaded. Invalid TOML or validation
failures leave the last valid configuration active. `server.host` and
`server.port` are defaults for newly created proxy services. Changes to
`admin.socket` or the shared HTTP/HTTPS listening mode require a daemon restart;
most other fields, including the proxy service list, apply without one.

External account-file changes are also reloaded. Corrupt account data never
replaces the valid in-memory snapshot. Large account stores can use a gzip
envelope plus incremental sidecar updates according to the storage settings.

### Configuration commands

```bash
kproxy config path
kproxy config show --effective
kproxy config validate
kproxy config edit
kproxy config reload
kproxy models resolve claude-sonnet-4.5
```

Configuration edits and service/apikey/alert/model-map mutations share validation,
transaction locks, atomic writes and hot reload. Change process overrides in the
environment and restart. Upgrades retain existing values instead of replacing them
with new defaults. Model discovery runs at startup, after account changes and on
cache expiry; account status tasks only refresh credits. Conditional model maps use
the selected account's credits; `--below-credits-percent` rules without a schedule apply all day.
Alerts use `--platform` and `--webhook-url`; see `kproxy alert platforms` for platform
options. `alert edit --event` replaces subscriptions. Ongoing incidents are deduplicated
until recovery; same-kind account incidents are briefly aggregated.
Use `kproxy help --all` and `kproxy guide` for the complete command tree and topics.

## 7. Logs and trace IDs

```bash
kproxy logs show --tail 100
kproxy logs follow --level warn
kproxy logs trace <TRACE_ID>
kproxy logs path
kproxy logs files --level error
kproxy status --since 30m
kproxy stats --detail --since 1h --by endpoint
```

Response headers `x-trace-id` and `request-id` identify a request across dates and
levels. Files are split by exact level and UTC date, with a default 100 MB shard
size and three-day retention; `info.log` excludes WARN/ERROR. Trace queries have
scan/output limits. The Docker wrapper adds host paths for logs within the data
volume. Use `RUST_LOG` or `log.level` for more detail; application logs omit prompt,
response and key values.

`status` covers the current daemon run; `stats` defaults to persisted totals.
Both accept `--since` or timezone-qualified `--start/--end`. Minute aggregates are
stored in UTC hourly shards under `stats-history/` and retained indefinitely, so
monitor disk growth. History already pruned by older releases cannot be recovered.
Use logs for individual failures; `stats --detail` is not a complete request audit.

## 8. Docker Compose

Published images currently target Linux amd64 full. `v0.2.4` is a version-selection
example; check registry availability before use. Build current source to validate
unreleased features. Setup pulls first, replaces the container, waits for health,
then installs the matching host wrapper:

```bash
./deploy/docker-setup.sh --image ghcr.io/yaocool/kiro-proxy:v0.2.4
kproxy version
kproxy ready
```

The default wrapper destination is `/usr/local/bin/kproxy`; use
`--target "$HOME/.local/bin/kproxy"` for a user-owned path. Later,
`./deploy/docker-upgrade.sh` follows `latest`; retain `--image` for version pinning.
`./deploy/install-kproxy-wrapper.sh` installs only the wrapper and refuses to replace unrelated commands by default.

Compose uses host networking, supported directly on Linux Engine; Docker Desktop
4.34+ needs it enabled in settings. Services bind host ports directly and default
to `0.0.0.0`; choose `127.0.0.1` for local-only access. The `kproxy-data` volume mounts
at `/var/lib/kproxy`. Do not mount development `.kproxy-dev` or `.env.example` into it.
Existing bridge deployments need container recreation to adopt the new network mode;
the volume is retained. Inspect deployment or deliberately build source with:

```bash
docker compose config --quiet
docker compose ps
docker compose logs -f kproxyd
# Source build only:
docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build
```

Full pairs Chromium `r1566079` with `chromiumoxide 0.9.1` and sets the container
no-sandbox option. Changing the build override target to `runtime-slim` removes
browser SSO; CI currently does not publish this target. Local builds default to
`CARGO_BUILD_JOBS=1`. The image includes vim; `EDITOR` may select another installed editor.

The wrapper requires a working Docker engine and an identifiable deployment. Select
among multiple deployments with `KPROXY_COMPOSE_PROJECT` or `KPROXY_DOCKER_CONTAINER`.
Stopped-container navigation uses the exact local image without network or business
mounts. Missing images, ambiguous targets and old images without local navigation
fail explicitly. Business commands need a running daemon. The wrapper preserves
exit codes/stdin, allocates a TTY for interactive use, and passes or falls back from `TERM`.

`kproxy restart` waits for health; `kproxy stop` retains the deployment.
`kproxy uninstall` stops service, backs up state, then deletes the container, volume,
unshared image and wrapper. Backups default to `~/.kproxy/backups`. Failed backup
restarts the container and preserves data. `uninstall --yes` retains the backup;
only explicit `--delete-backup` removes it. `docker compose down` retains volumes;
`down -v` deletes them.

For stale volume metadata with a confirmed missing directory, setup offers
`--repair-volume` only for a volume labeled for this project; interactive use asks
for confirmation. Restore broken data mounts or Docker storage roots first.
Failed deployment health triggers an attempt to restore the old image. Missing old
images or failed rollback health require operator recovery. **Image rollback reuses
the volume and does not undo writes or migrations.** [Back up full state](#back-up-and-restore)
before upgrading. Restart also clears Responses state. Check version, readiness,
accounts, services, effective configuration, statistics and a generation request after upgrading.

## 9. systemd

Build release binaries and install them with the provided unit:

```bash
cargo build --release --locked

sudo useradd --system --user-group --home-dir /var/lib/kproxy --shell /usr/sbin/nologin kproxy
sudo install -m 0755 target/release/kproxyd target/release/kproxy /usr/local/bin/
sudo install -m 0644 deploy/kproxyd.service /etc/systemd/system/kproxyd.service
sudo systemctl daemon-reload
sudo systemctl enable --now kproxyd
sudo systemctl status kproxyd
```

If the `kproxy` user already exists, skip `useradd`. The unit uses `/etc/kproxy`,
`/var/lib/kproxy`, and `/run/kproxy` through systemd-managed directories.

```bash
sudo -u kproxy kproxy --socket /run/kproxy/admin.sock status
sudo journalctl -u kproxyd -f
sudo systemctl reload kproxyd
```

Reload sends `SIGHUP`. Restart-required settings still need
`sudo systemctl restart kproxyd`.

Install Chrome or Chromium on the host before using `kproxy account add-sso`.
The provided unit supports the default full build: it intentionally leaves user
namespaces and executable JIT memory available for Chromium, while retaining
`NoNewPrivileges`, filesystem protection, an empty capability set, and the other
hardening controls. If the host kernel disables unprivileged user namespaces,
prefer enabling them. As a last-resort compatibility override, set
`KPROXY_CHROMIUM_NO_SANDBOX=1` with `systemctl edit kproxyd`; this disables
Chromium's own sandbox and should only be used after reviewing the host's
isolation boundary.

## Navigation and scripting

`kproxy`, `kproxy help`, and bare groups such as `kproxy logs` display help.
Use `kproxy help logs trace` for nested options, `kproxy help --all` for the public
command tree, and `kproxy guide` for operational topics. These commands and
`completions`/`version` work without a daemon or a valid `.env`.

Business output supports global `--json`. A bare group with `--json` exits 2,
leaves stdout empty, and asks for an explicit action on stderr. Explicit help
still prints text and exits 0. Success normally exits 0, argument errors exit 2,
and connection/execution failures exit 1; `ready` also fails when not ready.

Examples below are independent operations, not a script to run from top to bottom.
Replace IDs, names, paths and secrets with your own values. `diagnose all` and
`diagnose account` perform real upstream inference and consume quota; use `health`,
`ready`, `account list`, and `logs show` for initial inspection.

## Migration from the old CLI

Provider scope is shared across the command tree: `kproxy --provider copilot
account list`, `kproxy account --provider copilot list` and the leaf form are
equivalent. `-p` is the short form. `KPROXY_PROVIDER` sets the shell default;
explicit flags override it. The value is the configured instance ID shown by
`provider list`, not the driver kind or the display name “GitHub Copilot”.
For example, an instance created with `--id work-cp --kind copilot` uses
`--provider work-cp`. `provider show work-cp` prints commands using that ID;
`guide copilot -p work-cp` also produces matching setup examples.
The Docker wrapper forwards this shell default and recognizes provider options
before or after command groups when streaming a host CSV. Update the installed
wrapper together with the CLI to use these forms.

Nested help and `help --all` identify shared, Kiro-only, Copilot-only, global and
resource-selected commands. Global operations and commands that locate a
resource by positional ID/name ignore `KPROXY_PROVIDER` and reject an explicit
`--provider` instead of silently ignoring it. Global model disable/enable retains
its stricter check: override a shell scope with `--provider all`. Kiro tasks accept
only `kiro`; model-cache refresh accepts an instance ID; global tasks do not take
a provider. `tasks list` labels the scope of each task.

For `service edit`, `apikey edit`, and `model-map edit`, provider permission changes
require an explicit `--provider`; the shell default never replaces existing
permissions. `--provider` and `--clear-providers` cannot be combined.
When `service edit --provider ...` excludes the current default provider, the first
new ID becomes the default unless `--default-provider` is supplied. Existing defaults
still in scope are retained. API key permissions remain independent.

Use `account add --provider kiro --auth sso/api-key` or `--provider copilot
--auth device-flow` for the shared authentication entry point; the old Kiro
commands remain available. See the [provider guide](providers-and-copilot.zh-CN.md#统一和按来源管理).

`pool` and `account refresh/probe/reset-health --all` now default to all providers;
pass `--provider kiro` to retain the previous scope. `account export` still
defaults to the legacy Kiro format; request `--provider all` for unified export.
Service account listing, tags, explicit bindings and exclusions now include
Copilot. Qualify ambiguous account references with `provider/account_id`.

| Previous form | Current form |
| --- | --- |
| `kproxy logs --tail 100` | `kproxy logs show --tail 100` |
| `kproxy logs -f` / `--follow` | `kproxy logs follow` |
| `kproxy models --refresh --mapped` | `kproxy models list --refresh --mapped` |
| `kproxy tasks` (data query) | `kproxy tasks list` |
| `kproxy diagnose` (full diagnosis) | `kproxy diagnose all` |
| `kproxy help balance` | `kproxy guide balance` |
| `account add-sso-batch --file FILE` | `account add-sso --batch FILE` |
| `alert add --kind ... --url ...` | `alert add --platform ... --webhook-url ...` |
| Alert platform `wechat` | `wechat-work` (check `alert platforms` for platform-specific options) |

Removed options fail before business initialization. Bare `logs`, `models`,
`tasks`, and `diagnose` now show group help, including in redirected output.
`logs show` and `logs follow` already existed before the migration; the change
removes the parent-level shortcuts. See the [changelog](../CHANGELOG.md).

Daemon resource deletion requires interactive `y` or `yes`; it has no general `--yes` switch. Docker uninstall has separate options.

## Back up and restore

Record the source version/commit, actual image ID/digest, Compose project and
configuration/data paths. Back up **both** configuration and data directories
when using separate XDG paths. A backup must include:

- `config.toml` and accounts storage, including any incremental/compressed sidecars;
- `daily.json`, `stats.json`, `stats-history/` and `alert-incidents.json`;
- `web-search-replay.key`, so existing proxy-owned search replay remains readable;
- retained logs needed for incident review.

For the standard Docker volume, use a maintenance window and stop writes before
copying. Example (run each step after verifying the previous result):

```bash
umask 077
backup_dir="$HOME/kproxy-backups/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$backup_dir"
docker compose stop kproxyd
docker compose cp kproxyd:/var/lib/kproxy/. "$backup_dir/"
docker compose start kproxyd
```

Check the copy exit status and inspect required files before treating it as a
backup. Restart the original service if copying fails; do not proceed with an
upgrade until a valid backup exists. Native deployments should stop the daemon
and copy their resolved configuration/data roots with permissions preserved.

For recovery, retain the failed deployment's data for diagnosis, restore the
pre-upgrade backup into a separate empty data directory/volume, preserve the
service account's ownership and restrictive permissions (the Docker image uses
UID/GID 10001), and start the original image against that restored state. Avoid
merging old snapshots with files written by a newer version. Recheck config,
accounts, key bindings, counters, readiness and a test request before switching
traffic. The admin socket is recreated by the daemon and is not backup state.

There is no general downgrade compatibility guarantee yet. An account export
alone is not a full backup. `docker compose down -v` deletes the named volume;
the wrapper's `uninstall` is also destructive even though it first creates a backup.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Port already in use | Compare `service list` with actual listeners; change the service port, not just the default for new services. |
| Cannot connect to `admin.sock` | Check daemon, user, absolute `KPROXY_HOME`, `config path` and `--socket`. |
| Configuration unchanged | Run `config validate` and `config show --effective`; check environment overrides, retained defaults and restart-only settings. |
| 401 / client rejected | Check the service's bound key, product User-Agent and exemptions. |
| 503 | Inspect `ready`, credits/protection thresholds, concurrency, background tasks and `logs trace`. |
| Stream interrupted | Inspect the protocol's final event and Trace ID; HTTP 200 does not establish success. |
| Context overflow | Use `models resolve` and `error.context`; see [compaction boundaries](protocol-compatibility.md#automatic-compaction-and-model-windows). |
| Old container version | Compare actual image ID and `kproxy version`; pull the intended release or deliberately build source. |
