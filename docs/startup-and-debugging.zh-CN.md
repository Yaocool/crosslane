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

## 命令导航与归属

`-h`、`--help` 和 `help` 都显示完整参数及示例，帮助不需要运行 daemon：

```bash
crosslane service edit -h
crosslane help service edit
crosslane service help edit
crosslane help --all
crosslane help account --all
```

| 命令组 | 管理对象 |
| --- | --- |
| `provider` | 上游实例、驱动设置、默认模型和回退策略 |
| `account` | 上游登录账号、凭证、健康状态与标签 |
| `service` | 业务监听、允许来源、客户端 Key 绑定和账号池 |
| `apikey` | 客户端访问凭据、来源/模型权限、限额与用量 |
| `models` / `model-map` | 模型目录与全局禁用 / 客户端模型名映射规则 |
| `tasks` / `alert` | 周期任务 / 共享通知目标及当前 Kiro 告警事件 |

无参数的顶层命令组显示帮助；自动化中使用 `--json` 时必须指定操作。`account overage`
兼容旧版默认 `show` 行为。`tasks list` 标明可手工运行的任务，`tasks run` 的帮助和
Shell 补全仅列出可手工运行项；watcher 和 adaptive_admission 由调度器运行。

查询中的 `--provider` 选择实例，创建/编辑 service、apikey、model-map 时则设置或替换
来源权限。权限编辑必须显式传参，`CROSSLANE_PROVIDER` 不会覆盖已有资源的权限。
全局命令和直接通过 ID/名称定位的资源命令不接受来源筛选，也不受这个环境默认值影响；
模型全局禁用/启用兼容 `--provider all` 标记。告警目标通过名称定位，当前账号事件来自 Kiro。
空编辑和互斥的设置/清除参数会在连接 daemon 前报错，并给出对应帮助入口。

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

同时使用 Kiro 和 Copilot 的服务会自动均衡无来源前缀、未命中模型映射的请求：

```bash
crosslane service create --name mixed --host 127.0.0.1 --provider kiro --provider copilot --default-provider kiro
```

服务及客户端 Key 都必须允许对应来源。路由先从该服务的账号池及有效模型缓存中查找
同一模型（包含等价别名），再按账号容量与当前并发选择压力较低的来源；压力相同时轮转。
跨来源调度不会使用 Copilot 剩余额度估值，也不会把模型切到其他版本。来源确定后，
各自账号池保留原有选号逻辑。`default_provider` 为初始轮转偏好，并用于模型映射及
模型目录尚未发现可用路由时的兼容路径；仅绑定账号不会扩大服务或 Key 的来源权限。

响应交付前的连接故障、上游账号认证/额度错误、限流及服务异常可切换到另一兼容来源。
失败来源按服务和模型进入 5 秒起的冷却，连续失败时增加至最多 60 秒；到期允许单个
恢复请求，成功后重新均衡。客户端参数、权限及客户端 Key 额度错误不会触发灾备。
已开始交付的流式响应保持原有错误事件，后续请求避开失败来源，避免重放已输出内容。
Messages、Chat Completions、Responses 及 `count_tokens` 使用相同调度入口。
多候选 Chat 请求保留原有批处理方式；Kiro 拆分出的每个候选分别参与自动调度。
计数接口的冷却状态独立，Kiro 本地计数成功不会重置生成接口的故障状态。
自动路由每个来源在返回响应前最多等待 5 分钟；可通过 `[provider.routing]` 的
`failover_timeout_ms` 调整（CLI：`crosslane provider edit copilot --failover-timeout-ms 300000`），
设为 `0` 可取消此等待上限。开始交付后的流式响应
继续使用原有空闲超时，该参数不会截断已经开始的长响应。

`kiro/MODEL_ID`、`copilot/MODEL_ID` 和显式模型映射保留指定来源语义。
将请求改成另一模型的默认模型覆盖也沿用原有路径；覆盖为同一模型或等价别名时仍可均衡。
带 `previous_response_id` 的 Responses 请求沿用创建该 ID 的来源，包括匿名客户端。
显式跨来源模型映射仍需开启源 provider 的 `allow_cross_provider_fallback`；
自动均衡使用服务与 Key 已允许的来源范围。用日志事件 `upstream.provider.selected` 和
`upstream.provider.failover` 配合 `trace_id` 查看均衡与切换过程。

