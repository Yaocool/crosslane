# crosslane

[English](README.md) | [简体中文](README.zh-CN.md) | [Documentation](#documentation)

`crosslane` exposes Claude Messages, OpenAI Chat Completions, and OpenAI Responses
compatible APIs through a multi-provider architecture with built-in Kiro and GitHub
Copilot adapters. The Rust daemon `crosslaned` handles routing, generation, and account
scheduling; `crosslane` manages all providers, or one provider, through a local Unix socket.

Kiro supports enterprise SSO credentials (AWS IAM Identity Center/IdC) and imported
headless API keys (`ksk_...`). Copilot supports GitHub Device Flow or token import from
standard input. Upstream credentials and the proxy's client API keys are separate.
The project has no GUI, MITM, or local client configuration rewriting.

> The workspace version is `1.2.0`. The Crosslane rename is unreleased; build from
> source until an image containing it has been published. Existing installations
> must follow the [rename migration](docs/startup-and-debugging.md#crosslane-rename-migration)
> before switching binaries or Compose projects. See the [changelog](CHANGELOG.md).

## Capabilities and limits

| Area | Current behavior |
| --- | --- |
| APIs | Messages, token counting, Chat Completions, Responses, model discovery; JSON and SSE generation. |
| Accounts | Provider-isolated pools, per-account concurrency, enable/disable, tags, probes and refresh; Kiro quota scheduling and Copilot Device Flow. |
| Upstream routing | Regional Kiro runtimes and GovCloud isolation; Copilot dynamic endpoints, native forwarding and Chat/Responses/Messages adaptation for non-embedding models. |
| Models and tools | Cross-provider discovery, conditional mapping and access scopes; Kiro tool replay, Claude Tool Search and Web Search. |
| Operations | Hot-reloaded TOML, API-key quotas, TLS, webhooks, trace logs, persisted statistics, Docker and systemd. |
| Compatibility limits | Format/strict hints do not guarantee structured output. Responses state expires and is lost on restart. Hosted tools and automatic compaction differ by protocol. |

The default client policy accepts Claude Code on Claude routes and Codex on
OpenAI generation routes; model discovery accepts both. Other clients can use a
service or API-key exemption. Authentication and service key allowlists still
apply. Read the [protocol reference](docs/protocol-compatibility.md) and
[Responses support matrix](docs/openai-responses.md) before integrating a client.

## Quick start

Run commands from a checkout of this repository. Native operation uses Unix
sockets; Linux and macOS are the intended native environments. The published
Docker workflow targets **Linux amd64** with the full SSO image.

### Docker on a Linux server

With Docker Engine and a working `docker compose` command:

```bash
./deploy/docker-setup.sh --build
crosslane health
```

The Crosslane image has not been published yet, so build the current checkout.
The setup script checks daemon health and installs the matching host CLI wrapper;
pull deployments also attempt image rollback on failure. State lives in the
`crosslane-data` volume. The default wrapper
path is `/usr/local/bin/crosslane`; use `--target "$HOME/.local/bin/crosslane"` for a
user-owned location and include that directory in `PATH`.

After a Crosslane image is published, setup without `--build` uses `latest` for
fresh installs and reuses the saved image reference later. Use
`./deploy/docker-upgrade.sh` to follow `latest`, or `--image` to choose a
published tag/digest. Private GHCR packages require login.

Compose uses host networking. Linux supports it directly; Docker Desktop needs
host networking enabled. A newly created service defaults to `0.0.0.0:5580`;
restrict its port or choose loopback as shown below. Deployment health alone does
not prove that generation works. See the [deployment guide](docs/startup-and-debugging.md#8-docker-compose)
for platform requirements, upgrades, backup, rollback, and uninstall behavior.

### Native build

Install rustup and a C toolchain/linker. `rust-toolchain.toml` selects Rust 1.97.1
(edition 2021). The default build includes browser SSO; native SSO login also
requires an installed Chrome/Chromium.

```bash
cp .env.example .env             # First setup only; keep an existing .env
cargo build --release --locked
./target/release/crosslaned
```

In another terminal at the repository root:

```bash
export PATH="$PWD/target/release:$PATH"
crosslane health
crosslane config path
```

The example `.env` stores development state under `.crosslane-dev`. The daemon and
CLI business commands load the nearest `.env` upward from the working directory;
existing environment variables win. CLI help, guides, completion, and version
work without `.env` or a daemon. Use an absolute `CROSSLANE_HOME` when invoking the
programs from different directories. See [environment and paths](docs/startup-and-debugging.md#2-environment-loading).

### Add credentials and create a service

A fresh daemon has no business listener. Choose one credential import method:

```bash
crosslane account import --stdin < /secure/accounts.json
# Alternative: reads KIRO_API_KEY from the CLI environment
crosslane account add-api-key --email ci@example.com --region us-east-1
```

Then create a service and save its printed client key:

```bash
crosslane service create --name main --host 127.0.0.1
crosslane account list
crosslane models list
crosslane ready
```

Do not reuse a `ksk_...` upstream key as the proxy client key. Import schemas,
SSO login and credential handling are covered in [account setup](docs/startup-and-debugging.md#4-add-or-import-accounts).
`health` checks daemon liveness; `ready` checks business prerequisites. A real
generation request additionally verifies the upstream route and consumes quota.

### Global model disabling

Disable models globally across Kiro/Copilot, every protocol, default routing,
fallback, retries, and internal probes:

```bash
crosslane model disable claude-sonnet-4.6 gpt-4.1
crosslane model list                         # retained, marked unavailable/disabled
crosslane model enable claude-sonnet-4.6      # takes effect without restarting
```

Rules persist in `[models].disabled` and survive configuration resets. Public
`/models` and `/v1/models` hide disabled entries. `model` is an alias for `models`;
use unqualified model IDs because the policy applies to every provider. Once
the update completes, queued and subsequent sends are blocked; requests already
sent cannot be recalled.

### Kiro overage credits

Inspect or change the policy for the entire Kiro account pool with the account
command:

```bash
# Enable overage with a 500-credit maximum for each account
crosslane account overage enable --max-credits 500

# Show the policy and per-account proxy/Kiro limits
crosslane account overage

# Refresh Kiro usage before displaying it
crosslane account overage show --refresh

# Disable overage while retaining the configured maximum
crosslane account overage disable
```

`enable` and `disable` update the configuration atomically and reload it.
`enable` also refreshes Kiro usage by default. Use
`crosslane account overage enable --kiro-limit` to rely entirely on Kiro's
reported cap. You can also edit the equivalent settings in the `config.toml`
shown by `crosslane config path`:

```toml
[pool]
enable_overage = true
max_overage_credits_per_account = 500.0
```

It is off by default. With it off, plan and bonus credits remain subject to
low credit protection and exhaustion checks. With it on, the proxy includes
Kiro's reported overage cap. If `max_overage_credits_per_account` is omitted,
the proxy retains the original switch behavior and does not pause accounts
based on its local balance. When set, the maximum applies separately to every
account: new requests stop once usage reaches the non-overage limit plus the
smaller of Kiro's overage cap and this setting. The low credit threshold does
not apply. For example, a 10,000 credit plan with a 10,000 credit Kiro overage
cap and a 500 credit proxy maximum has an effective limit of 10,500.

The proxy uses the latest known usage when admitting requests, so requests
already in flight can exceed this maximum. Upstream quota rejections and the
service daily credit limit still apply. After manually editing TOML, run
`crosslane config reload`, then `crosslane tasks run status_check` to refresh Kiro's
overage values. Inspect `crosslane config show pool --effective` and
`crosslane pool --model claude-sonnet-4.6 --explain`. In `crosslane account list`,
the quota column shows current usage, the proxy's configured total, and Kiro's
server total, for example `10000.01/10500.00/20000.00`. The
`Overage(configured/Kiro)` column shows the per-account proxy maximum and
Kiro's reported cap together.

## Connect a client

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/messages` | Claude Messages; aliases `/messages` and `/anthropic/v1/messages`. |
| `POST /v1/messages/count_tokens` | Local token estimates; the Claude aliases also support `/count_tokens`. |
| `POST /v1/chat/completions` | OpenAI Chat Completions; alias `/chat/completions`. |
| `POST /v1/responses` | OpenAI Responses; alias `/responses`. |
| `GET /v1/models` | Model discovery for both clients; alias `/models`. |
| `GET /health`, `GET /ready` | Liveness and readiness on an existing service listener. |

For Claude Code, set `ANTHROPIC_BASE_URL` to `http://127.0.0.1:5580` and
`ANTHROPIC_AUTH_TOKEN` to the service's client key. Large MCP catalogs can use
`ENABLE_TOOL_SEARCH=auto`; gateway model discovery uses
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`.

For Codex, use `http://127.0.0.1:5580/v1` and `wire_api = "responses"`. The
[Codex setup guide](docs/openai-responses.md) includes the provider configuration,
state limits, unsupported controls, and stream errors. Select an available model
from `crosslane models list`; aliases do not enlarge an actual model's input window.

## Daily operations

```bash
crosslane status
crosslane stats --since 1h
crosslane logs show --tail 100
crosslane logs trace <TRACE_ID>
crosslane config show --effective
crosslane service list
crosslane help --all
crosslane help logs trace
crosslane guide balance
```

Bare command groups show help. Scripts must use explicit actions such as
`logs show`, `models list`, `tasks list`, and `diagnose all`; the last command
performs real inference on all accounts. Read the [CLI migration](docs/startup-and-debugging.md#migration-from-the-old-cli)
before updating scripts. Use [startup and troubleshooting](docs/startup-and-debugging.md)
for configuration, log retention, Docker lifecycle commands and systemd.

## Documentation

| Topic | Reference |
| --- | --- |
| Deployment, CLI migration, logs and recovery | [English](docs/startup-and-debugging.md) · [中文](docs/startup-and-debugging.zh-CN.md) |
| Protocol limits, model controls and compaction | [English](docs/protocol-compatibility.md) · [中文](docs/protocol-compatibility.zh-CN.md) |
| Responses, Codex and stateful continuation | [Integration guide (中文)](docs/openai-responses.md) |
| Multi-provider routing and GitHub Copilot | [Integration guide (中文)](docs/providers-and-copilot.zh-CN.md) |

## Development

The workspace crates separate domain/configuration, storage, IPC, translation,
provider adapters, upstream access, scheduling, notifications, the daemon and the CLI.
See [contributor guidance](CONTRIBUTING.md) and [architecture notes](CLAUDE.md).

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

These are required checks, not a claim that the current checkout passes them.
See [validation and releases](CONTRIBUTING.md#validation) for the full checks.
Pushing a matching `v*` tag triggers the Docker image publishing workflow.
The project is licensed under [MIT](LICENSE).
