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

原生 Runtime 的 OAuth 请求、ControlPlane 模型目录和 extension 管理 REST 使用各自的
协议与身份头。管理请求默认访问 `management.{region}.kiro.dev`；缺失 Profile 的发现
结果有 15 分钟、最多 500 项的账号隔离缓存。默认 Web Search 使用 Runtime 根路径的
`InvokeMCP`；显式 `CROSSLANE_MCP_URL` / `upstream.web_search_endpoint` 保留旧
JSON-RPC 兼容格式。

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
Responses 的 namespace 函数工具和 custom 工具在上游使用唯一名称，返回客户端时恢复原来的 namespace
和名称。适配层分别对 namespace 和工具名保留 128 字符上限，拼接后的内部名称超过该长度时，
会在工具声明、历史调用及 tool choice 中统一使用请求内的唯一别名；流式、非流式输出均恢复原字段。
Claude Code 的 `thinking.display: "updates"` 会适配为上游支持的 `summarized`，
保留 thinking 设置；`omitted` 仍隐藏思考内容。
Kiro 工具调用在返回客户端前会校验是否匹配本次请求的工具声明，流式、非流式都适用。
已知别名恢复原名；客户端明确声明的 hash 名仍合法。未知名称返回
`upstream_tool_name_mismatch`：非流式为 HTTP 502，流式为错误事件，不切换账号重试，
也不输出该工具调用。`proxy.tool_name.mapping_miss` 日志包含 trace/request ID、
工具调用 ID 及有长度限制的工具名，不记录工具参数和 schema。
代理执行的 Tool Search/Web Search 继续使用服务端工具处理逻辑。
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
Claude 服务端搜索支持 `allowed_domains` 或 `blocked_domains`，两者不能同时提供。
域名自动包含子域名，搜索路径支持前缀和 `*` 通配符；域名必须使用 ASCII 主机名，不能带
协议、端口、凭据或主机名通配符。代理向 Kiro MCP 的查询加入 `site:` / `-site:` 约束，
并在结果进入模型续写、引用和加密回放前按 URL 再次过滤。无匹配时返回空搜索结果。
`user_location` 仍不支持，提供时会明确报错。

## 模型与额度

```bash
crosslane models list
crosslane model disable MODEL_ID
crosslane model enable MODEL_ID
crosslane pool --model MODEL_ID --explain
crosslane account overage show
```

模型 ID 从列表中选取。禁用模型后，新请求将无法使用。
业务接口 `GET /v1/models` 和 `GET /models` 返回当前 service 有效账号池内启用账号
所支持模型的并集，并继续检查 service/API key 来源权限、API key 模型权限及全局禁用模型。
Kiro、Copilot 和多来源列表都遵循账号标签、手工加入及显式排除规则。来源在该账号池内
没有启用账号时不返回该来源的模型；账号池为空时返回空列表。Kiro 在池内账号尚未发现
目录或动态发现关闭时保留静态目录兜底；Copilot 的目录发现及缓存聚合只使用池内账号。
CLI 模型列表是来源级管理查询，不受某个 service 账号池限制。

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

### Claude Code 的 MCP 工具按需加载

使用 Crosslane 的自定义 `ANTHROPIC_BASE_URL` 时，Claude Code 默认可能关闭工具搜索，
导致每轮把所有 MCP 工具加载进模型上下文。在启动 Claude Code 前显式启用：

```bash
ENABLE_TOOL_SEARCH=true claude
```

也可以将以下字段合并进 Claude Code 的 `~/.claude/settings.json`，然后重新启动会话：

```json
{
  "env": {
    "ENABLE_TOOL_SEARCH": "true"
  }
}
```

Crosslane 的 `features.enable_tool_search` 默认为 `true`。客户端 `ToolSearch` 调用
仍交给客户端执行；`defer_loading` 工具只在历史中被 `tool_reference` 引用或实际使用后
加载到 Kiro 请求，未命中的定义不进入上游上下文。已经使用的工具和显式 `tool_choice`
指定的工具保持可用；长工具名在上游使用别名，返回客户端时恢复原名。显式关闭代理工具
搜索时，带延迟工具的请求仍会被拒绝，不会静默加载全部工具。

