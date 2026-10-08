# crosslane

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust 1.97.1](https://img.shields.io/badge/Rust-1.97.1-orange?logo=rust)](rust-toolchain.toml)
[![Docker Ready](https://img.shields.io/badge/Docker-Ready-blue?logo=docker)](docs/startup-and-debugging.md#docker)
[![Auto Release Pipeline](https://github.com/Yaocool/crosslane/actions/workflows/docker-image.yml/badge.svg)](https://github.com/Yaocool/crosslane/actions/workflows/docker-image.yml)
[![Image: GHCR](https://img.shields.io/badge/Image-GHCR-blue?logo=github)](docker-compose.yml)

[English](README.md) | [简体中文](README.zh-CN.md)

Crosslane is a self-hosted API proxy for Kiro and GitHub Copilot accounts. It
offers Claude Messages, OpenAI Chat Completions, OpenAI Responses, and model
listing endpoints. Use `crosslaned` to run the service and `crosslane` to manage
accounts, services, and logs. Upstream credentials and client API keys are
separate.

## ⚠️ Important notice

**Service terms:** Accessing Kiro or GitHub Copilot through a proxy may not be
permitted under the terms that apply to a particular account. Review the
[Kiro terms notice](https://kiro.dev/downloads/),
[AWS Service Terms](https://aws.amazon.com/service-terms/),
[GitHub Copilot terms](https://docs.github.com/en/site-policy/github-terms/github-terms-for-additional-products-and-features#github-copilot),
and your organization's policies before using Crosslane. Noncompliant use may
lead to account restrictions or service interruption.

**Disclaimer:** Crosslane is provided “as is” under the [MIT License](LICENSE),
without a warranty of upstream availability or continued account access. Users
are responsible for their credentials and use of upstream services. To the
extent permitted by applicable law, the maintainers are not liable for account
restrictions, service interruption, or related losses arising from use of this
project.

## Quick start

Run these commands from a checkout of this repository. Docker requires Docker
Engine and Compose on a Linux host:

```bash
./deploy/docker-setup.sh --build
crosslane health
```

The setup script installs a host `crosslane` command and stores service data in
the `crosslane-data` Docker volume. To use an available registry image, run the
script without `--build` or select a tag with `--image`. See the
[operations guide](docs/startup-and-debugging.md#docker) for upgrades and backup.

For a native build on Linux or macOS, install Rust and a C toolchain:

```bash
cp .env.example .env
cargo build --release --locked
./target/release/crosslaned
```

In another terminal at the repository root:

```bash
export PATH="$PWD/target/release:$PATH"
crosslane health
```

The example environment keeps local data in `.crosslane-dev`. Use an absolute
`CROSSLANE_HOME` when running commands from different directories.

## Create a service

Add a Kiro account using one of the supported login methods. For an imported
headless API key, provide `KIRO_API_KEY` in the CLI environment:

```bash
crosslane account add-api-key --email ci@example.com --region us-east-1
```

For enterprise SSO or GitHub Copilot accounts, follow
[account setup](docs/startup-and-debugging.md#accounts). Then create a listener
and keep the client key printed by the command:

```bash
crosslane service create --name main --host 127.0.0.1
crosslane account list
crosslane models list
crosslane ready
```

`health` checks that the daemon responds; `ready` checks whether a configured
service can accept work. A generation request is needed to verify upstream
access. Never use an upstream credential as the client key.

## Connect a client

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/messages` | Claude Messages |
| `POST /v1/messages/count_tokens` | Token counting |
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/responses` | OpenAI Responses |
| `GET /v1/models` | Available models |

For Claude Code, point `ANTHROPIC_BASE_URL` at your service URL, for example
`http://127.0.0.1:5580`, and set `ANTHROPIC_AUTH_TOKEN` to the client key.
Choose a model shown by `crosslane models list`. Other clients may need a
User-Agent policy exemption; see [client access](docs/startup-and-debugging.md#services-and-client-access).
Use TLS and restrict network access when exposing a service beyond the local host.

## Daily commands

```bash
crosslane status
crosslane service list
crosslane models list
crosslane stats --since 1h
crosslane logs show --tail 100
crosslane logs trace <TRACE_ID>
crosslane help --all
```

For deployment, account management, model controls, logs, and recovery, see the
[operations guide](docs/startup-and-debugging.md). For building and validating
changes, see [CONTRIBUTING.md](CONTRIBUTING.md). Crosslane is licensed under
[MIT](LICENSE).
