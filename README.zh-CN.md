# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md) | [文档](#文档)

`kiro-proxy` 通过多 provider 架构提供 Claude Messages、OpenAI Chat Completions 和
OpenAI Responses 兼容 API，内置 Kiro 与 GitHub Copilot adapter。Rust 常驻进程 `kproxyd`
负责路由、生成和账号调度，`kproxy` 通过本地 Unix socket 统一管理，也可按 provider 管理。

Kiro 支持企业 SSO 凭证（AWS IAM Identity Center/IdC）和显式导入的 headless API key
（`ksk_...`）；Copilot 支持 GitHub Device Flow 或从标准输入导入 token。上游凭证与代理向客户端
签发的 API Key 相互独立。项目不包含 GUI、MITM 或本机客户端配置改写。

> 文档对应当前源码，workspace 版本仍为 `0.2.4`，包含 **`v0.2.4` tag 之后尚未发布的改动**，
> 其中包括 CLI 命令迁移。预构建的 `v0.2.4` 镜像不包含这些改动。
> 版本差异见[变更记录](CHANGELOG.md)，发布判断见 [1.0.0 评估](docs/release-readiness-1.0.0.zh-CN.md)。

## 能力与边界

| 范围 | 当前行为 |
| --- | --- |
| API | Messages、Token 计数、Chat Completions、Responses、模型发现；生成支持 JSON 和 SSE。 |
| 账号 | Provider 隔离账号池、单账号并发、启停、标签、探测和刷新；Kiro 保留额度调度，Copilot 支持 Device Flow。 |
| 上游路由 | Kiro 区域 runtime 与 GovCloud 隔离；Copilot 动态 endpoint、原生透传和非 embedding 模型的 Chat/Responses/Messages 转换。 |
| 模型与工具 | 跨 provider 动态发现、条件映射和访问范围；Kiro 继续支持工具回放、Claude Tool Search 与 Web Search。 |
| 运维 | TOML 热重载、API Key 限额、TLS、Webhook、Trace 日志、持久化统计、Docker 和 systemd。 |
| 兼容限制 | format/strict 提示不提供结构化输出保证；Responses 状态会过期且重启丢失；托管工具与自动压缩的支持范围因协议而异。 |

默认 Claude 路由接受 Claude Code，OpenAI 生成路由接受 Codex，模型发现接受两者。
其他客户端可通过服务或 API Key 级别的豁免接入，认证和服务 Key 白名单继续生效。
接入前请阅读[协议兼容说明](docs/protocol-compatibility.zh-CN.md)和
[Responses 支持矩阵](docs/openai-responses.md)。

## 快速开始

以下命令在项目检出目录中运行。原生程序使用 Unix socket，面向 Linux 和 macOS；
当前 Docker 发布工作流只构建 **Linux amd64** 的 full SSO 镜像。

### Linux 服务器使用 Docker

准备 Docker Engine 和可用的 `docker compose` 命令：

```bash
./deploy/docker-setup.sh
kproxy health
```

脚本先拉取镜像，再替换容器并检查 daemon 健康；部署失败时尝试回退镜像，健康后安装匹配的
宿主机 CLI 包装器。数据保存在 `kproxy-data` 卷中。包装器默认安装到 `/usr/local/bin/kproxy`；
使用 `--target "$HOME/.local/bin/kproxy"` 可安装到用户目录，记得将该目录加入 `PATH`。

首次部署使用 `latest`，后续 setup 复用已保存的镜像引用。跟随 `latest` 使用
`./deploy/docker-upgrade.sh`，固定版本使用 `--image` 指定已发布的 tag 或 digest。
需要当前检出源码中的未发布能力时，使用 `./deploy/docker-setup.sh --build`。
私有 GHCR 包需要先登录。

