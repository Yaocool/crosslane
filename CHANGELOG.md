# Changelog

This file records user-visible changes. The current source still declares
`1.2.0`, and this history includes changes after the `v0.2.4` tag. Items below are unreleased;
they must receive a new version rather than replacing that tag or its image.

## Unreleased — since v0.2.4

### Breaking changes and migration

- Renamed `kiro-proxy` to `crosslane` across workspace crates, binaries
  (`crosslane` / `crosslaned`), file paths, CLI help/completion, `CROSSLANE_*`
  environment variables, state/socket/log defaults, HTTP diagnostic headers,
  Docker images/labels/volumes, systemd and deployment scripts. Provider IDs and
  account/API-key data formats are unchanged. New Web Search replay records use
  the Crosslane identity; authenticated pre-rename records remain readable with
  the original replay key. Existing state must be migrated explicitly; see the
  [English guide](docs/startup-and-debugging.md#crosslane-rename-migration) or
  [中文说明](docs/startup-and-debugging.zh-CN.md#crosslane-改名迁移).
- `service apikey` / `service apikeys`, `account export`, and `config show` now
  redact credentials by default in text and JSON. Use `--show-secret`,
  `--include-secrets`, or `--show-secrets`, respectively, for complete output.
  Existing `--redact` options remain accepted. Redacted `config show` output
  omits source comments.
- Explicit `--provider` on global or resource-ID-selected commands now fails
  instead of being ignored. Shell provider defaults do not affect those commands;
  global model policy retains its explicit `--provider all` override requirement.

- `pool` and `account refresh/probe/reset-health --all` now default to all
  providers. Use `--provider kiro` to keep the old scope. Account export keeps its
  legacy Kiro default; use `--provider all` for a unified export.
- CLI groups `logs`, `models`, `tasks`, and `diagnose` now show help when used
  without an action. Use `logs show`, `models list`, `tasks list`, and `diagnose all`
  for data. Bare groups with `--json` fail with exit code 2.
- Removed parent-level logs/models options, hidden `account add-sso-batch`, alert
  `--kind`/`--url` and platform `wechat`, and topic-style `help balance`.
  See the [English migration](docs/startup-and-debugging.md#migration-from-the-old-cli) or
  [中文迁移表](docs/startup-and-debugging.zh-CN.md#旧命令迁移).
- Protocol-specific User-Agent checks are enabled by default: Claude Code for
  Claude routes, Codex for OpenAI generation routes, both for models. Other clients
  need a service/key exemption or a global configuration change; authentication
  and service key allowlists continue to apply.

### Added

- `service create` without `--port` uses `server.port` for the first service,
  then one above the highest effective service port, including disabled
  services and any `CROSSLANE_HTTP_PORT` override. Service views report the
  effective port, and creation reports when HTTP listening is disabled.
  Explicit `--port` remains available.
- `config reload` exits nonzero when a configuration was not applied; task
  help distinguishes scheduler-only tasks, and `stats --recent` accepts 0–1000.
- `config show --effective` reads the last applied configuration even if the
  on-disk TOML is currently invalid; its JSON output omits raw TOML.
  `config edit --json` returns a result object. `status/pool --watch --json`
  emit one compact JSON snapshot per line. Admin RPC calls now time out.

- Provider ID/type columns, actual-instance next-step hints, per-command scope
  labels, task scope summaries, `-p` at every command depth, scoped provider lists,
  and custom-ID setup guides. Missing-provider errors list configured IDs. Service
  scope edits retain a valid default or select the first newly allowed provider.
- Docker wrapper forwarding for `CROSSLANE_PROVIDER` and host CSV batches with a
  provider option at any command depth. Update the installed wrapper with the CLI.

- Persistent global model disabling with `model disable/enable` (`models` remains
  supported). CLI lists retain disabled entries; public catalogs hide them.
  Shared transport barriers reject disabled models across protocols, mappings,
  defaults, fallback, retries, stream continuations and internal probes, including
  clients created before a hot reload. Configuration resets preserve deny rules.

- Global `--provider` at any command depth and `CROSSLANE_PROVIDER` shell defaults;
  a shared `account add` entry point for Kiro SSO/API keys and Copilot Device Flow.
  Existing provider-specific authentication commands remain available.
- Chat Completions, Responses and Messages JSON/SSE adaptation for every
  discovered non-embedding Copilot generation model, with native endpoint
  preference, function-tool history, multimodal input, usage and billing retention.
  Local token-count estimates are explicitly marked when Messages is not native.
- Unified service account listing, binding, tag pools and exclusions for Kiro
  and Copilot; qualified account references persist without changing legacy IDs.
- Per-service account pools selected by account tag, with global-pool fallback,
  explicit account additions and exclusions, binding-aware account mutation
  guards, stopped-service-only tag changes, reverse account-to-service lookup,
  and tag assignment during single or batch account creation.
- One-command tag updates across multiple existing accounts, validating every
  target before applying the batch.
- Multi-tag proxy-service pools with union matching via
  `service create --account-tag xx1 xx2` and stopped-service edits.
- OpenAI Responses JSON/SSE endpoints and aliases, bounded service/key-scoped
  `previous_response_id` state, function/custom/namespace tools, Responses Lite
  `additional_tools`, tool choice, reasoning summaries and Codex automation input
  normalization. State defaults to enabled and disappears on restart.
- Explicit Kiro headless API-key import, regional runtime/management routing,
  Claude gateway model discovery aliases, effort metadata and client `envState`.
- Module-scoped configuration management, serialized cross-process configuration
  transactions, API-key restoration with a supplied secret, and per-service/key
  User-Agent exemptions.
- Nested CLI help, complete command-tree discovery, guides, shell completion,
  and Docker help/navigation from stopped containers using the matching image.

### Fixed

- Preserve existing resource permissions when editing under `CROSSLANE_PROVIDER`;
  require explicit scope changes and reject contradictory provider flags.
- Report partial provider-pool discovery as a failure with retained error details;
  exclude globally disabled models from operational account/pool availability.
- Retain structured-output schemas, refusal content, and incomplete tool-call
  finish reasons across Copilot protocol conversion, including streaming output.
- Resolve Copilot accounts when tagging by bare ID, including the legacy tag RPC;
  validate all batch selectors and duplicate aliases before changing tags.
- Apply Copilot service account membership to scheduling and health/readiness,
  observe live membership edits, and retain protocol errors in request statistics.
- Update upstream client identities to Kiro IDE `1.0.437` and CLI `2.21.4`
  ahead of the 2026-11-09 retirement of older clients. Share the version baseline
  across generation, metadata and Social token refresh, add missing CLI Management
  headers, and log the active baseline at startup. Rebuild and restart the daemon
  to apply the update.
- Align Claude counting, optional tool results and nullable citations, and OpenAI
  nullable stream/strict controls, disabled reasoning and refusal replay with the
  public protocols. Normalize missing tool descriptions for Kiro tool replay.
- Execute Chat stop sequences and multiple choices in JSON/SSE, map
  `max_completion_tokens`, and adapt legacy function requests and responses.
  Wait for each streamed candidate's settlement before the next admission,
  retain body memory reservations, and record failures after SSE starts.
- Translate OpenAI inline/URL files and Claude search-result content to Kiro,
  accept empty web-search domain filters, and raise the image count limit to 100.
- Support Responses `truncation: auto` against the account's resolved model
  window, including smaller fallback models and continuations, while preserving
  instructions and the current tool chain.
- Validate cache controls against Claude's `ephemeral` and TTL schema across
  Messages, counting aliases, and OpenAI Chat cache extensions; accept absent/null
  controls and reject malformed or unknown types. Check Claude TTL ordering,
  automatic/explicit breakpoint conflicts and limits, uncacheable blocks, and
  nested search-result controls without forwarding client cache fields to Kiro.
- Preserve tool-only/empty assistant turns and validate upstream tool JSON across
  buffered/unbuffered, JSON/SSE paths; recover hollow Responses tool continuations
  once and surface repeated failure.
- Accept additive output-format and reasoning hints without claiming structured
  output enforcement; omit unsupported thinking controls when model metadata is
  missing, and preserve reasoning/tool markup across stream fragments.
- Resolve the account's actual model before compaction, summarize complete source
  input in bounded chunks, preserve checkpoints across client turns, and handle
  oversized Claude Code `/compact` source/system/tool material.
- Correct summary timeout, failure and usage accounting; diagnose fixed-context
  overflow without logging prompt or schema contents.
- Keep alerts when resetting general configuration; improve wrapper deployment
  preflight/atomic replacement and health-check rollback handling.
- Harden model identifiers and normalized upstream error redaction; tolerate
  transient browser navigation-context errors during SSO.

### Documentation

- Reorganize bilingual quick starts, startup/configuration references, CLI
  migration and protocol limits; replace historical design prose with current
  compaction behavior and explicitly labelled remaining work.
- Add contributor/release guidance and a dated 1.0.0 readiness assessment
  (the historical assessment is no longer part of the maintained documentation).
- Consolidate operations, CLI migration, compatibility and release guidance into six maintained references; remove duplicate indexes and design documents.

## Earlier releases

Historical source changes remain available through Git tags and commit history.
`v0.2.4` points to `b06ec16` (2026-08-31). This changelog does not reconstruct
unverified release notes or assert registry availability for older images.

```bash
git log --oneline v0.2.4..HEAD
git tag --list --sort=-v:refname
```