服务只接受已绑定的客户端 Key。生成接口还会检查客户端产品 User-Agent。接入其他客户端时，
操作员可在服务或客户端 Key 上设置 `--skip-user-agent-check true`；Key 认证和其他服务限制
仍然生效。调整访问前先查看命令帮助：

```bash
crosslane help service edit
crosslane help apikey edit
```

## 桌面端工具与网页搜索

Copilot Responses 流保留首次出现的响应 ID，并按输出序号统一消息 ID，使增量、
结束事件和最终输出快照指向同一条消息。Copilot 在事件间改变不透明 ID 时，
桌面客户端仍能合并流式内容与完成消息，避免回答显示两遍。
消息阶段、工具调用 ID、引用和 `copilot_usage` 保持原样。

Claude 上游不接受工具 schema 根部的 `oneOf`、`allOf`、`anyOf` 时，Crosslane
将组合约束移到等价的条件子 schema，保留分支必填字段、定义引用和闭合对象约束。
Responses 的 namespace 函数工具在上游使用唯一函数名，返回客户端时恢复原来的 namespace
和名称。Claude Code 的 `thinking.display: "updates"` 会适配为上游支持的 `summarized`，
保留 thinking 设置；`omitted` 仍隐藏思考内容。
Copilot Messages 请求会移除上游不支持的 Anthropic `safeguards` 扩展，token counting
请求也使用相同适配，避免新版 Claude Code 请求被拒绝。

Copilot 普通生成请求保持原来源，即使 Responses 工具列表声明了 `web_search`。
模型实际调用搜索后，搜索及本轮后续回答交给 Kiro，不回退到 Copilot。Claude Code 的
WebSearch 服务端子请求也使用此路径。为 Copilot 服务在已有 `[[proxy_service]]` 配置中加入：

```toml
web_search_provider = "kiro" # Kiro provider 实例 ID
```

这项配置只授权搜索及其后续生成，仍受服务账号标签、加入/排除账号和客户端模型权限限制。
Kiro 服务省略该项时使用自身默认来源。修改后执行 `crosslane config reload`。
未配置 Kiro、没有可用账号、搜索失败或搜索后的 Kiro 生成失败时，返回
`web_search is not supported`，不把失败包装成搜索成功。

Responses 返回 `web_search_call`、真实来源链接与 URL 引用，并保留流式事件和工具历史。
`external_web_access:false` 仅复用该服务和 Key 的五分钟搜索缓存；未命中时明确报错。
需要实时搜索的 Codex 客户端可在配置中设置 `web_search = "live"`。搜索历史续接仅在
同一服务和 Key 下有效，最长保留三十分钟；服务重启后需要发送新请求。
Claude 服务端搜索目前仍不支持非空的 `allowed_domains`、`blocked_domains` 和
`user_location`；这些配置会明确报错。

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

## 请求延迟控制

Copilot 业务模型查询优先复用启用账号在 `provider.models.max_stale_ms` 范围内的
目录快照，由周期任务或显式刷新更新目录。其他账号缺少目录时，不会阻塞已有可用
快照的请求；首次发现或全部快照过期时仍需查询上游，过期模型不会用于授权和选号。
目录刷新按 `provider.settings.model_refresh_concurrency` 控制同时查询的账号数，
默认 32，实际并发不超过启用账号数。大账号池可提高该值，例如 64；上游出现限流时
可调低。该值必须大于零，不影响已有可用快照的业务查询。

```toml
[provider.settings]
model_refresh_concurrency = 32
```

Kiro 在流式重试或续答前释放上一轮连接名额。工具结果后的空回复仍保留独立的
有界语义重试。工具结果续答出现有效文本后立即流出；工具参数校验完成后直接发送，
不再额外休眠。`features.tool_call_buffer_delay_ms` 为兼容旧配置保留，默认值为零，
已不参与请求等待。

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

每个 HTTP 请求结束时都会记录一次 `http.response.completed`，覆盖流式响应、拒绝请求
和响应体中断。耗时单位均为毫秒：

| 字段 / 事件 | 含义 |
| --- | --- |
| `duration_ms` | 从服务器收到请求头，到响应体结束或被丢弃的总耗时，包含请求上传和所有来源尝试 |
| `response_ready_ms` | 从收到请求到响应头准备好的耗时；原有 `http.response.ready.duration_ms` 也是这个含义 |
| `first_byte_ms` | 从收到请求到首个非空响应数据帧交给 HTTP 传输层的耗时；无数据的响应不记录此字段 |
| `response_body_ms` | 响应头准备好后，到响应体结束或被丢弃的耗时 |
| `response_bytes` | 结束或中断前交给 HTTP 传输层的响应字节数 |
| `http.request.body_read.body_read_ms` | 读取请求体的耗时；读取失败时也记录已收到的 `body_bytes` |
| `proxy.stream.preflight.duration_ms` | CRS 流预检耗时，仅在启用 `crs` 的构建中记录 |