Compose 使用 host network。Linux 可直接使用；Docker Desktop 需要启用 host networking。
新服务默认监听 `0.0.0.0:5580`，应限制端口访问，或按下方示例监听回环地址。
部署健康不等于上游生成可用。平台要求、升级、备份、回退和卸载见
[部署指南](docs/startup-and-debugging.zh-CN.md#8-docker-compose)。

### 本地编译

安装 rustup、C 工具链和链接器。`rust-toolchain.toml` 自动选择 Rust 1.97.1，使用 edition 2021。
默认构建包含浏览器 SSO；原生 SSO 登录还需要安装 Chrome/Chromium。

```bash
cp .env.example .env             # 仅首次配置；保留已有 .env
cargo build --release --locked
./target/release/kproxyd
```

另开一个终端，在仓库根目录运行：

```bash
export PATH="$PWD/target/release:$PATH"
kproxy health
kproxy config path
```

示例 `.env` 将开发数据放在 `.kproxy-dev`。daemon 和 CLI 业务命令从工作目录向上查找最近的
`.env`，已有进程环境变量优先；CLI 帮助、指南、补全和版本查询不依赖 `.env` 或 daemon。
从不同目录执行时应使用绝对路径的 `KPROXY_HOME`，详见[环境与路径](docs/startup-and-debugging.zh-CN.md#2-环境变量加载)。

### 导入凭证并创建服务

全新 daemon 不创建业务监听。选择一种凭证导入方式：

```bash
kproxy account import --stdin < /secure/accounts.json
# 另一种方式：从 CLI 环境变量 KIRO_API_KEY 读取
kproxy account add-api-key --email ci@example.com --region us-east-1
```

再创建服务并保存命令输出的客户端 Key：

```bash
kproxy service create --name main --host 127.0.0.1 --port 5580
kproxy account list
kproxy models list
kproxy ready
```

不要将 `ksk_...` 上游密钥作为代理客户端 Key。导入格式、SSO 登录和凭证处理见
[账号配置](docs/startup-and-debugging.zh-CN.md#4-添加或导入账号)。`health` 检查 daemon 存活，
`ready` 检查业务前置条件；真实生成还需验证上游路由，并会消耗额度。

### 全局模型禁用

模型可全局禁用，立即作用于 Kiro/Copilot、所有协议、默认模型、回退、重试和内部探测：

```bash
kproxy model disable claude-sonnet-4.6 gpt-4.1
kproxy model list                         # 保留条目，标记“不可用（已禁用）”
kproxy model enable claude-sonnet-4.6      # 解除禁用，无需重启
```

规则持久化在 `[models].disabled`，公共 `/models`、`/v1/models` 均隐藏禁用项。
`model` 与 `models` 等价；禁用规则跨全部 provider，使用不带 provider 前缀的模型 ID。
配置重置会保留规则。禁用确认生效后，排队请求和后续发送均被拦截；此前已发送的请求无法撤回。

### Kiro 超额额度

用账号命令查看或修改整个 Kiro 账号池的 overage 策略：

```bash
# 开启，并限制每个账号最多使用 500 credits overage
kproxy account overage enable --max-credits 500

# 查看策略及各账号的代理/Kiro 额度
kproxy account overage

# 强制刷新 Kiro 额度后查看
kproxy account overage show --refresh

# 关闭；保留 500 的配置供下次启用
kproxy account overage disable
```

`enable` 和 `disable` 会原子更新配置并自动重载；`enable` 默认还会立即刷新 Kiro 额度。若要完全采用 Kiro 返回的上限，使用 `kproxy account overage enable --kiro-limit`。也可以在 `kproxy config path` 显示的 `config.toml` 中手工设置等价配置：

```toml
[pool]
enable_overage = true
max_overage_credits_per_account = 500.0
```

开关默认关闭。关闭时，账号按套餐及赠送额度接受低额度保护和耗尽检查。开启后，代理计入 Kiro 返回的超额上限；不设置 `max_overage_credits_per_account` 时，沿用原有开关行为，不按本地余额暂停账号。设置后，该值对池中**每个账号分别**生效，代理在账号已用额度达到“非超额额度 + min(Kiro 超额上限, 配置上限)”时停止为它分配新请求，不提前应用低额度保护。例如 Kiro 返回 10000 套餐额度及 10000 超额上限，配置 500 时该账号的代理可用总额度为 10500。

此配置按 Kiro 最近一次返回的用量拦截新请求；已在处理中的请求可能让最终用量超过配置值。Kiro 上游实际拒绝额度请求时仍按上游错误处理；服务每日额度上限仍然生效。手工编辑 TOML 后运行 `kproxy config reload`，再运行 `kproxy tasks run status_check` 刷新 Kiro 返回的 overage 数值。可用 `kproxy config show pool --effective` 和 `kproxy pool --model claude-sonnet-4.6 --explain` 核对调度；`kproxy account list` 的“额度”列按“当前已用/代理配置后总额/Kiro 服务端总额”显示，例如 `10000.01/10500.00/20000.00`，`Overage(配置/Kiro)` 列同时显示每账号本地上限与 Kiro 返回的上限。

## 接入客户端

| 端点 | 用途 |
| --- | --- |
| `POST /v1/messages` | Claude Messages；别名 `/messages`、`/anthropic/v1/messages`。 |
| `POST /v1/messages/count_tokens` | 本地 Token 估算；Claude 别名也支持 `/count_tokens`。 |
| `POST /v1/chat/completions` | OpenAI Chat Completions；别名 `/chat/completions`。 |
| `POST /v1/responses` | OpenAI Responses；别名 `/responses`。 |
| `GET /v1/models` | 两类客户端共享的模型发现；别名 `/models`。 |
| `GET /health`、`GET /ready` | 已创建监听器上的存活检查和就绪检查。 |

Claude Code 的 `ANTHROPIC_BASE_URL` 使用 `http://127.0.0.1:5580`，
`ANTHROPIC_AUTH_TOKEN` 使用服务的客户端 Key。MCP 工具较多时可设置 `ENABLE_TOOL_SEARCH=auto`；
模型发现使用 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`。

Codex 使用 `http://127.0.0.1:5580/v1` 和 `wire_api = "responses"`。
[Codex 接入指南](docs/openai-responses.md)提供完整 provider 配置、状态上限、不支持参数和流式错误说明。
模型从 `kproxy models list` 中选择；别名不会扩大实际模型的输入窗口。

## 日常运维

```bash
kproxy status
kproxy stats --since 1h
kproxy logs show --tail 100
kproxy logs trace <TRACE_ID>
kproxy config show --effective
kproxy service list
kproxy help --all
kproxy help logs trace
kproxy guide balance
```

无参命令组显示帮助。脚本必须使用 `logs show`、`models list`、`tasks list`、`diagnose all`
等显式动作；其中 `diagnose all` 会对全部账号发起真实推理。更新脚本前阅读
[CLI 迁移](docs/startup-and-debugging.zh-CN.md#旧命令迁移)。服务配置、日志保留、Docker 生命周期和
systemd 管理见[启动与排障指南](docs/startup-and-debugging.zh-CN.md)。

## 文档

| 主题 | 入口 |
| --- | --- |
| 部署、CLI 迁移、日志与恢复 | [中文](docs/startup-and-debugging.zh-CN.md) · [English](docs/startup-and-debugging.md) |
| 协议限制、模型控制与上下文压缩 | [中文](docs/protocol-compatibility.zh-CN.md) · [English](docs/protocol-compatibility.md) |
| Responses、Codex 与状态续轮 | [接入指南](docs/openai-responses.md) |
| 多提供源路由与 GitHub Copilot | [接入指南](docs/providers-and-copilot.zh-CN.md) |
| 1.0 评估、缺口修复与发布步骤 | [发布方案](docs/release-readiness-1.0.0.zh-CN.md) |

## 开发

workspace crate 分别负责领域与配置、持久化、IPC、协议转换、provider adapter、上游访问、
调度、通知、daemon 和 CLI。开发流程见[贡献指南](CONTRIBUTING.md)，源码职责见[架构说明](CLAUDE.md)。

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

以上是要求运行的检查，不代表当前代码已全部通过。
[发布评估](docs/release-readiness-1.0.0.zh-CN.md)记录本次核查结果和剩余门槛，
[发布清单](docs/release-readiness-1.0.0.zh-CN.md#发布执行清单)说明 tag 与镜像行为。项目采用 [MIT 许可](LICENSE)。
