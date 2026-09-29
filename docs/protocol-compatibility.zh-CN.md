# 协议兼容与模型控制

[English](protocol-compatibility.md) | [简体中文](protocol-compatibility.zh-CN.md) | [项目说明 / Overview](../README.md)

本文描述当前源码的 Claude Messages 与 OpenAI Chat Completions 转换行为。Responses 的工具、状态和流式差异见 [Responses 与 Codex 接入](openai-responses.md)。兼容指可接入以下能力，不代表完整实现官方 API。

## Copilot 三协议转换

动态发现的所有非 embedding Copilot 生成模型均提供 Messages、Chat Completions 和 Responses。
调度先限定服务账号池和账号真实模型目录，优先原生接口，缺少目标协议时才转换。
公开 `protocols` 包含三种接口，`capabilities.native_protocols` 保留上游原生协议；内部逐账号
目录不被扩写，转换不会绕过模型权限。

转换覆盖完整历史、system/developer、函数工具与并行结果、图片/内联文件、文本/思考增量、
结束原因、usage/cache 和原始 `copilot_usage`。流式处理支持 UTF-8 分片、CRLF、多行 SSE、
稳定工具索引与尾部用量；错误或断流不会补发成功事件。转换路径限制单个 SSE 事件 8 MiB、
完整响应/累计流 64 MiB。
协议要求的初始零用量用 `kproxy_usage_source: unreported` 标记，收到真实 usage 后才
按 provider 用量记录，避免把“未报告”误记为“报告了零”。
拒绝回答会保留正文和 OpenAI refusal 字段。输出长度耗尽时保留工具参数片段和未完成的
结束原因，Responses 输出项也标记为 incomplete，不将正常截断误报成传输失败。
JSON Schema 在 Chat `response_format`、Responses `text.format` 和 Messages
`output_config.format` 之间转换，并保留同时指定的 thinking effort。转换到 Messages
时需要具体 schema；OpenAI `json_object` 模式应使用原生 OpenAI 接口或改为明确 schema。
最终 schema 约束仍取决于上游模型能力，字段依据
[Anthropic 结构化输出契约](https://platform.claude.com/docs/en/build-with-claude/structured-outputs)。

需要转换时不模拟上游托管能力：`previous_response_id`/`conversation` 应改成完整历史，
后台任务、托管工具和远端文件 ID 会明确报错；原生路径仍保留这些字段。
协议转换不为模型增加视觉/推理能力，也不保证结构化输出；不透明思考签名无法跨协议通用回放。
已知 Claude 家族按版本选择 manual/adaptive thinking，未知模型不强加 Claude 思考参数。
`count_tokens` 对原生 Messages 模型透传，其他模型返回带 `estimated: true` 和
`x-kproxy-token-count-source: estimate` 的本地估算，不计为上游生成用量。

本次独立 Rust 实现参考了高关注度项目
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI/tree/main/internal/translator) 的双向转换与流式生命周期、
[LiteLLM](https://github.com/BerriAI/litellm) 的协议/能力边界，以及
[copilot-api](https://github.com/ericc-ch/copilot-api) 的 Copilot Messages 适配。
思考模式依据 [Anthropic 文档](https://platform.claude.com/docs/en/build-with-claude/extended-thinking)。
测试使用 mock 上游，不代表真实 Copilot 席位与全部模型已逐一验收。

下文的 Kiro 专用转换规则不改变 Copilot 原生转发。

## Kiro 客户端版本与 2026 年 11 月停用通知

根据收到的 AWS Health 通知，2026-11-09 起，IDE 低于 `0.11.133`、CLI 低于
`1.28.2` 的客户端将无法连接 Kiro。此前代理的 IDE 标识 `0.7.45` 低于此门槛；
CLI 标识 `2.10.0` 已高于门槛，仍一起更新到核验过的稳定版本。

2026-09-14 核验的官方发布包基线如下，来源为 [IDE 下载页](https://kiro.dev/downloads/)、
[CLI stable manifest](https://prod.download.cli.kiro.dev/stable/latest/manifest.json) 和
[官方 changelog](https://kiro.dev/changelog/)：

| 路径 | 客户端版本 | SDK / 生成的服务客户端 |
| --- | --- | --- |
| IDE / CodeWhispererStreaming | `1.0.437` | `aws-sdk-js/1.0.39`、`codewhispererstreaming#1.0.39` |
| CLI / V2 生成 | `2.21.4` | `aws-sdk-rust/1.3.15`、`codewhispererstreaming/0.1.17975` |
| CLI / Management 模型目录 | `2.21.4` | `aws-sdk-rust/1.3.15`、`codewhispererruntime/0.1.17975` |

版本和 UA 统一在 `crates/kproxy-kiro/src/identity.rs` 定义。IDE 生成、模型、订阅、
额度、Profile 发现及 MCP Web Search 共用版本；Social Token 刷新也使用同一个
`KiroIDE-<version>-<machine_id>`。CLI 生成和 Management 分别使用对应的服务标识，
同时发送 `User-Agent` 和 `x-amz-user-agent`。保留原有 IDE Windows / CLI macOS
平台模板与账号 machine ID，这些模板不表示代理宿主机的操作系统。

这套基线对应当前已实现的 V2/CodeWhisperer 协议。新版客户端的 KiroRuntimeService、
V3/KAS 是其他请求路径，不能只更换 UA 中的 service 名称就完成迁移。

部署时必须重新构建并替换 `kproxyd` 二进制或镜像，再重启服务；配置热加载不会更新
编译进二进制的版本。启动日志 `Kiro upstream client identity` 应显示
`ide_version=1.0.437`、`cli_version=2.21.4`。升级后按
[启动与排障](startup-and-debugging.zh-CN.md) 核对 `ready`、一次真实生成及适用的 Token
刷新，并按 [官方防火墙文档](https://kiro.dev/docs/privacy-and-security/firewalls/)
检查上游域名可达性。回归测试校验最低版本、实际发出的请求头、回退请求和凭证轮换；
本地测试不等同于 AWS 停用日期后的服务端验收。

## 兼容性维护原则

| 类别 | 处理原则 |
| --- | --- |
| 已实现控制 | 校验读取的字段，并按实际模型元数据映射；例如 effort、采样和工具选择。 |
| 缓存控制 | `cache_control` 按 Claude 的 `ephemeral` / TTL 定义校验；未知类型不作为兼容提示忽略。 |
| 兼容提示 | 接收但忽略不参与执行的附加字段；format/strict 不产生结构化输出保证。 |
| 缺少执行/数据链路 | 对请求启用的未实现能力明确拒绝；例如 Responses background 和 Files API 解析。 |
| 历史中的不可转换条目 | 按 Responses 的跳过规则处理，不等于支持调用对应托管工具；必须仍有有效会话内容。 |
| 资源与访问边界 | 保留认证、服务白名单、模型标识、工具配对、附件/Schema/上下文上限。 |

新增拒绝条件前，核对本文固定参考的对应路径，说明是转换所需、已实测的上游限制还是资源/安全边界。
除明确按协议校验的缓存控制外，不能仅因无原生映射或不在官方枚举中就拒绝此前可用的附加提示，
也不照搬实测会让上游失败的映射。
已知忽略字段只记录 `proxy.compatibility.controls_ignored` 的固定字段名，不记录 Schema 或请求值。

模型标识限制为 256 字节、禁止空白/控制字符；继续接受 Unicode、别名、`/`、`:` 和 `[1m]` 后缀。
非法值在选账号前返回 400，日志仅保留 `[invalid-model]`，不截取原输入。此处是资源边界，不是模型枚举。
保留非 null 的真实 format Schema/task budget 回归，覆盖 Messages 别名、counting、JSON/SSE，
并断言它们不进入 Kiro payload；不能用 null 绕开兼容性验证。

## 模型发现响应协议

OpenAI 和 Anthropic 在各自 API 域名下都使用 `GET /v1/models`，因此代理复用该路径并提供
`/models` 别名，再根据已认证客户端的请求头选择响应协议。Codex
获得 [OpenAI 模型列表格式](https://developers.openai.com/api/reference/resources/models/methods/list)：
顶层包含 `object: "list"`，模型项包含 `id`、`object`、`created`、`owned_by`，不混入
Anthropic 专属字段。

Claude Code 获得 [Anthropic 模型列表格式](https://platform.claude.com/docs/en/api/models/list)。
支持 `limit`（默认 20，范围 1–1000）、`after_id` 和 `before_id`，响应包含 `data`、
`first_id`、`has_more`、`last_id`。每个模型包含 `type`、`id`、`created_at`、
`display_name`、`capabilities`、`max_input_tokens`、`max_tokens`；不返回 OpenAI 字段和
Kiro 的 description 扩展。Kiro 模型发现不提供发布日期和 Anthropic 能力矩阵，因此
`created_at` 使用协议明确允许的 Unix epoch，`capabilities` 为 `null`。动态发现返回的
`tokenLimits.maxInputTokens`、`maxOutputTokens` 分别映射到两个 Anthropic 上限字段；静态或
配置回退模型返回 `null`，不虚构上限。游标使用客户端实际看到的模型 ID，包括非 Claude
Kiro 模型使用的 `anthropic.` 前缀。动态发现保留 Kiro 返回顺序，不再改成字母序；回退
模型保留人工维护的目录顺序，因为 Kiro 不提供发布日期。

如果 Codex/Claude Code User-Agent 和 `anthropic-version` 都无法识别协议，通过鉴权的请求
会获得联合响应：同时返回 OpenAI 的 `object`、模型 `object`、`created`、`owned_by`，以及
Anthropic 的分页和模型元数据字段。联合响应使用 Claude 兼容的发现 ID，保证非 Claude 的
Kiro 模型仍可见；OpenAI 生成接口的模型解析也接受该 ID。联合响应默认返回完整列表，显式
提供 `limit`、`after_id`、`before_id` 时才分页。API Key 鉴权仍优先执行。模型发现只使用
User-Agent 协商响应结构，不会因缺失或无法识别的 User-Agent 被客户端准入检查拒绝；生成
接口仍遵循原有准入策略。

这里的兼容范围仅包括列表；OpenAI/Anthropic 独立的模型详情接口
`GET /v1/models/{model_id}` 尚未实现。

## 工具调用参数

上游原生工具调用（包括 MCP 工具）在 Claude/OpenAI、流式/非流式、开启/关闭缓冲时，
使用同一套 JSON 校验规则，不根据工具名称推断读写操作。空参数统一为 `{}`；参数是合法
JSON 时，即使上游未发送工具 stop 事件也可正常返回。非空但损坏的 JSON 会明确报错，
不会通过猜测缺失参数来补全调用。

## 协议字段归一化

Claude `count_tokens` 接受不带输出预算的 enabled thinking，不再拿 `budget_tokens`
与临时补入的生成上限比较；生成请求仍校验预算。省略 `tool_result.content` 按空结果处理，
`document.citations: null` 等同省略，空文档标题使用中性名称。客户端 `search_result` 及
工具结果内的同类块保留 source、title 和正文，作为带标识的来源数据；不伪造 Anthropic 引用索引。

Chat 接受可空的 `stream`、`tools`、`tool_calls`、`stream_options.include_usage` 和 function `strict`。
assistant refusal 内容块及 Chat 的 `refusal` 字段会保留为历史文本。旧版 `functions`、
`function_call`、`role: function` 转成配对的 Kiro 工具调用；仅使用旧接口声明的请求，响应
和流式 delta 仍使用 `function_call` 及对应结束原因。工具描述缺省或空白时补中性默认值：
真实 Kiro 会接收空描述声明，却在回放其工具调用时拒绝请求。
现代工具列表为 `null` 或空数组时，不覆盖旧版函数控制，也不改变旧版响应格式。

Chat `stop` 复用增量停止序列过滤，覆盖跨流块匹配。`n`（1–128）串行执行独立 Kiro 请求，
返回全部带索引的候选并汇总 usage；流式按需只发一个最终 usage 块及一个 `[DONE]`。
每个候选都执行正常的准入、认证和额度核算；客户端取消流后不继续发起后续候选。
下一候选会等待上一轮计费及并发名额释放；流中保留的请求正文继续占用共享内存配额。
SSE 开始后的候选失败也会记录，并正确区分 HTTP 200 和上游错误状态。

OpenAI Chat 的 `file` 和 Responses 的 `input_file`（含工具输出）支持内联 base64、
data URL 和公共文件 URL，转为 Kiro 文档并复用 Claude 的媒体、大小和 SSRF 检查。
外部托管的 `file_id` 仍需要当前代理没有提供的 Files 服务。
文件名或 MIME 不能确定格式时，可通过 PDF 标识或 UTF-8 文本推断内联内容的类型，
因此无扩展名文件也能转换为 Kiro 支持的文档。

图片数量上限改为 100，字节数和内存限制不变；真实 Kiro 已接收 100 张有效小 PNG。
6 个小文本文件仍触发 Kiro 400，因此保留 5 个文档的限制。Kiro 接收零 token 设置后仍生成
文本，所以 `max_tokens: 0` 缓存预热仍未实现。后台任务、Conversations API 状态、Responses
context-management 执行和托管工具、精确 prefill、logprobs 与严格 JSON Schema 保证属于
单独的能力，不能靠放宽字段校验实现。Claude Web Search 的空域名列表按无过滤处理；非空
过滤仍需适配搜索执行器。

## Claude Code MCP Tool Search

当 `ANTHROPIC_BASE_URL` 指向第三方代理时，Claude Code 默认会关闭 Tool Search，并在请求中
一次性加载全部 MCP schema。MCP 工具较多时，应显式启用：

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:5580 ENABLE_TOOL_SEARCH=auto claude
```

`kiro-proxy` 现已兼容 Anthropic 的 `defer_loading`、regex/BM25 Tool Search 和
`tool_reference` 历史块。由于 Kiro 没有原生 Tool Search server tool，代理会在本地执行搜索，
并在同一个响应中继续生成。官方 Tool Search 输入包含 `pattern` 或 `query`，并支持可选
`limit`（1–10000，默认 5）。代理会先遵守请求的 limit，再按剩余工具数、tool token、上下文和
payload 字节预算动态装载，因此不存在固定 5 个的工作集上限；未命中的 deferred schema 不会进入
Kiro 上下文或上游 payload。Catalog 构建和搜索运行在 blocking worker，不会阻塞 HTTP runtime。

自动生成的 `[context]` 配置还通过 `max_loaded_tools` 限制已加载工作集（默认值及代理上限
均为 512）、通过 `max_tool_input_tokens` 限制 deferred Tool Search 工作集的估算 token，
并通过 `max_upstream_payload_bytes` 限制序列化后的 Kiro 请求大小。未启用 Tool Search 的普通请求
不会再被这个 32k 工作集预算误拦截，其工具定义仍会计入模型总输入 token，并接受上下文窗口、
工具数量及 payload 字节限制。真实超限请求会在本地拒绝，而不是留给上游返回不透明错误。
`413/request_too_large` 仅用于真实入站请求体超过 50 MiB；工具、上下文及转换后 payload 的
语义预算错误使用 400，以免 Claude Code 将其误显示成 32MB 附件错误。

## 自动压缩与窗口

Claude Messages 默认开启 `context.auto_compact_on_overflow`。代理先选定账号、完成条件映射、加权选择、
别名及默认模型解析，再按实际模型的安全窗口决定压缩或拒绝请求；无需压缩时复用这次选择。
如果上游仍返回 `prompt is too long`/`context length exceeded`，代理会按保守窗口
重新压缩并只重试一次。摘要请求使用完整源材料；输入装不下时，先按 UTF-8/token 边界无损分段。
自动溢出压缩只有在全部分段能于同一并发波次启动时才调用语义摘要，并按时间顺序组合 checkpoint；
超过单波并发能力时立即使用本地 extractive fallback，避免多个近窗口上限的分段在一次总超时内排队、
超时后丢弃全部已完成结果。显式客户端 compact 仍执行完整语义分段摘要。
分段前不做有损摘录，每段都校验输入窗口。
摘要前释放账号并发名额，避免单并发账号阻塞自己的摘要请求；摘要后重新调度时若模型窗口更小，
同一份压缩产物最多重新应用一次。OpenAI Chat Completions、使用 `truncation: disabled` 的 Responses
以及 Tool Search 已开始输出后的上下文增长仍返回明确的上下文错误，因为这些路径无法安全回传位于
Claude 响应首部的 `compaction` 边界。摘要超时会立即释放主请求；后台仅在有界宽限期内继续结算，
到期后主动取消摘要流，并结算此前已经解码的 usage。

摘要等待默认 120 秒（`context.compaction_summary_timeout_ms`）；升级会保留已有显式值，原先的
`30000`、`60000` 或 `150000` 不会自动改写。Claude Code 流式压缩使用完整 start 块以兼容忽略 compaction delta 的客户端；
普通 SDK 仍使用标准 delta。超时回退只记录一条 WARN，后台结算为带 trace ID 的 INFO，
`credits_source=estimated` 表示本地估算，并非上游实际扣费证明。日志卷按日持久化，重建容器不会清除旧 WARN。

### 摘要资源与窗口

一次主请求最多启动一次语义摘要操作，最多 16 段、并发最多 2，遵循账号并发及一次总超时；自动溢出压缩需要
超过一个并发波次时不会启动上游摘要，显式客户端 compact 不受这条快速降级规则影响。
任何一段失败都不能冒充完整摘要成功。一次上下文重规划/重试复用 `CompactionArtifact`，
可调整 checkpoint 和近期保留轮次，不再次启动摘要。主生成遵循实际映射模型，源名称或 `[1m]`
后缀不会扩大目标模型容量。压缩目标通常预留安全窗口的 25% 余量；固定部分超过目标但低于
硬上限时可以放宽目标，超过硬上限则拒绝。输入与输出遵循各自模型上限，不假设共享窗口。

配置默认值来自 [ContextConfig](../crates/kproxy-core/src/config.rs)：

```toml
[context]
max_input_tokens = 200000
safe_input_ratio = 0.95
compact_safe_input_ratio = 0.99
auto_compact_on_overflow = true
max_tool_input_tokens = 32000
max_loaded_tools = 512
max_upstream_payload_bytes = 8388608
compaction_summary_model = ""
compaction_summary_timeout_ms = 120000
compaction_preserve_recent_turns = 3
```

`max_input_tokens` 是缺少元数据时的基线，不是所有模型的固定容量。摘要模型留空时，初次压缩
复用已解析模型；显式模型必须可调度。近期完整 user/assistant 轮数默认 3、上限 64，仍受窗口限制。
`count_tokens` 应用已有边界和编辑，不生成摘要。Prompt Cache 按最终 payload 计算；
Claude cache TTL 不控制 Kiro 缓存期限，压缩不保证命中缓存。

### Claude Code 的手动 `/compact`

Claude Code 可能通过普通 Messages 请求追加摘要指令，而不是发送 `compact_*` edit。
相邻 user 消息合并后，超大工具结果可能与摘要指令落在同一个 `currentMessage`，
从而被普通生成路径视为不可压缩的当前轮。

当前兼容分支只对带 Claude Code User-Agent、通过请求校验且末尾独立 text block 匹配已知摘要
指令的请求生效（含 text-only 变体）：

- 把摘要指令之前的完整源 blocks 还原成历史，摘要指令独立成为当前轮。
- 使用精简摘要 system，将原 system、system 消息、已加载工具的完整定义、Schema 和示例
  转为带标签的完整 JSON 源记录，纳入可压缩材料。
- 历史工具调用与结果只作为材料，不继续执行 Web Search/Tool Search，不载入可执行工具定义。
  未发现的 deferred 工具目录仍留在模型上下文之外。
- 复用有界分段摘要，随后返回客户端请求的普通摘要文本。

普通消息或工具结果中引用相似指令不会启动该模式；其他 SDK 和指定 `tool_choice: any/tool`
的请求保留原边界。明确的客户端摘要可在 `auto_compact_on_overflow = false` 时使用这条路径。

`count_tokens` 使用相同准备和渲染逻辑，计入完整源材料，但不发起摘要。
一次 `/compact` 成功不会缩小客户端下一轮重新发送的原 system/工具定义；普通请求的固定部分
仍超限时，需要精简客户端配置或选择实际具有更大窗口的模型。

### 普通生成的保留边界

普通生成始终保护 system 前缀、当前轮、工具 Schema 和 tool use/result 配对。
压缩不能靠删除系统约束让请求通过。固定部分超限时，`error.context` 和日志中的
`diagnostics.context_overflow` 返回同一份数值明细：总量、受保护前缀、工具、当前消息、
可压缩历史、结构开销、不可压缩最小量及模型上限。CLI 日志显示 `context_tokens`。

`protected_prefix_tokens` 包括 system 和翻译层移入前缀的长工具文档；当前消息估算包括附件与
工具结果，但不重复计入工具定义。这些都是本地 token 估算，诊断中不回显源正文。

### 流式与下轮回放

`compaction` 必须是响应首个内容块。代理先暂存前导块，待上游轮次可提交后再输出，
保留在响应开始前切号、刷新认证和重试的机会。

已有针对 Claude Code **2.1.260** 的客户端回归记录：该版本只保留 start 中的摘要，忽略
`compaction_delta`。当前编码区分以下两种累积方式：

| 客户端 | 输出方式 |
| --- | --- |
| 识别为 Claude Code | start 携带完整 checkpoint，随后 stop。 |
| 普通 SDK | null start、一个携带完整 checkpoint 的 delta、stop。 |

同一响应不能在 start 和 delta 重复发送摘要，也不能用空 delta 覆盖已有摘要。
语义路径和 extractive fallback 都要验证下一轮回传。该版本记录不等于已验证所有未来客户端；
客户端升级应重跑真实客户端测试。

代理还保留最多 256 个、有效期 1 小时的进程内 checkpoint，仅保存 checkpoint、源消息数和源消息前缀的 SHA-256 指纹，
不保存原请求正文。若同一 service/凭据隔离后的会话下一轮没有带回 `compaction`，但完整消息前缀与
已压缩源逐字节哈希一致，代理会在原 assistant 边界重放 checkpoint，再走同一套标准边界归一化。
新追加的 assistant/user 轮次不会被删除；前缀不一致、客户端已带回有效边界、进程重启或缓存过期时均不重放。
该机制避免旧历史再次进入 Kiro 或重复触发摘要，但无法阻止不兼容客户端继续把大请求体传到代理。
`proxy.request.validated` 会记录 `claude_code_client`、`client_compact_edit`、
`client_compaction_boundary_applied` 和 `server_compaction_replayed`，用于区分客户端触发、客户端回传与代理补偿。

### 降级、超时与记账

摘要失败、超时、额度不足、空/非法摘要或无法达到窗口目标时，先恢复原 payload，再尝试
`compact_kiro_payload` 的抽取式降级。降级会丢失细节，最终仍装不下时返回上下文错误；
不能承诺失败后一定成功。日志用 `compaction_mode=semantic/extractive_fallback` 区分质量。

超时立即结束主请求对摘要的等待，停止派发新段。已被上游接受的请求只在有界后台宽限期内
继续结算；宽限到期主动取消流，并结算已解码的 usage。本地清理另有硬上限。
超时回退记录一条 WARN，后续结算以同一 trace ID 记录 INFO。

摘要请求独立使用账号池、额度 reservation、meter 和 `/internal/compact` 统计。
Claude 顶层 `input_tokens`/`output_tokens` 表示主生成；`usage.iterations` 区分摘要与主采样。
`credits_source=server` 和 `estimated` 区分上游用量与本地估算；估算不是实际扣费凭据。
完全没有输出或 usage 的失败摘要不会仅按输入估算消耗 credits，空/非法摘要也按失败计数。
摘要成功后主生成仍可能因额度不足失败，两次上游操作不能原子结算。

跨段滚动归并尚未实现；新的有效压缩会替换同一会话的旧 checkpoint，二次摘要仍可能损失信息。
实现见[请求准备](../crates/kproxyd/src/http/handlers/request.rs)、
[摘要执行](../crates/kproxyd/src/http/handlers/compaction.rs)、
[摘要规划](../crates/kproxy-translate/src/tokenizer/compaction.rs)和
[客户端识别](../crates/kproxy-translate/src/context/claude_code.rs)。
回归命令及默认忽略的真实客户端测试见[贡献指南](../CONTRIBUTING.md#validation)。

## 工具检索的续轮限制

`features.tool_search_max_rounds` 默认 4、硬上限 8；单次请求达到内部轮次上限时返回 Claude
`pause_turn` 续轮状态，不再把合法的 server call 转换成 HTTP 5xx。
`features.tool_search_max_operations` 默认 32（有效范围 1–256），统一限制历史待续调用及所有内部
轮次的搜索操作总数；超出的调用会收到响应内 `unavailable` Tool Search 结果。将
`features.enable_tool_search=false` 作为回滚开关时，代理会明确拒绝原生 Tool Search 请求，
不会把 deferred 工具重新全量塞给上游。持久化请求日志会保存 Catalog/Working Set 大小、搜索
limit 与预算截断、Client/Upstream Status 和稳定错误码。错误响应保持原有 Claude/OpenAI body，
并通过 `request-id`、`x-kproxy-error-code`、`x-kproxy-error-stage`、
`x-kproxy-upstream-status`、`x-kproxy-account-error` 响应头提供诊断信息。

## Web Search

Claude 原生 `web_search` server tool 会先交给 Kiro 模型决定是否搜索及搜索词，再由代理调用
Kiro 的 `/mcp` JSON-RPC `web_search`，把真实结果作为工具结果续回同一模型轮次。代理不会把
首条用户消息直接当查询，也不会把原始搜索摘要伪装成模型最终回答。流式和非流式 Claude 响应
均使用 `server_tool_use` / `web_search_tool_result`；搜索错误作为 200 响应内的工具错误返回，
不会冷却或封禁账号。并行搜索不会被丢弃；当同一轮同时包含 server tool 和 client tool 时，
server call 会保持 pending，等客户端回传 client tool 结果后再按官方续轮协议补结果。搜索结果
携带代理自有的 AES-256-GCM opaque 内容，后续轮次可恢复 snippet，任何篡改都会在进入模型上下文
前被拒绝；Anthropic 自有 opaque 值仍可带回，但代理不会尝试解密。只有最终文本确实包含结果的
完整 URL 时才会输出结构化 `web_search_result_location` citation。代理安全上限默认 20 次；显式 `max_uses` 超过配置值时会
明确拒绝，不再静默截断。Claude Web Fetch 在兼容的服务端执行器完成前会被明确拒绝。

默认 MCP 地址是 `https://runtime.{region}.kiro.dev/mcp`。可通过
`upstream.web_search_endpoint`（支持 `{region}`）或临时环境变量 `KPROXY_MCP_URL` 覆盖；
`upstream.web_search_timeout_ms` 默认为 60000。每个 MCP 请求都会携带 Kiro 必需的
`x-amzn-kiro-profile-arn` 请求头。导入账号缺少 profile ARN 时，代理会通过
`ListAvailableProfiles` 自动发现，并发请求会按同一 token 合并，发现结果会写回账号文件；
本文覆盖企业 SSO 与 headless API key 的受支持接入方式；底层兼容分支不代表实现个人/社交 OAuth 登录。domain/location 过滤和
code-execution caller 仍需兼容执行器支持；strict 和 eager streaming 作为兼容提示接收并忽略。代理生成
的加密字段明确属于 kproxy 自有格式，不宣称与 Anthropic 托管搜索的 ciphertext 互通。

## 文档、上下文编辑与兼容边界

Claude `document` 支持 `base64`、`text`、HTTP(S) `url` 和 `content` 来源，也支持工具结果中的
文档。自定义 `content` 文档按原顺序转为文本，内嵌图片提升到同一条 Kiro 消息的图片列表，并保留
图片序号标记；这不保留 Anthropic 的自定义引用分块语义。每个请求最多 5 个文档（每个解码后
4,500,000 字节）和 100 张图片（每张 5 MiB）。URL 附件只访问公共地址，每次重定向重新检查 DNS，
禁用环境代理，并验证实际文件签名与媒体类型。Kiro 的引用、网页来源和许可证信息会显示为
References，不会把回答位置伪装成 Claude 原始文档的字符/页码/块索引。

`clear_tool_uses` 支持 trigger、keep、clear_at_least、exclude_tools，以及布尔或工具名列表形式的
clear_tool_inputs；`clear_thinking` 支持保留指定轮次或全部 thinking。生成响应会报告实际执行的
`context_management.applied_edits`；清理触发与清除 token 数采用本地估算，`count_tokens` 返回
编辑前后输入估算。生成路径会先清理已无需保留的历史，再获取仍然需要的远程附件。

兼容性以 jwadow/kiro-gateway、hj01857655/kiro-account-manager、chaogei/Kiro-account-manager
的实际接入行为为基线，不要求完整复刻 Claude/OpenAI 官方语义。请求、消息、工具的附加字段，以及
工具 strict 和 Claude/OpenAI format 提示继续宽松接收；接收格式提示不代表提供原生结构化输出保证。
固定源码版本见[参考与回归](#固定参考与回归)，接收和拒绝规则见[兼容性维护原则](#兼容性维护原则)。

相邻同角色消息会合并。assistant prefill、`max_tokens=0` 缓存预热和 Anthropic Files API 的
`file_id` 来源尚未接入对应的生成/数据获取链路，仍会拒绝。
协议兼容在首次发送前确定，不再缓存字段拒绝结果、定时过期重探测或通过逐项删字段重试。
缓存标记只发送 `type: default`，Claude 的缓存 TTL 不控制 Kiro 缓存有效期。
所有支持的请求、消息、工具和内容块位置都将省略或 `cache_control: null` 视为未设置。
缓存对象遵循 [Claude 官方定义](https://platform.claude.com/docs/en/api/messages/create)：
`type` 必须为 `ephemeral`；`ttl` 可省略（默认 `5m`），指定时只能为 `5m` 或 `1h`。
缺失、空字符串、`null`、非字符串或未知的内层 `type`，以及非法 `ttl` 都返回 400；
不会将未知类型静默忽略或改写。附加字段不传给 Kiro，也不表示代理实现了其语义。
Chat Completions 的缓存扩展字段复用这套对象校验；消息级 `cache_control` 仍是代理扩展。

Claude Messages 与 token 计数别名还按[官方缓存规则](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
检查断点：总数最多四个，按 `tools → system → messages` 顺序，`1h` 必须位于 `5m` 前面。
自动缓存寻找末个可缓存块；已有同 TTL 显式标记时合并，不同 TTL 返回 400；有目标但四个显式
断点已占满时拒绝自动缓存。没有可缓存目标时跳过自动断点。不能直接标记 thinking、
redacted_thinking 或空文本块；工具结果、内容文档和搜索结果内的嵌套缓存对象也会校验。
这些入参检查不保证 Kiro 提供与 Claude 相同的缓存有效期、命中位置或计费行为。
Claude 历史 thinking 不回传
到 Kiro 请求历史，但不关闭当前生成的 thinking；Responses 的明文推理摘要保留在 assistant 历史中。
文档 context 则保留为独立、带 JSON 标识的消息文本。
模型控制参数采用 [chaogei/Kiro-account-manager](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/translator.ts) 的显式映射方式，但不沿用其缺失元数据时猜测开启 thinking 的回退：

| 客户端参数 | Kiro / 代理处理 |
| --- | --- |
| `max_tokens`、`temperature`、`top_p` | 映射为 `inferenceConfig.maxTokens/temperature/topP`，保留显式的零采样值；OpenAI 两个输出上限均未传时不补 8192，也不发送 `maxTokens`，交给 Kiro 默认行为；仍可能受到具体模型的参数限制。 |
| OpenAI `max_completion_tokens` | 映射为 `inferenceConfig.maxTokens`，优先于 `max_tokens`；同一有效上限也用于内部续写预算、额度预估和结束原因。 |
| Claude `top_k` | 接收但不发送，不因模型 schema 而开启；记录 debug 诊断。这是网关兼容策略，不代表断言 Kiro 全局不支持。 |
| Claude `stop_sequences` / Chat `stop` | 在流式 / 非流式响应中本地执行，不发送原生 `stopSequences`；不保证上游生成量或费用也因此受限。 |
| thinking / effort | 有可识别的 effort 元数据时使用 `thinking: adaptive` + `output_config.effort`，或 `reasoning.effort`；元数据缺失、不完整或不可识别时，完全省略 `additionalModelRequestFields`，不发送 `{}`、`null` 或猜测的 adaptive thinking。 |
| Claude `output_config.effort` | 显式 effort 优先于 thinking budget，按实际模型元数据映射；system 消息中的 effort 从下一条 user 消息开始生效，压缩和内部续写后保留。映射为 Kiro 的请求级 effort，不保证 Anthropic 的逐消息缓存语义。 |
| Claude `output_config.format` / `task_budget` | 接收但不发送给 Kiro，debug 诊断仅记录字段名；不提供 JSON/Schema 或 task budget 保证，`output_config.effort` 仍独立映射。 |
| OpenAI `response_format` / Responses `text.format` | 接收但不放入 Kiro 输入，沿用参考项目的宽松行为；不新增 JSON/Schema 保证或基于 Schema 的生成重试。 |
| 工具 `strict`、Claude `eager_input_streaming` | 接收为提示，保留正常 Kiro 工具 schema 和既有流式行为。 |
| 服务等级、附加字段、未使用的流式提示 | 接收但不猜测为 Kiro 字段发送；实际使用的 `include_usage` 等值仍校验类型。 |
| OpenAI `reasoning_effort` | `none` 走已有的 disabled thinking 路径；其他值优先于 `thinking.budget_tokens`；均未提供时默认 high。档位不支持时取模型枚举最后一项，不排序、不做最近档位匹配。 |
| `thinking.display` | output_config 路径固定发送 summarized；reasoning 路径不发 display；没有可识别的元数据时省略整个扩展字段。客户端 display 不覆盖这些上游格式。 |
| `thinking.budget_tokens` | 原始预算直接映射为 low（≤4000）、medium（≤16000）、high（≤64000）、xhigh，不是上游独立 thinking token 硬上限。 |
| `thinking: disabled` | 不发送 thinking 控制字段，并过滤返回的思考内容；省略字段不保证默认开启思考的模型在内部停止思考。 |

默认 effort 按参考项目的运行时提取方式固定为 `high`，不读取 JSON Schema 的 default。
工具结果 / 控制续写轮次不再自动关闭 thinking。旧配置 `adaptive_thinking` 和
`max_thinking_budget_tokens` 均仅保留读取兼容，不再影响生成参数；显式配置的操作员
`model_thinking_mode` 禁用规则仍然生效；允许规则不能创造上游未提供的参数能力。

无法应用的 thinking 控制降级为上游模型默认行为，请求仍正常执行，但不保证客户端要求的 thinking
模式或预算生效。debug 决策原因为 `ModelControlsUnavailable`，它表示参数能力不可用，不代表
底层模型无法推理。AmazonQ Haiku 4.5 实测连空扩展对象也会拒绝，省略整个字段则成功。
该降级不依赖模型名称黑名单，不注入提示词模拟 thinking，也不通过删字段重试试探能力。

对齐范围是出站生成参数；保留请求校验、本地停止词过滤、内部续写预算和响应保护。
`display: omitted` 仍在本地隐藏返回的思考文本、保留原生签名，但不改变参考项目的上游 display 策略。
原始参数保存在不序列化的内部元数据中，每次按实际模型重新转换，覆盖 HTTP / 流内模型回退和内部续写。
OpenAI 两个输出上限均未传时，8192 仅用于额度预估，不限制内部续写，也不会据此返回 `length`；
显式输出上限仍然生效，不向上游补默认 `maxTokens`。
普通参数校验和签名错误直接返回，不推导或试探协议能力；现有鉴权、临时故障和上下文溢出处理不变。

## 客户端环境与工具恢复

Claude Code system 中的 `<env>` 会转换为 Kiro `envState`（工作目录、操作系统），并在内部
续写中保留，不使用代理宿主机环境替代。XML 工具调用恢复仅处理行首、代码围栏/缩进代码及
thinking 标签之外、属于本轮已声明工具的完整合法调用；行内示例、未知工具及畸形 XML
保留为普通文本。恢复的字符串参数保留 Schema 声明的类型和首尾空白，支持本地引用与组合
Schema；无法解析或超过遍历预算时保留原 XML，不等同于完整 Schema 校验。

## 固定参考与回归

以下是历史实现依据，不代表已核查最新版本；当前行为以本项目源码和回归测试为准。

| Reference | Pinned snapshot |
| --- | --- |
| jwadow/kiro-gateway | [Claude models · a5292ca](https://github.com/jwadow/kiro-gateway/blob/a5292ca04c7c6231e0b47673ac3f981f5a706e1e/kiro/models_anthropic.py) |
| hj01857655/kiro-account-manager | [Gateway models · c5c4776](https://github.com/hj01857655/kiro-account-manager/blob/c5c477647f8cba4c9b9f07e8fb41e403672adf36/src-tauri/src/gateway/models.rs) |
| chaogei/Kiro-account-manager | [Translator · 447adcd](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/translator.ts) |
| d-kuro/kirocc | [Runtime / effort · e0850b1](https://github.com/d-kuro/kirocc/tree/e0850b11e1a61cbb84aa55739a41b21d4a5c5fcf) |
| ZyphrZero/kiro.rs | [Tool recovery · f357292](https://github.com/ZyphrZero/kiro.rs/tree/f3572929fbc2c0c090c29b13a7c285d1b2777dcd) |
| pinealctx/kiro-gateway | [Model aliases · 9f614d4](https://github.com/pinealctx/kiro-gateway/tree/9f614d4b270e8e80e13c7d3603cbb1b090f327c4) |

```bash
cargo test -p kproxy-translate --test compatibility_controls --locked
cargo test -p kproxy-translate --test claude_gateway_controls --locked
cargo test -p kproxyd --test end_to_end compatibility_controls:: --locked
cargo test -p kproxyd --test end_to_end claude_gateway:: --locked
```