普通内置工具、system 和对话历史仍会占用上下文；这项设置不会自动缩短这些内容。
可对照 `proxy.request.prepared` 的 `loaded_tool_count`、`deferred_tool_count`、
`loaded_tool_bytes` 和 `input_tokens` 判断实际减少量，再比较首包与完整响应耗时。
计数只涵盖本次请求携带的定义，不包含 Claude Code 留在客户端的完整 MCP 目录。
开关说明见 [Claude Code MCP 文档](https://code.claude.com/docs/en/mcp#configure-tool-search)。

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

可在对应 Copilot 实例的 `[provider.settings]` 中设置以下正整数，单位为毫秒：

```toml
stream_read_timeout_ms = 600000
oauth_refresh_timeout_ms = 30000
account_queue_wait_timeout_ms = 30000
```

读取超时限制连续无数据的时间，包括等待响应头；持续收到数据的长流不会被总时长
限制截断。OAuth 刷新总超时覆盖响应头和响应体；排队超时会取消等待，返回本地
`503` / `server_overloaded`，不标记供应商状态或影响账号健康。

Kiro 在流式重试或续答前释放上一轮连接名额。本地槽位耗尽返回 `503` /
`server_overloaded`，不会换账号重复等待或扣减账号健康。在首个有效内容前，重试次数
遵循 `upstream.max_retries`，账号数量不会扩大预算；工具结果后的空回复仍保留独立的
有界语义重试。工具结果续答出现有效文本后立即流出；工具参数校验完成后直接发送，
不再额外休眠。`features.tool_call_buffer_delay_ms` 为兼容旧配置保留，默认值为零，
已不参与请求等待。

上述控制适用于 Anthropic Messages、OpenAI Chat Completions 和 Responses。
请求鉴权、客户端检查、模型权限和 50 MiB 请求体上限保持原有规则；工具参数校验和
各协议的完成/失败事件格式保持不变。启用 `crs` 时，Messages 的流式服务错误仍使用
HTTP 200 SSE，并在 `error.status` 中携带实际状态，例如本地过载的 503。

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

启用 `context.auto_compact_on_overflow`（默认开启）时，历史导致字节或 token 超限会在
上游发送前进行语义摘要。成功提交的 checkpoint 必须覆盖所有被移除的原文，包括正文
中部事实和后续修正。摘要超时、容量不足、上游失败或完整 checkpoint 无法放入窗口时，
返回 `compaction_failed`（422），保留原始上下文，不再把截取头尾的片段作为完整压缩边界。
系统指令、已加载工具、当前消息及其必需的工具调用链保持原样。不可压缩部分自身超限
时，400 错误通过 `current_message` 或 `required_context` 区分范围，提示缩小或拆分
输入、分页读取结果；压缩旧历史无法解决这类超限。

单次粘贴、附件或工具结果过大时，恢复步骤直接写在错误消息里，让 Claude Code 能显示：
停止重复发送同一份大输入，拆成较小片段或分页读取工具结果。保留当前 session，尝试
`/compact`，也可以发送一条不附带大输入的简短消息，让客户端尝试自动压缩。简短消息
可能与被拒绝的大输入合并；大输入仍处于当前轮次，或可压缩的对话组不足时，直接
`/compact` 也可能失败。如果仍失败，在交互式 Claude Code 中用 Esc Esc 或 `/rewind`
只编辑过大的那一轮，拆小后重发，保留之前的对话。`required_context` 超限要先缩小
必需的系统指令、启用的工具定义或其他必需输入，压缩历史无法缩小这些内容。
这些带本地数值诊断的 Messages 错误保留语义状态 400、
`context_length_exceeded`、数值诊断及 `retryable: false`；可压缩的历史超限与上游
上下文错误保留原有文案，继续触发客户端自动压缩。启用 `crs` 后，本地上下文错误的
非流式 JSON 使用 HTTP 200 传输，在 `error.status` 和 `x-crosslane-error-status` 中
保留 400，避免 CRS Console 重写恢复详情；流式链路继续使用 SSE 错误事件。

摘要预算采用所选模型的窗口，全局窗口只作缺少元数据时的回退。当前用户消息作为可分段
的源记录，不再重复放进每段指令。每段同时满足 token 和序列化字节限制；初始最多 16 段、
同时最多两个请求，共享一个操作截止时间。上游原生上下文限制拒绝摘要时，可以在原截止
时间和用量统计内将完整原文重新分成更小的段，只重试一次，不截断内容。
摘要流报告上游暂时不可用，或大段输出缺少摘要闭合标签时，只对失败的源分段重试一次，
大段会拆成更小的段；小段的格式错误明确返回失败。
已经成功的其他摘要继续保留，失败轮次的部分输出不会发布，每次调用单独统计用量。
子分段串行执行，沿用原并发限制和截止时间；日志事件
`proxy.compaction.summary_part_retry.started` 可识别这条恢复路径与触发原因。失败元数据
区分上游流错误和摘要校验错误，并记录标签数量，不记录摘要内容。
`proxy.upstream.context_usage` 将上游上下文百分比或原生输入计数与本地估算分别记录，
避免把随内容变化的 tokenizer 差异误认为固定字节或 token 限制。

Tool Search 检查包含新工具定义与结果的完整续轮请求。持有账号名额时，只能提交在预算内
完整保留原文的 checkpoint；无法完整保留时，保留历史并拒绝超限续轮，避免丢失事实或
再获取一个摘要请求名额。客户端忽略 checkpoint 时，服务端按已校验的历史前缀重放
完整 checkpoint。

普通 Kiro 请求若没有任何助手输出，会重试一次；仍为空时明确返回错误。有效正文、工具
调用、推理和终止语义继续保留。流式重试不会重放已展示的内容或重复发布压缩边界。
设置 `features.buffer_tool_calls=false` 时，普通工具参数片段仍立即流式输出；
展示内容后发生流错误会结束本次响应，不再重复调用上游。
非流式轮次在重试或工具续轮前释放已完成的上游连接名额，单连接池也能正常继续。

大文本的 token 估算按 UTF-8 边界分块，并为边界保留少量余量，避免 tokenizer 在大段
连续空白中抛异常；实际消息与工具 JSON 不会因此拆分。`/v1/messages/count_tokens`
继续只返回本地估算，不触发新的摘要或压缩。关闭自动压缩时保留原有超限拒绝行为。
日志中的 `payload_budget_overflow`、`pending_server_tools_payload_overflow` 和
`tool_search_continuation_overflow` 可用于确认压缩触发位置。

Kiro 返回 `CONTENT_LENGTH_EXCEEDS_THRESHOLD` 或 `Input content length exceeds threshold`
时，即使本地估算未超过模型公布的窗口，也会进入上下文恢复流程。自动恢复只重试一次；
客户端主动请求摘要时，关闭自动压缩仍可恢复。摘要恢复会分段处理完整原文，并保留客户端
摘要指令。该上游错误没有报告准确的 token 限制，也不会被当作账号故障。

Claude Code 原生摘要请求会在最终生成时整合完整、按时间排序的分段 checkpoint，
省去重复的上游合并请求。完整分段也会附在最终摘要指令旁，放在源历史之后；原生最终摘要不再重复携带 checkpoint 已覆盖的可选原文，
避免早期事实被较长的近期原文和局部缺失声明掩盖。任一分段的明确值优先于其他分段的局部“没看到”声明；
只有明确的后续修正或决策变更才能取代先前事实。分段 checkpoint 不会被截断。
如果完整内容放不进目标窗口或实际路由到的较小窗口，Crosslane 会对已生成的分段
进行语义合并，并沿用摘要输入、时间与用量限制。其他服务端压缩请求保留原来的上游合并。
`proxy.compaction.summary_merge.completed` 日志增加 `merge_mode`（`client_final_summary`
或 `upstream`）和 `duration_ms`；上游合并另有 `proxy.compaction.summary_merge.started`。
单分段摘要保持原来的处理方式。

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
固定文案。为保留本地 `context_length_exceeded` / `context_validation` 错误，非流式
Claude Messages 使用 HTTP 200 传输 `type: "error"` JSON，在 `error.status` 和
`x-crosslane-error-status` 中保留语义状态 400，并禁止缓存。正文不包含成功消息、结束原因或
用量；客户端必须检查 `type` / `error`，不能仅凭 HTTP 200 判断生成成功。请求统计仍记为
400 失败，同时记录客户端传输状态 200。这个例外只适用于启用 `crs` 的本地上下文错误；
未启用 `crs`、OpenAI 接口、客户端认证及供应商错误保留原 HTTP 状态。
其他非流式错误仍可能被 Console 脱敏；CCR 路径会保留其原始非流式错误正文。

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
