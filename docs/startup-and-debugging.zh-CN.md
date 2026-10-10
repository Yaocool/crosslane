# Crosslane 运维指南

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [项目说明](../README.zh-CN.md)

本文介绍部署、账号、服务访问、日常检查和备份。命令在仓库根目录执行，并使用
与服务匹配的 `crosslane` CLI。

## 环境与配置

Docker 部署需要 Docker Engine 和 Compose。本地编译支持 Linux、macOS，需要 Rust
和 C 工具链；原生环境使用浏览器登录时还需 Chrome 或 Chromium。

本地安装可将 `.env.example` 复制为 `.env`。从不同目录启动 daemon 和 CLI 时，
将 `CROSSLANE_HOME` 设为绝对路径。使用 `crosslane config path` 查找当前配置，
编辑后运行 `crosslane config reload`。不要把账号凭证写进命令参数、日志或版本库。

## Docker

构建当前源码并安装宿主机 CLI 命令：

```bash
./deploy/docker-setup.sh --build
crosslane health
crosslane ready
```

也可使用 `./deploy/docker-setup.sh --image IMAGE` 部署可用镜像。宿主机 CLI 默认安装到
`/usr/local/bin/crosslane`；其他位置使用安装脚本的 `--target PATH` 参数。

Compose 使用 host network。本机访问的服务应绑定 `127.0.0.1`；向外开放时需限制端口
并配置 TLS。数据保存在 `crosslane-data` 卷中。日常检查与升级：

```bash
docker compose ps
docker compose logs -f crosslaned
./deploy/docker-upgrade.sh
crosslane version
crosslane ready
```

升级前停止写入并备份完整数据卷。镜像回退不会撤销数据变更。`crosslane stop` 可停止部署
并保留数据；移除前先查看 `crosslane uninstall --help`。

## 账号

### Kiro

可从 CLI 环境变量或标准输入导入 headless API key：

```bash
crosslane account add-api-key --email ci@example.com --region us-east-1
crosslane account add-api-key --email ci@example.com --region us-east-1 --key-stdin < /secure/kiro-key
```

第一条命令读取 CLI 环境中的 `KIRO_API_KEY`。企业 SSO 需先设置组织的登录入口，再添加账号：

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

```bash
crosslane config reload
crosslane account add-sso --email user@example.com --password-stdin < /secure/kiro-sso-password
```

交互、批量和 stdin 参数见 `crosslane account add-sso --help`。使用
`crosslane account list` 检查账号，刷新选项见 `crosslane account refresh --help`。
账号导出默认脱敏；包含凭证的导出只应存放在受保护的备份中。

### GitHub Copilot

先创建 Copilot 来源，再通过 GitHub Device Flow 授权，或从标准输入导入已有 token：

```bash
crosslane provider add --id copilot --kind copilot
crosslane account add --provider copilot --auth device-flow
crosslane account add --provider copilot --token-stdin < /secure/github-token
crosslane account list --provider copilot
```

选择组织允许的账号方式。`--provider` 使用 `provider add --id` 指定的实例 ID。
无头服务器可用 `account add --provider copilot --headless` 选择 GitHub 密码登录，
或使用 `--username <GitHub用户名或邮箱> --password-stdin`。邮箱必须属于授权账号且已验证；
程序会通过 GitHub API 核验，核验失败不会保存账号。Azure SSO 的 `--username` 仍须填写
完整 GitHub 用户名，Azure 邮箱使用 `--sso-username`。MFA 动态码或推送仍需本人完成。
同一服务账号池内，Copilot 按账号当前并发数和最近使用时间选择账号；
`[provider.pool.balance]` 可设置 `weight_active`、`weight_idle` 和
`idle_window_ms`，默认分别为 `0.8`、`0.2` 和 `300000`。
Copilot 当前没有可靠的单账号剩余额度数据，因此不按 Kiro credits 打分。

`account list` 会显示 Copilot 套餐、API Token 到期时间、额度重置时间及各账号
在本机累计的 AIU，上游 `credits_used` 月用量单独展示。按 token 计费的额度使用 credits
单位，已用量优先取 `credits_used`，余额优先取保留小数的 `quota_remaining`；旧版额度
保留请求数单位。Business 的占位数据（`unlimited: true` 且总额、余额为零）显示
“总额/余额未知”，不显示 `0/0` 或“剩余 100%”。JSON 新增 `details.quota_summary`，
包含个人范围、单位、已用量、总额、余额、剩余百分比、不限额标记及缓存状态；未知数值
为 null，原始 `details.info.quota_snapshots` 保留。
`account show ID --provider copilot`
还会显示 GitHub 访问凭据和
刷新凭据的有效期、额度查询时间及 AIU 统计时间范围；`--json` 保留安全的上游额度字段。
API Token 会自动续期，其到期时间不是订阅到期时间。上游未返回的有效期和计费数据
显示为未知。额度查询成功和失败均缓存五分钟；失败保留旧数据，不改变账号健康状态。