结束状态 `outcome` 为 `completed`、`cancelled` 或 `body_error`，表示 HTTP 响应体的
传输状态。HTTP 200 内的 SSE 错误仍需结合来源或流错误事件判断。首帧可能是 SSE 开场
事件或保活帧，因此 `first_byte_ms` 不等于模型首个 token 的等待时间；这些指标也不代表
客户端实际接收或界面渲染完成的时间。
原有 `proxy.*` 完成事件和 `crosslane stats` 保留操作耗时口径，可能从请求体上传后开始
计时，也可能包含内部子请求。排查整个请求的总延迟应看最终 HTTP 事件。若
`body_read_ms` 很大，或出现 HTTP 408 `request body read timed out`，延迟发生在模型
调度前，应先检查客户端上传及网络、代理链路。CRS 流预检在等待内容或早期错误期间，
可能暂存 SSE 开场事件最多 5 秒。
请求体读取按数据块设置 15 秒空闲超时；持续有数据到达时，总上传时长可以超过 15 秒。

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
该 feature 对二进制中的所有 Claude Messages 监听服务生效，普通 Cargo 构建默认关闭。
完整 Docker 镜像默认启用全部 Cargo feature，包括 `sso` 和 `crs`。
使用 `./deploy/docker-setup.sh --build` 本地构建并部署，或
`docker build --target runtime-full -t crosslane:full .` 仅构建镜像。
如需只启用部分 feature，可为 `docker build` 传入
`--build-arg CROSSLANED_FEATURES=crosslaned/sso`，此时关闭 `crs`。

新构建的 `crosslaned --version` 会显示编译 feature；容器内运行
`docker exec <容器名> crosslaned --version`，检查是否包含 `crs=true`。
旧构建可直接探测 Crosslane 监听端口（不经过 CRS）：

```bash
curl -i "$CROSSLANE_URL/v1/messages" \
  -H "x-api-key: $CROSSLANE_API_KEY" \
  -H 'User-Agent: claude-cli/2.1.295 (external, cli)' \
  -H 'Content-Type: application/json' --data '{"stream":true}'
```

此请求通过认证后仅触发本地必填字段校验，不调用上游。启用 `crs` 时返回
HTTP 200、`text/event-stream` 和 `event: error`；未启用时返回 HTTP 400 JSON。

Kiro 和 Copilot 返回的错误状态码保留原值，包括 401、402、403、413、503、504 和 529。
非流式响应及未启用 `crs` 的响应使用原 HTTP 状态码；CRS 兼容的 Claude 流式响应将原值
写入 SSE 错误字段。错误分类和凭据脱敏不映射供应商状态码。没有供应商 HTTP 错误响应的
传输、协议故障继续使用代理自身的错误状态。

启用 `crs` 后，错误正文保留 Anthropic 格式、脱敏原因、操作建议、失败阶段、请求 ID、
`error.status` 和已知的真实 `error.upstream_status`。上下文错误还保留 token 诊断和
压缩重试结果。无需修改 CRS 代码。现有 CRS Console 会把非 2xx JSON 错误正文替换成
固定文案，因此只有流式响应能通过其现有转发保留详细诊断；CCR 路径也会保留非流式错误正文。

Claude 请求设置 `stream: true` 且启用 `crs` 时，供应商错误采用 HTTP 200 加 SSE
`event: error`，在 `error.status`、`error.upstream_status` 和 `x-crosslane-error-status`
中保留原始状态码。流开始前的错误先初始化零用量的空文本块，避免 Claude Code 退回非流式
重试后被 Console 清理掉错误详情。客户端认证、方法和 HTTP 请求体限制错误保留原 HTTP 状态。
成功的 SSE 响应已经开始输出后，后续错误继续通过流内事件表达。请求统计分别记录失败状态和
客户端 HTTP 200 传输状态。服务端检查最初最多 64 KiB 或五秒的 SSE 事件，也会补充已经开始
输出助手内容后的错误诊断。

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
