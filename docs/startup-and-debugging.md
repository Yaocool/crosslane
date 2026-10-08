# Deployment, operations and CLI migration

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [Project overview](../README.md)

This guide follows current source, including unreleased changes after `v0.2.4`.
Start with the project README for initial setup. This reference covers environment,
accounts, persisted state, Docker/systemd, CLI migration and recovery. Run commands
from the repository root; `crosslane` means a matching native binary or the installed Docker wrapper.

- [Environment](#2-environment-loading), [accounts](#4-add-or-import-accounts), [configuration](#6-configuration-and-hot-reload)
- [Docker](#8-docker-compose), [systemd](#9-systemd), [backup and recovery](#back-up-and-restore)
- [CLI migration](#migration-from-the-old-cli), [logs](#7-logs-and-trace-ids), [troubleshooting](#troubleshooting)
- [Crosslane rename migration](#crosslane-rename-migration)

## Crosslane rename migration

Crosslane replaces the former `kiro-proxy` project identity. This is a breaking
deployment rename; the daemon does not automatically move or select old state.

| Previous name | Crosslane name |
| --- | --- |
| CLI `kproxy`, daemon `kproxyd` | `crosslane`, `crosslaned` |
| Crates/directories `kproxy-*` | `crosslane-*` |
| Environment variables `KPROXY_*` | `CROSSLANE_*` with the same suffix |
| Development home `.kproxy-dev` | `.crosslane-dev` |
| XDG config/data/runtime subdirectory `kproxy` | `crosslane` |
| systemd `kproxyd.service`, user/group `kproxy` | `crosslaned.service`, user/group `crosslane` |
| `/etc/kproxy`, `/var/lib/kproxy`, `/run/kproxy` | `/etc/crosslane`, `/var/lib/crosslane`, `/run/crosslane` |
| Compose project `kiro-proxy`, service `kproxyd`, volume `kproxy-data` | `crosslane`, `crosslaned`, `crosslane-data` |
| Image `ghcr.io/yaocool/kiro-proxy` | `ghcr.io/yaocool/crosslane` |
| `deploy/kproxy-docker`, `deploy/install-kproxy-wrapper.sh` | `deploy/crosslane-docker`, `deploy/install-crosslane-wrapper.sh` |
| Saved deployment image `deploy/.kproxy-image` | `deploy/.crosslane-image` |
| Docker label `io.kiro-proxy.role`, diagnostic headers `x-kproxy-*` | `io.crosslane.role`, `x-crosslane-*` |
| Unreported-usage marker `kproxy_usage_source` | `crosslane_usage_source` |

1. Stop the old daemon before copying state. For Docker, use the old checkout's
   Compose file and project name to stop `kproxyd`. Back up the complete config
   and data directories, including `providers/`, statistics, policies, and
   `web-search-replay.key`. Do not delete the old Docker volume.
2. For native development, move `.kproxy-dev` to `.crosslane-dev` and update the
   existing `.env`, preserving its values. For XDG/systemd installations, copy the
   config and data into the new directories and grant the new service user access.
   Change environment variable names and `RUST_LOG` module filters (`kproxyd` →
   `crosslaned`, `kproxy_*` → `crosslane_*`). `KIRO_API_KEY` is an upstream Kiro
   credential variable and keeps its name.
3. Inspect the copied `config.toml`: update `admin.socket`, `log.file_path`, TLS
   certificate paths and provider-specific file paths that refer to moved
   directories. Existing configuration is not rewritten by bootstrap. Keep
   provider IDs, account IDs, model policy, credentials and client API keys intact.
4. For Docker, restore the full backup into the new project's data volume
   **before starting** `crosslaned`. The default volume is
   `crosslane_crosslane-data`, mounted at `/var/lib/crosslane`; custom project
   names produce different volume names. Read the actual name with
   `docker compose config`. The container still uses UID/GID `10001`; preserve
   that ownership. Update old absolute paths in the restored config. Merely
   changing `--project-name` does not migrate the differently named volume.
5. Install matching Crosslane binaries or the new Docker wrapper; regenerate
   shell completion using `crosslane completions`. When reusing a custom wrapper
   path, `install-crosslane-wrapper.sh --target PATH` recognizes the old managed
   wrapper without `--force`; the default destination is now `/usr/local/bin/crosslane`.
   Install and start
   `crosslaned.service` for systemd. Select a Crosslane image explicitly instead
   of copying the old saved image reference; pre-rename images lack the new
   executable names and container paths. Use a source build until the renamed
   image is published. Image rollback does not migrate state between identities.
6. Verify `crosslane version`, `crosslane health`, `crosslane config path`,
   `crosslane provider list`, `crosslane account list --provider all`, and
   `crosslane model list`. Keep the stopped old deployment and backup until
   account counts, service/API-key access and disabled-model policy are confirmed.

The API routes and persisted account/key formats retain their existing meanings.
New Web Search replay records carry `crosslane.v2.`; old `kproxy.v2.` records are
still authenticated and decrypted with the original replay key. Retaining this
key is necessary to replay earlier conversations.
Stable upstream session IDs retain their pre-rename hash namespace so existing
client conversations keep the same identity after upgrading.

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

`crosslaned` loads `.env` before parsing startup arguments. `crosslane` first parses
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

The example uses `CROSSLANE_HOME=.crosslane-dev` to isolate development files. The most
important process-level variables are:

| Variable | Purpose |
| --- | --- |
| `CROSSLANE_HOME` | Places configuration, data, logs, and the generated admin socket below one directory. |
| `CROSSLANE_HTTP_PORT` | Overrides a configured proxy service port that matches the `server.port` default; it does not create a service. |
| `CROSSLANE_DISABLE_HTTP=1` | Prevents configured proxy services from listening while leaving their configuration and the Unix administration socket intact. |
| `CROSSLANE_ADMIN_SOCKET` | Overrides the socket used by the `crosslane` CLI; it does not reconfigure `crosslaned`. |
| `CROSSLANE_CODEWHISPERER_URL` | Overrides the CodeWhisperer upstream URL for integration tests or controlled proxies. |
| `CROSSLANE_AMAZONQ_URL` | Overrides the Amazon Q upstream URL for integration tests or controlled proxies. |
| `RUST_LOG` | Sets tracing filters for console and application diagnostics. |
| `RUST_BACKTRACE` | Enables Rust backtraces when set to `1` or `full`. |

Use an absolute `CROSSLANE_HOME` when launching from different directories. Loading
the same `.env` does not rebase relative paths: `.crosslane-dev` is relative to the
process working directory. CLI socket precedence is `--socket`, process environment,
`.env`, then configuration/defaults. The [environment template](../.env.example)
also documents MCP/runtime/management overrides and CLI credential input.

Use `config.toml` instead of `.env` for persistent service, pool, model, API-key,
TLS, notification, and logging configuration.

## 3. Start locally with native binaries

```bash
cargo build --workspace --locked
cargo run -p crosslaned
```

In another terminal at the same root, run `cargo run -p crosslane -- health`.
For later commands, use `./target/debug/crosslane` or put that directory in `PATH`.
Use `cargo build --release --locked` for optimized binaries. Only one daemon may
own a socket: stale sockets are cleaned up, but reachable sockets are preserved.
`CROSSLANE_DISABLE_HTTP=1` disables business listeners while retaining their configuration and the admin socket.

## 4. Add or import accounts

Import enterprise SSO credentials from a JSON file or stdin:

```bash
crosslane account import --file accounts.json --tag team-a
cat accounts.json | crosslane account import --stdin --tag team-a --tag prod
```

`id`, `machine_id`, and `created_at` may be omitted; the CLI generates them.
Repeat `--tag` or use comma-separated values to merge tags into every account in
this import; per-account `tags` already present in JSON are preserved.
To tag accounts that already exist, pass multiple IDs or emails to one command:

```bash
crosslane account tag --add test acc_2c36cfad acc_332c7cb2 acc_41c6e3ad
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
crosslane account add-api-key --email ci@example.com --region us-east-1 --tag ci
# Alternatively read the key from standard input:
crosslane account add-api-key --email ci@example.com --region eu-central-1 --key-stdin < /secure/kiro-key
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
Builder ID profile. Test/deployment overrides `CROSSLANE_RUNTIME_URL` and
`CROSSLANE_MANAGEMENT_URL` accept `{region}`. Setting `KIRO_API_KEY` on the daemon
alone does not import an account.

Account exports now redact credentials by default. Explicitly add
`--include-secrets` only for a protected credential backup (`--redact` remains
accepted for older scripts):

```bash
crosslane --json account export
```

### Enterprise SSO authentication

The default `crosslaned` build and Docker Compose enable all features, including the
Chromium-based enterprise IAM Identity Center login. First set a global start
URL with `crosslane config edit`:

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

Manual account additions can then omit `--start-url`:

```bash
printf '%s\n' "$PASSWORD" | crosslane account add-sso \
  --email user@example.com \
  --tag team-a \
  --password-stdin

crosslane account add-sso --batch accounts.csv -c 1 --tag team-a

# Read explicitly from stdin for pipelines and automation:
crosslane account add-sso --batch - -c 1 < accounts.csv
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
account, crosslane records Kiro's stable user ID and refuses to register that same
identity under another email. Kiro display names are diagnostic only because
IAM Identity Center names do not always match login email addresses.
This flow does not add support for non-enterprise or non-SSO accounts.

## 5. Create and verify a proxy service

A new daemon creates no business listener. `health` checks liveness; `ready` checks
accounts, listeners, meter recovery and background-task heartbeats. Neither proves
that upstream generation works. Save the client API key returned by service creation:

```bash
crosslane service create --name main --host 127.0.0.1
crosslane service list
crosslane service apikeys main
crosslane ready
crosslane models list
```

Select an account pool by one or more tags when creating a service. Multiple
tags form a union: an account with any selected tag is included. A service
without `--account-tag` continues to use the global account pool. Tag accounts
before they become bound to a service; if you created the global-pool `main`
service above, exclude these accounts from it first:

```bash
crosslane service remove-account main alice@example.com bob@example.com
crosslane account tag alice@example.com --add team-a
crosslane account tag bob@example.com --add team-b
crosslane service create --name team-a --host 127.0.0.1 --account-tag team-a team-b
crosslane service accounts team-a
crosslane account services alice@example.com

# Explicitly include an account with any tag, or exclude one from this service:
crosslane service add-account team-a carol@example.com
crosslane service remove-account team-a alice@example.com
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
crosslane service disable team-a
crosslane service edit team-a --account-tag team-b team-c  # or --clear-account-tag for the global pool
crosslane service enable team-a
```

Omitting `--host` defaults to `0.0.0.0`. Omitting `--port` uses `server.port`
for the first service, then one above the highest configured service port
(including disabled services). An external listener can still occupy that port;
use `--port` to override it. `service apikey <service>` (alias of
`service apikeys`) hides full keys by default, including JSON output. Use
`--show-secret` to reveal them; `--redact` remains accepted for older scripts.
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

Set `CROSSLANE_HOME` to place configuration, data, logs, and the administration socket
under one directory. Without `CROSSLANE_HOME`, XDG locations are used:

| File | Default location | Notes |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/crosslane/` | Human-edited daemon configuration. |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/crosslane/` | Contains credentials; created with mode `0600`. |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/crosslane/` | Daily credit accounting, reset on UTC boundaries. |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/crosslane/` | Persisted aggregate request statistics. |
| `stats-history/` | `${XDG_DATA_HOME:-~/.local/share}/crosslane/` | One-minute request aggregates split into bounded UTC hourly shards. |
| `alert-incidents.json` | `${XDG_DATA_HOME:-~/.local/share}/crosslane/` | Persisted alert incident suppression; include it in backups. |
| `web-search-replay.key` | `${XDG_DATA_HOME:-~/.local/share}/crosslane/` | AES-256-GCM replay key; created with mode `0600` and never overwritten. |
| `admin.sock` | `${XDG_RUNTIME_DIR}/crosslane/` or `/run/crosslane/` | Local administration plane. |
| Logs | `${XDG_DATA_HOME:-~/.local/share}/crosslane/logs/` | Split by UTC date and severity. |

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
crosslane config path
crosslane config show --effective
crosslane config validate
crosslane config edit
crosslane config reload
crosslane models resolve claude-sonnet-4.5
```

`config show` redacts credentials and omits source comments by default. Use
`config show --show-secrets` only when the complete original configuration is
needed. `config show --effective` uses the daemon's last applied configuration,
including while the on-disk TOML is invalid. With `--json`, it returns only
`path` and `effective_json`. `config edit --json` returns a structured result.
`config reload` exits nonzero when the daemon rejects the new file.

`service list` shows effective ports (the expected port for a stopped service).
Without `--port`, service creation chooses one above the highest effective port, including a
`CROSSLANE_HTTP_PORT` override. If `CROSSLANE_DISABLE_HTTP=1`, creation reports
that the listener is not running. `status --watch --json` and
`pool --watch --json` produce newline-delimited JSON snapshots.

Configuration edits and service/apikey/alert/model-map mutations share validation,
transaction locks, atomic writes and hot reload. Change process overrides in the
environment and restart. Upgrades retain existing values instead of replacing them
with new defaults. Model discovery runs at startup, after account changes and on
cache expiry; account status tasks only refresh credits. Conditional model maps use
the selected account's credits; `--below-credits-percent` rules without a schedule apply all day.
Alerts use `--platform` and `--webhook-url`; see `crosslane alert platforms` for platform
options. `alert edit --event` replaces subscriptions. Ongoing incidents are deduplicated
until recovery; same-kind account incidents are briefly aggregated.
Use `crosslane help --all` and `crosslane guide` for the complete command tree and topics.

## 7. Logs and trace IDs

```bash
crosslane logs show --tail 100
crosslane logs follow --level warn
crosslane logs trace <TRACE_ID>
crosslane logs path
crosslane logs files --level error
crosslane status --since 30m
crosslane stats --detail --since 1h --by endpoint
```

Response headers `x-trace-id` and `request-id` identify a request across dates and
levels. Files are split by exact level and UTC date, with a default 100 MB shard
size and three-day retention; `info.log` excludes WARN/ERROR. Trace queries have
scan/output limits. The Docker wrapper adds host paths for logs within the data
volume. Use `RUST_LOG` or `log.level` for more detail; application logs omit prompt,
response and key values.
Business HTTP logs include a `user_agent` field directly on request and stream
failure events, including WARN/ERROR records. `logs show/follow` also print it
for each request and include it in JSON output. Missing values are `unknown`,
and values are capped at 256 characters. This client-supplied field helps
identify a version but does not authenticate the client.

`status` covers the current daemon run; `stats` defaults to persisted totals.
Both accept `--since` or timezone-qualified `--start/--end`. Minute aggregates are
stored in UTC hourly shards under `stats-history/` and retained indefinitely, so
monitor disk growth. History already pruned by older releases cannot be recovered.
Use logs for individual failures; `stats --detail` is not a complete request audit.

## 8. Docker Compose

The Crosslane image has not been published yet; build the current source on a
Linux amd64 host. Setup waits for health and installs the matching host wrapper.
After the first image release, `--image` can select a published tag or digest:

```bash
./deploy/docker-setup.sh --build
crosslane version
crosslane ready
```

The default wrapper destination is `/usr/local/bin/crosslane`; use
`--target "$HOME/.local/bin/crosslane"` for a user-owned path. Later,
`./deploy/docker-upgrade.sh` follows `latest`; retain `--image` for version pinning.
`./deploy/install-crosslane-wrapper.sh` installs only the wrapper and refuses to replace unrelated commands by default.

Compose uses host networking, supported directly on Linux Engine; Docker Desktop
4.34+ needs it enabled in settings. Services bind host ports directly and default
to `0.0.0.0`; choose `127.0.0.1` for local-only access. The `crosslane-data` volume mounts
at `/var/lib/crosslane`. Do not mount development `.crosslane-dev` or `.env.example` into it.
Existing bridge deployments need container recreation to adopt the new network mode;
the volume is retained. Inspect deployment or deliberately build source with:

```bash
docker compose config --quiet
docker compose ps
docker compose logs -f crosslaned
# Source build only:
docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build
```

Full pairs Chromium `r1566079` with `chromiumoxide 0.9.1` and sets the container
no-sandbox option. Changing the build override target to `runtime-slim` removes
browser SSO; CI currently does not publish this target. Local builds default to
`CARGO_BUILD_JOBS=1`. The image includes vim; `EDITOR` may select another installed editor.

The wrapper requires a working Docker engine and an identifiable deployment. Select
among multiple deployments with `CROSSLANE_COMPOSE_PROJECT` or `CROSSLANE_DOCKER_CONTAINER`.
Stopped-container navigation uses the exact local image without network or business
mounts. Missing images, ambiguous targets and old images without local navigation
fail explicitly. Business commands need a running daemon. The wrapper preserves
exit codes/stdin, allocates a TTY for interactive use, and passes or falls back from `TERM`.

`crosslane restart` waits for health; `crosslane stop` retains the deployment.
`crosslane uninstall` stops service, backs up state, then deletes the container, volume,
unshared image and wrapper. Backups default to `~/.crosslane/backups`. Failed backup
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

sudo useradd --system --user-group --home-dir /var/lib/crosslane --shell /usr/sbin/nologin crosslane
sudo install -m 0755 target/release/crosslaned target/release/crosslane /usr/local/bin/
sudo install -m 0644 deploy/crosslaned.service /etc/systemd/system/crosslaned.service
sudo systemctl daemon-reload
sudo systemctl enable --now crosslaned
sudo systemctl status crosslaned
```

If the `crosslane` user already exists, skip `useradd`. The unit uses `/etc/crosslane`,
`/var/lib/crosslane`, and `/run/crosslane` through systemd-managed directories.

```bash
sudo -u crosslane crosslane --socket /run/crosslane/admin.sock status
sudo journalctl -u crosslaned -f
sudo systemctl reload crosslaned
```

Reload sends `SIGHUP`. Restart-required settings still need
`sudo systemctl restart crosslaned`.

Install Chrome or Chromium on the host before using `crosslane account add-sso`.
The provided unit supports the default full build: it intentionally leaves user
namespaces and executable JIT memory available for Chromium, while retaining
`NoNewPrivileges`, filesystem protection, an empty capability set, and the other
hardening controls. If the host kernel disables unprivileged user namespaces,
prefer enabling them. As a last-resort compatibility override, set
`CROSSLANE_CHROMIUM_NO_SANDBOX=1` with `systemctl edit crosslaned`; this disables
Chromium's own sandbox and should only be used after reviewing the host's
isolation boundary.

## Navigation and scripting

`crosslane`, `crosslane help`, and bare groups such as `crosslane logs` display help.
Use `crosslane help logs trace` for nested options, `crosslane help --all` for the public
command tree, and `crosslane guide` for operational topics. These commands and
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

Provider scope is shared across the command tree: `crosslane --provider copilot
account list`, `crosslane account --provider copilot list` and the leaf form are
equivalent. `-p` is the short form. `CROSSLANE_PROVIDER` sets the shell default;
native business commands also read it from `.env`, including `account add` and
`account login-code`. Explicit flags override the process environment, which
overrides `.env`. The value is the configured instance ID shown by
`provider list`, not the driver kind or the display name “GitHub Copilot”.
For example, an instance created with `--id work-cp --kind copilot` uses
`--provider work-cp`. `provider show work-cp` prints commands using that ID;
`guide copilot -p work-cp` also produces matching setup examples.
The Docker wrapper forwards this shell default and recognizes provider options
before or after command groups when streaming a host CSV. Update the installed
wrapper together with the CLI to use these forms.

Nested help and `help --all` identify shared, Kiro-only, Copilot-only, global and
resource-selected commands. Global operations and commands that locate a
resource by positional ID/name ignore `CROSSLANE_PROVIDER` and reject an explicit
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
| `crosslane logs --tail 100` | `crosslane logs show --tail 100` |
| `crosslane logs -f` / `--follow` | `crosslane logs follow` |
| `crosslane models --refresh --mapped` | `crosslane models list --refresh --mapped` |
| `crosslane tasks` (data query) | `crosslane tasks list` |
| `crosslane diagnose` (full diagnosis) | `crosslane diagnose all` |
| `crosslane help balance` | `crosslane guide balance` |
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
backup_dir="$HOME/crosslane-backups/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$backup_dir"
docker compose stop crosslaned
docker compose cp crosslaned:/var/lib/crosslane/. "$backup_dir/"
docker compose start crosslaned
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
| Cannot connect to `admin.sock` | Check daemon, user, absolute `CROSSLANE_HOME`, `config path` and `--socket`. |
| Configuration unchanged | Run `config validate` and `config show --effective`; check environment overrides, retained defaults and restart-only settings. |
| 401 / client rejected | Check the service's bound key, product User-Agent and exemptions. |
| 503 | Inspect `ready`, credits/protection thresholds, concurrency, background tasks and `logs trace`. |
| Stream interrupted | Inspect the protocol's final event and Trace ID; HTTP 200 does not establish success. |
| Context overflow | Use `models resolve` and `error.context`; see [compaction boundaries](protocol-compatibility.md#automatic-compaction-and-model-windows). |
| Old container version | Compare actual image ID and `crosslane version`; pull the intended release or deliberately build source. |
