# 请求处理

[English](request-processing.md) | [简体中文](request-processing.zh-CN.md) | [项目说明](../README.zh-CN.md)

本文介绍缓存用量、工具加载、请求延迟、Kiro 文本脱敏和上下文压缩。部署与账号配置见
[运维指南](startup-and-debugging.zh-CN.md)，客户端协议适配见[客户端兼容](client-compatibility.zh-CN.md)。

[缓存与用量](#prompt-cache-与用量) · [工具加载与延迟](#请求延迟控制) · [文本脱敏](#kiro-请求内容脱敏) · [上下文压缩](#上下文与自动压缩)

## Prompt cache 与用量

`features.enable_prompt_cache` 默认是 `false`。运行
`crosslane config edit features`，将其改为 `true` 后保存，CLI 会校验并热重载。
Kiro 通常只报告 credits，不报告 token 和缓存计数，因此 Crosslane 使用与输入、输出
统计相同的 tokenizer 在本地模拟缓存用量。这些估算不代表 Kiro 后端真实命中或 credits
折扣；上游实际报告的 credits 仍单独结算。

Claude 请求需在内容块或请求根部提供 `cache_control: {"type":"ephemeral"}`。
OpenAI Chat/Responses 会进行隐式前缀核算，无需这个 Anthropic 扩展；有效的显式
标记也会转换为 Kiro `cachePoint`。本地记录按上游账号、实际模型及可选的 OpenAI
`prompt_cache_key` 隔离，默认保留 5 分钟，显式 `ttl=1h` 则保留 1 小时，读取时续期；
daemon 重启会清空记录。这是本地策略，不保证上游保留时长。

成功完成的首次请求计为缓存创建。后续请求在断点前最多回看 20 个块，读取最长的
已记录前缀，仅将新增部分计为创建。缓存标记移动不会破坏已有前缀；前置工具定义、
角色或内容变化则会影响匹配。失败、取消以及未产生输出的拒绝不会建立缓存记录。
本地最小前缀长度为：Opus 5/5.5、Sonnet/Haiku 5.5 为 512 token；Opus 4.7 为
2,048；Opus 4.5/4.6、Haiku 4.5 为 4,096；其他模型为 1,024。这些是 Crosslane 的
本地核算规则。上游如有明确缓存计数（包括 0），优先保留上游值。

Claude Code 和 `/v1/messages` 的 `cache_read_input_tokens`、
`cache_creation_input_tokens` 都是输入的一部分，`input_tokens` 表示剩余未缓存输入，
三者相加才是内部记录的完整输入。`output_tokens` 是生成输出，不是缓存写入量。
流式 `message_start` 包含初始本地缓存计划，最终 `message_delta.usage` 与非流式
`usage` 返回完成后的核算结果。请求诊断和 `proxy.usage.accounting` 日志会将 token、
缓存来源标记为 `estimated`，与实际额度的 `credits_source=server` 分开记录。

OpenAI Chat Completions 在 `usage.prompt_tokens_details` 中返回 `cached_tokens`（读）
和 `cache_write_tokens`（写）；Responses 在 `usage.input_tokens_details` 中返回同样两个字段。
两者都已包含在输入总量中。Chat 流式请求需设置
`stream_options: {"include_usage": true}` 才会收到最终用量块；Responses 流式响应
在终止事件中返回用量，参见 [OpenAI 官方缓存用量说明](https://developers.openai.com/api/docs/guides/prompt-caching#monitor-cache-performance)。

Kiro 路径下，两个 OpenAI 入口都支持使用 `prompt_cache_key` 提供稳定的亲和提示，
该值会哈希为上游 conversation ID，并隔离本地缓存核算，但不会固定账号。Kiro 的缓存
断点仍通过上述 `cache_control` 扩展转换，不承诺 OpenAI 的保留时长或断点 TTL。
Kiro 对显式 `prompt_cache_breakpoint`，以及要求预热、最短 `ttl` 或 `mode=explicit`
的 `prompt_cache_options`，会在生成前返回 HTTP 400，不会把预热悄悄降级为普通生成。
空值、默认控制、`prewarm=false` 和旧版 `prompt_cache_retention` 提示仍可接受，
但不承诺旧版提示中的保留时长。
原生 OpenAI Chat 与 Responses 上游之间转换协议时，会保留
目标协议能表达的 `prompt_cache_options`、`prompt_cache_retention` 及内容块的
`prompt_cache_breakpoint`。预热和 12 小时最短保留时长要求原生 Responses；
目标仅支持 Chat 或 Messages 时明确拒绝。

即使 Chat 客户端省略或关闭 `stream_options.include_usage`，Copilot 仍会向上游请求
用量用于内部记账。HTTP 层先记录真实 token、缓存计数及供应商账单，再移除该客户端
未请求的用量块和账单元数据；正文、错误和 `[DONE]` 保留。明确请求 usage 的客户端
仍会收到完整统计，Messages 和 Responses 保持各自正常的用量事件。
内部统计将 Messages 的普通输入、缓存读取和写入相加作为输入总量；OpenAI 的输入
总量本就包含缓存部分，直接保留。对外响应仍遵循各自协议的输入 token 语义。

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

### Copilot 模型目录与超时

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

### Kiro 连接与重试

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

## Kiro 请求内容脱敏

### 配置

通过 `crosslane config edit features` 编辑 `[features]`：

```toml
[features]
enable_request_redaction = true
enable_request_redaction_notice = false
# request_redaction_notice_text = "自定义 Markdown 提示"
```

保存后校验并热重载，可用 `crosslane config show features --effective` 检查生效值。
修改只影响新请求，已经开始的工具调用、重试和摘要沿用原策略。脱敏默认开启；关闭后，
新请求的正文、历史、工具参数和摘要会按真实值发送给 Kiro。旧 checkpoint 封套仍在
本地解包；配置导出和日志脱敏有各自的规则，不受这个开关控制。

### 覆盖范围与误报

脱敏在 Kiro 发送入口本地执行，不使用模型或额外网络请求。处理当前输入、system 与
历史消息、工具参数及结果、描述与 schema 示例、自动摘要、重试和 Kiro MCP 搜索 query。
命中的凭证替换为请求内占位符，相同值复用同一占位符。映射仅保存在内存，不写入
日志或磁盘；上下文和字节预算包含脱敏文本及引用提示的开销。

| 输入 | 处理方式 |
| --- | --- |
| 凭证字段、赋值及当前账号已知凭证 | 屏蔽值及其精确副本，包括其他字符串内的相同内容 |
| 云凭证 | 识别 AWS、腾讯云、阿里云、火山引擎/金山云、Google 等特征前缀；无前缀秘密需结合 `SecretKey`、`AccessKeySecret`、`ak`、`sk` 等凭证名 |
| 鉴权语法 | 处理认证头、Cookie、凭证 URL、连接串密码、私钥及常见 curl、HTTPie/xh、wget、数据库、netrc、Docker/npm、SDK 凭证写法 |
| 环境变量引用 | 保留已识别变量引用、凭证文件路径和公共鉴权控制字段；明文默认值、拼接秘密仍会屏蔽 |
| JSON 与文本附件 | 解码 JSON 字段名和 UTF-8 txt/md/csv/html 附件后处理；含敏感键的损坏 JSON 采取保守处理 |
| 二进制附件 | 不解析或 OCR PDF、Office、图片中的内容 |

引用规则遵循已声明的 shell 或代码语言，例如 POSIX 单引号内的 `$TOKEN` 是字面值，
受支持的 shell 展开才是引用。Go、Java、.NET、Rust、Ruby、PHP、Deno、Bun、Node、
Python 等已识别代码取值表达式可保留变量名；字面凭证或 JSON 工具参数中看起来像 getter
的内容不因此获得豁免。检测不执行命令、getter、环境变量展开或凭证文件读取。

值短、常见或用于示例都不会自动豁免。`key: "Enter"`、`cache_key: "product:42"`、
`password: "true"` 等歧义字段可能被屏蔽。任意无标签口令、未知格式、编码或拆分的秘密
无法可靠识别，规则不能同时保证零误报和零漏报。

### 工具还原与检查点

模型应在工具参数中原样复制占位符。网关解析完整 JSON 参数后，在本地还原字符串、
数字和布尔值，兼容并行调用的流式分片。普通回答保持脱敏。明确识别的 POSIX shell
工具会按上下文处理转义，普通 HTTP/JSON 参数不增加 shell 引号；原样重放的命令和
JSON 文本保留来源中的转义、空格和换行。

任意 heredoc、嵌套命令替换、PowerShell 和 shell 内嵌语言不做通用的还原语义推断，
这些场景应使用结构化凭证参数或执行端环境变量。

同一逻辑请求的内部重试、工具续接和摘要共用映射；独立 HTTP 请求不共用。后续历史
需携带已还原参数，或保留 checkpoint 封套供新请求重建映射。未知或被模型修改的
占位符无法猜测还原。checkpoint 封套携带可逆编码的凭证，**不是加密**，应按包含
秘密的会话数据保护。

### 可选的最终回答提示

`enable_request_redaction_notice` 默认关闭，需要启用 `crs` 的构建。开启后，最新用户
正文、工具结果或文本附件在成功生成请求中被脱敏时，可在 Claude 最终文本块末尾追加
本地提示；历史凭证不会单独触发。配置编辑器展示完整默认 Markdown，自定义文案不做
变量替换，空字符串或纯空白会关闭提示。

提示属于回答正文，不调用模型，也不包含凭证或占位符。工具调用响应、压缩摘要、错误、
不完整或截断回答、结构化/JSON 输出、OpenAI 响应及其他 provider 不追加提示；工具 ID、
参数、结束原因及模型用量不变。

转发前会清理历史 assistant 文本末尾的完整默认提示，以及本地能确认来源的自定义提示，
追加前也检查是否已经存在完整提示。自定义提示通过有界内存指纹识别，不保存回答正文；
重启或记录过期后无法确认的自定义正文会保留。关闭脱敏也会关闭提示。

## 上下文与自动压缩

| 设置或限制 | 默认值 |
| --- | --- |
| Messages HTTP 请求体 | 50 MiB |
| `context.max_upstream_payload_bytes` | 翻译后的 Kiro 请求 8 MiB |
| `context.safe_input_ratio` | 所选模型输入窗口的 0.95 |
| `context.compact_safe_input_ratio` | 压缩后为 0.99 |
| `context.auto_compact_on_overflow` | `true` |

输入窗口优先采用实际模型元数据，配置窗口仅作回退。token 数量是本地估算，不保证
上游一定接受；`/v1/messages/count_tokens` 不生成摘要或启动新的压缩。

### 可压缩的范围

历史导致字节或 token 超限时，Crosslane 在发送前对完整源记录生成摘要。发布的
checkpoint 必须覆盖所有被移除的记录，包括正文中部事实和后续修正。失败、超时或
放不进窗口的检查点返回 `compaction_failed`（422），保留原始上下文，不把截取头尾
作为完整压缩结果。

系统指令、已加载工具、当前输入及必需的工具链保持原样。这些内容自身超限时，400
`context_length_exceeded` 会指出 `current_message` 或 `required_context`，给出数值
诊断及 `retryable: false`；压缩旧历史无法缩小这些内容。CRS Console 可用 HTTP 200
传递该本地错误，同时保留语义状态 400，详见[CRS 错误传递](client-compatibility.zh-CN.md#claude-relay-service-响应兼容)。

### 恢复过大的当前轮次

1. 停止重复发送同一份大粘贴、附件或工具结果，拆小输入或分页读取工具输出。
2. 保留会话，尝试 Claude Code 的 `/compact`，或发送不带大输入的简短追问。客户端
   可能把追问合并进被拒绝的轮次；大输入仍是当前轮或历史不足时，`/compact` 也可能失败。
3. 必要时在交互式 Claude Code 中用 Esc Esc 或 `/rewind` 仅编辑过大的轮次，拆小后
   重发，保留此前对话。
4. `required_context` 超限时，先缩小必需的系统指令、已加载工具或其他强制输入。

### 摘要执行与诊断

摘要分段保留原文和角色标签，同时满足 token 与序列化字节限制；初始最多 16 段，
并发最多两个请求，共享一个操作截止时间。针对明确识别的暂时错误或格式错误，只做
一次有界恢复，重新分割完整原文或仅重试失败分段。已成功的其他分段保留，部分输出
不发布；明确拒绝、认证和额度错误不重试。整体失败时，已受理的尝试仍会结算用量。

启用 `crs` 时，上游占用率可按账号、模型、会话、checkpoint 保守校准本地估算，
30 分钟后过期；不改写计费 token、输出上限或公布的模型窗口。Kiro 的
`CONTENT_LENGTH_EXCEEDS_THRESHOLD` 也会进入有界上下文恢复，但不表示准确的 token
上限，也不将账号标为故障。

Claude Code 原生最终摘要整合完整、按时间排序的 checkpoint。任一源记录的明确值
优先于其他分段的局部“未看到”声明，只有明确的后续修正才能替代先前事实。放不进
窗口的 checkpoint 会做语义合并，不截断；其他服务端压缩路径保留上游合并。

Tool Search 会检查包含新工具和结果的完整续轮。持有账号名额时，只能发布符合预算、
完整保留原文的 checkpoint；否则保留历史并拒绝超限续轮。经过完整历史前缀校验后，
可重放客户端省略的 checkpoint。

| 日志字段或事件 | 用途 |
| --- | --- |
| `payload_budget_overflow`、`pending_server_tools_payload_overflow`、`tool_search_continuation_overflow` | 定位超限阶段 |
| `proxy.compaction.summary_part_retry.started` | 失败分段恢复及其原因 |
| `proxy.compaction.summary_merge.completed` | 合并耗时及 `merge_mode`（`client_final_summary` / `upstream`） |
| `proxy.upstream.context_usage` | 上游占用率或计数，与本地估算分开 |

关闭自动压缩后仍会拒绝超限输入；客户端主动请求摘要时，保留其有界的上游上下文恢复。
