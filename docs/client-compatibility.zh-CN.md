# 客户端兼容

[English](client-compatibility.md) | [简体中文](client-compatibility.zh-CN.md) | [项目说明](../README.zh-CN.md)

本文介绍工具与网页搜索、会话归属、CRS 传输和 Kiro 上游拒绝。服务监听与访问控制见
[运维指南](startup-and-debugging.zh-CN.md#服务与客户端访问)，缓存、脱敏及压缩见[请求处理](request-processing.zh-CN.md)。

[工具协议](#工具协议) · [网页搜索](#网页搜索) · [会话归属](#客户端与会话归属) · [CRS](#claude-relay-service-响应兼容) · [Kiro 端点与拒绝](#kiro-端点与上游拒绝)

## 工具协议

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

## 网页搜索

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

## 客户端与会话归属

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

## Claude Relay Service 响应兼容

Claude Messages 作为 CRS Console/CCR 上游时，使用
`cargo build -p crosslaned --bin crosslaned --release --features crs` 构建。
该 feature 对二进制中的所有 Claude Messages 监听服务生效，普通 Cargo 构建默认关闭。
默认使用 Console 兼容策略。只接入 CCR 的监听服务可在现有 `[[proxy_service]]` 项内配置
`crs_relay = "ccr"`；该策略保留 HTTP 错误状态，供 Claude Code 执行原生参数恢复。
省略此项或设置为 `"console"` 时，继续使用下文的 Console 错误传输策略。
此配置在未启用 `crs` 的构建中不改变 HTTP 行为。
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

此请求通过认证后仅触发本地必填字段校验，不调用上游。Console 兼容策略下返回
HTTP 200、`text/event-stream` 和 `event: error`；CCR 策略或未启用时返回 HTTP 400 JSON。

### 错误与流式传输

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

Claude 请求设置 `stream: true` 且采用 Console 策略时，供应商错误采用 HTTP 200 加 SSE
`event: error`，在 `error.status`、`error.upstream_status` 和 `x-crosslane-error-status`
中保留原始状态码。流开始前的错误先初始化零用量的空文本块，避免 Claude Code 退回非流式
重试后被 Console 清理掉错误详情。客户端认证、方法和 HTTP 请求体限制错误保留原 HTTP 状态。
成功的 SSE 响应已经开始输出后，后续错误继续通过流内事件表达。请求统计分别记录失败状态和
客户端 HTTP 200 传输状态。服务端检查最初最多 64 KiB 或五秒的 SSE 事件，也会补充已经开始
输出助手内容后的错误诊断。

Console 路径遇到明确的 `output_config.effort` 不受支持的 HTTP 400 时，只在输出前向
同一账号重试一次，且只删除该字段；不会删除 thinking 历史、签名或工具参数。CCR 策略保留
HTTP 400，由客户端自行恢复。其他字段拒绝和其他 HTTP 400 不会触发这次兼容重试。
重试只锁定原 provider 实例的原账号，保留服务排除项及 provider 权限，不会将标签池或
显式绑定的账号池扩大为全局账号池。

流内 JSON 字符串以 ASCII Unicode 转义传给 CRS，避免 CRS 逐块 UTF-8 解码损坏中文或
工具参数，客户端解析后的值保持一致；转义仍会增加线路字节数。
单个事件超过 64 KiB 时只跳过该事件的诊断解析，
后续事件仍会检查。原生 Messages 流缺少结束信号或仍有未闭合内容块时，EOF 会被记录为
502 并返回流内错误，不会补造成功事件或关闭未完成工具。已闭合内容块且已有结束原因、
仅缺少 `message_stop` 的情况单独处理。

能观测到的下游取消记为 499，并且仅结算已经收到的 usage；未知 token 或供应商费用不估造。
Console 未向 Crosslane 传播取消时，Crosslane 无法判断客户端已离开。自动路由只允许在
首段内容或工具开始之前切换。`failover_timeout_ms` 仍是自动路由每个来源各自的尝试期限，
`0` 只关闭该来源的尝试期限；固定来源不新增此期限，也不隐式设置整次请求的总期限。
候选检查和最终响应检查共享同一个五秒窗口，避免对首段 SSE 重复等待。
CRS 丢弃的 `Retry-After` 等响应头不能靠 Crosslane 增加同名头恢复。
被舍弃的自动路由候选仍按实际 provider、账号和模型累计已收到的用量；最终结果只计为
一次客户端请求，内部摘要请求仍独立记账。

### 工具历史与检查点

Claude Code 的延迟 `tool_addition` / `tool_removal` 声明在 `crs` 构建中按消息顺序投影为
当前 `tools[]`，既支持已有工具的引用，也支持新增 custom 工具定义。声明原位置改为文字
通知并保留缓存标记，原有工具调用 ID、工具参数、thinking 和签名不变。
明确移除之前的历史工具引用转为保留原始引用内容的文字记录；官方搜索结果保留调用配对及
仍可用的引用。已移除工具不会重新进入当前目录，移除后的新引用及未知引用仍明确拒绝。
生成与 count_tokens 使用相同处理。未知引用、冲突重定义和不支持的定义类型会明确拒绝，
有效目录仍须通过原有工具配对、容量与结构校验；未启用 `crs` 的行为不变。

最新的非空 assistant `compaction` 块重置工具变更历史，先以其中的 `tool_changes` 恢复
检查点工具状态，再应用后续变更；已经丢弃的旧声明和引用不会修改当前目录或拒绝续聊。
`content` 为 null 或缺失的压缩块仍视为无操作。这是一次式的旧协议目录投影，检查点原块
及其签名保持不变。显式 `tool_choice=none` 在禁用工具功能时仍保留已完成的工具配对，
以及解释历史所需的 schema 来源数据；这些声明不会激活 Tool Search、Web Search、
恢复待执行调用或发布可调用工具。生成与 token 计数使用相同的工具策略。

## Kiro 端点与上游拒绝

OAuth Runtime 生成、ControlPlane 模型目录和 extension 管理 REST 使用各自的协议。
管理请求默认访问 `management.{region}.kiro.dev`，Runtime 访问
`runtime.{region}.kiro.dev`。Headless API key 保留 CLI 兼容协议，不执行 OAuth Profile 发现。

| 覆盖项 | 作用 |
| --- | --- |
| `CROSSLANE_MANAGEMENT_URL` | 覆盖模型目录及 Profile、用量、订阅等管理请求的主机，支持 `{region}` |
| `CROSSLANE_AMAZONQ_URL` | 未设置 management 覆盖时，兼容旧 extension 管理地址 |
| `CROSSLANE_RUNTIME_URL` | 覆盖 Runtime，也供默认原生 MCP 使用 |
| `CROSSLANE_MCP_URL` 或 `upstream.web_search_endpoint` | 显式选择旧 JSON-RPC 搜索传输 |

默认网页搜索使用 Runtime 根路径的原生 `KiroRuntimeService.InvokeMCP`。需要使用该协议时，
两个显式搜索 URL 都应留空；在其中填写根 URL 仍会选择旧协议。缺失 Profile 的成功发现
结果按账号、token、区域隔离缓存 15 分钟，最多 500 项，失败不缓存。显式选择原生 OAuth
生成可配置：

```toml
[upstream]
preferred_endpoint = "runtime"
```

以上覆盖项用于代理账号请求，不改变 [Kiro 企业订阅管理](kiro-enterprise-admin.zh-CN.md)
按已验证 AWS 区域生成的管理端点。

### 拒绝与空回复

启用 `crs` 且自动选择端点时，符合条件的 Claude 5 OAuth 工具请求可使用原生 Runtime
取得终止元数据；显式端点、API key 认证及其他请求仍遵循各自策略。Kiro 的
`CONTENT_FILTERED` 元数据转换为 Messages 的 `stop_reason: "refusal"` 和
`stop_details`，上游报告时包含 `reasoning_extraction`。尚未提交的工具调用会取消，
已经收到的用量继续结算。Claude 5 工具调用会等待终止确认，即使设置了
`features.buffer_tool_calls=false`。

拒绝可能在工具执行前以 HTTP 200 返回。应检查最终流事件和拒绝详情，不能把 HTTP 200
或不完整工具参数当成成功。协议适配不能证明 Kiro 的分类原因，也不保证模型接受请求。
临时模型访问限制与单次任务拒绝不同；本地 `health` / `ready` 无法确认模型可用性或恢复时间。

普通 Kiro 响应没有任何助手输出时，会有界重试一次，仍为空则明确报错；流式请求不重放
已经展示的内容。启用 `crs` 时，Claude 的该次重试可在工具描述中澄清由模型返回结构化
调用、交给本地执行；工具名、schema、参数和会话正文保留，追加后超过 1024 UTF-8 字节
的描述保持原样。