AIU 从本机观察到的第一条计费响应开始累计，包含普通 JSON 和流式响应，重启和清空
客户端 Key 的统计均不会清除账号累计值。计量取自上游
`copilot_usage.total_nano_aiu`（1 AIU = 1,000,000,000 nano AIU），不代表 GitHub
全账号用量或组织剩余额度。个人额度不限，包括 Business 返回的零上限及
`credits_used`，不能用来推断组织余额。Kiro credits 与 Copilot AIU 分别统计。

## 服务与客户端访问

新安装不会自动创建业务监听。创建服务并妥善保存命令输出的客户端 API Key：

```bash
crosslane service create --name main --host 127.0.0.1
crosslane service list
crosslane ready
```

仅使用 Copilot 的服务可以指定来源及默认来源：

```bash
crosslane service create --name copilot --host 127.0.0.1 --provider copilot --default-provider copilot
```

服务只接受已绑定的客户端 Key。生成接口还会检查客户端产品 User-Agent。接入其他客户端时，
操作员可在服务或客户端 Key 上设置 `--skip-user-agent-check true`；Key 认证和其他服务限制
仍然生效。调整访问前先查看命令帮助：

```bash
crosslane help service edit
crosslane help apikey edit
```

## 模型与额度

```bash
crosslane models list
crosslane model disable MODEL_ID
crosslane model enable MODEL_ID
crosslane pool --model MODEL_ID --explain
crosslane account overage show
```

模型 ID 从列表中选取。禁用模型后，新请求将无法使用。
Copilot 也接受等价的客户端别名，例如将 `claude-opus-4-8` 解析为模型目录中的
`claude-opus-4.8`。显式模型映射优先执行，权限按解析后的 ID 检查。要将缺失的
Claude 版本路由到同一 Opus、Sonnet 或 Haiku 系列中最近的可用版本，可运行
`crosslane provider edit copilot --enable-model-fallback true`。
优先选择相同主版本，再比较版本距离，距离相同时选择较新版本。例如目录中最近的
Opus 型号为 `claude-opus-4.8` 时，缺失的 `claude-opus-4-6` 会路由到该型号。
有歧义的 ID、未知系列以及 `-fast` 等变体仍需显式映射。回退继续检查禁用模型和
API key 权限。此来源开关默认关闭；可运行
`crosslane models resolve claude-opus-4-6 --provider copilot --json` 查看结果。
Kiro overage 默认关闭。
要将每个账号的超额额度上限设为 500 credits，可运行
`crosslane account overage enable --max-credits 500`；关闭时运行
`crosslane account overage disable`。上游限制和已在处理中的请求仍可能影响最终用量。

## 日志与排障

```bash
crosslane logs show --tail 100
crosslane logs follow --level warn
crosslane logs trace <TRACE_ID>
crosslane status --since 30m
crosslane stats --detail --since 1h
```

业务请求日志会记录客户端提供的 `user_agent`。可用响应头 `x-trace-id` 或 `request-id`
查找请求日志。排查失败时同时看响应状态、错误码和 Trace。流式响应也可能在 HTTP 200 后失败，
应检查最终事件。
`upstream.account.selected` 和完成事件会记录 `provider_id`、`account_id`；
用 `trace_id` 可串联同一请求的账号切换记录。

| 现象 | 优先检查 |
| --- | --- |
| `health` 失败 | daemon/容器状态和日志 |
| `ready` 失败 | 服务、账号、模型和额度状态 |
| 401 或客户端被拒绝 | 绑定的客户端 Key、User-Agent 策略 |
| 503 | 账号可用性、额度与并发 |
| 流式中断 | 最终流事件和 Trace 日志 |
| 上下文超限 | 实际选中模型及请求大小 |

### Kiro 请求大小与自动压缩

Anthropic 请求先经过 50 MiB 的 HTTP 请求体限制，翻译后的 Kiro 请求默认最多
8 MiB（`context.max_upstream_payload_bytes`）。模型输入窗口优先采用实际选中模型的
元数据；普通请求和压缩后的请求分别使用 `safe_input_ratio` 与
`compact_safe_input_ratio`，默认 0.95 和 0.99。

启用 `context.auto_compact_on_overflow`（默认开启）时，历史导致字节超限会在上游发送前
先做本地提取式压缩，不需要客户端先发送 compact 指令。Token 超限仍走语义摘要和本地
回退流程。系统指令、已加载工具定义、当前消息及当前工具结果所依赖的完整工具调用链
保留原样；这些不可压缩部分仍超限时返回具体的 400 错误。

Tool Search 先预估压缩历史后的工具加载空间，再检查包含新工具与结果的完整续轮请求。
续轮超限时使用本地压缩，避免持有账号并发名额时再请求摘要模型。流式压缩 checkpoint
之后会补回之前已加载工具的 references，保证下一次工具结果回传仍能识别这些工具。
客户端忽略 checkpoint 时，服务端仍可按已校验的历史前缀重放最新 checkpoint。

大文本的 token 估算按 UTF-8 边界分块，并为边界保留少量余量，避免 tokenizer 在大段
连续空白中抛异常；实际消息与工具 JSON 不会因此拆分。`/v1/messages/count_tokens`
继续只返回本地估算，不触发新的摘要或压缩。关闭自动压缩时保留原有超限拒绝行为。
日志中的 `payload_budget_overflow`、`pending_server_tools_payload_overflow` 和
`tool_search_continuation_overflow` 可用于确认压缩触发位置。

Kiro 返回 `CONTENT_LENGTH_EXCEEDS_THRESHOLD` 或 `Input content length exceeds threshold`
时，即使本地估算未超过模型公布的窗口，也会进入上下文恢复流程。自动恢复只重试一次；
客户端主动请求摘要时，关闭自动压缩仍可恢复。摘要恢复会分段处理完整原文，并保留客户端
摘要指令。该上游错误没有报告准确的 token 限制，也不会被当作账号故障。

### 客户端与会话归属

所有构建都支持客户端识别，无需启用 `crs` feature。Claude Code 从旧格式或 JSON 字符串格式的
`metadata.user_id` 提取设备、会话；Codex 从 `client_metadata` 提取
`x-codex-installation-id`、`session_id`、`thread_id`，相应 HTTP 头作为后备。
CRS Console 和 OpenAI Responses 转发保留这些字段时即可使用，无需修改 CRS。

`proxy.client.context` 事件及请求日志的 `diagnostics.client_context` 记录哈希后的
`user_ref`、`session_ref`、`thread_ref`、客户端类型与身份来源。同一设备/安装在不同会话中
保持相同用户归属；这不代表自然人或 CRS Key。信息缺失时保持未知，异常或过长的标识会被忽略。

生成与 token 计数采用一致的会话规则，压缩重放按服务、Crosslane Key、设备、会话/线程隔离，
并继续校验完整历史前缀。Responses 续接在原有服务/Key 归属校验后继承省略的信息；分叉线程使用
独立的上游会话，已知设备冲突返回请求错误。由 provider 保存历史的 Responses 归属信息仅在
成功响应后保存在内存，最多 256 条、30 分钟；`store: false` 禁止保留。
账号池调度、额度与按请求内容计算的 token 缓存继续使用原有策略。

### Claude Relay Service 响应兼容

Claude Messages 作为 CRS Console/CCR 上游时，使用
`cargo build -p crosslaned --bin crosslaned --release --features crs` 构建。
该 feature 对二进制中的所有 Claude Messages 监听服务生效，默认关闭。
完整 Docker 镜像可使用
`docker build --target runtime-full --build-arg CROSSLANED_FEATURES=crosslaned/sso,crosslaned/crs -t crosslane:crs .`；
未指定此参数时，完整镜像只启用 `sso`。

启用 `crs` 后，非流式请求的异常上游响应（包括 HTTP 200 错误对象）会转换为
Anthropic 格式 JSON。普通上游 400/413 转为 HTTP 502，可明确修正的请求错误保留 HTTP 400。

CRS Console 会把非 200 错误体替换为固定文案。有效请求设置 `stream: true` 时，
Crosslane 对 400、404、408、422、429 和 5xx 使用 HTTP 200 加 SSE `event: error`。
客户端认证、方法和 HTTP 请求体限制错误保留原有 HTTP 状态。服务端检查最初最多
64 KiB 或五秒的 SSE 事件，也会规范化已经开始输出助手内容后的错误。

错误保留 `error.status`、已知的真实 `error.upstream_status`、稳定的 `error.code` 和
`request_id`；流开始前的 SSE 错误还带有 `x-crosslane-error-status`。
文案包含操作建议、失败阶段和请求 ID；上下文错误保留已知 token 数量，并在压缩重试
失败时说明恢复结果。请求统计分别记录业务失败状态和客户端 HTTP 200 传输状态。

## 备份与恢复

升级或修改可能影响数据的配置前先备份。标准 Docker 部署可停止写入后复制数据目录：

```bash
umask 077
backup_dir="$HOME/crosslane-backup-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$backup_dir"
docker compose stop crosslaned
docker compose cp crosslaned:/var/lib/crosslane/. "$backup_dir/"
docker compose start crosslaned
```

备份中包含凭证，应检查内容并妥善保护。原生部署先停止 daemon，再根据
`crosslane config path` 显示的位置同时备份配置和数据。恢复时先写入隔离目录或新卷，
保留所有权和权限；核对 `ready`、账号、服务及真实请求后再切回流量。
