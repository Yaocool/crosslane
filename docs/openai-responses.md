# OpenAI Responses 与 Codex 接入

[项目说明](../README.zh-CN.md) · [协议边界](protocol-compatibility.zh-CN.md) · [CLI 迁移](startup-and-debugging.zh-CN.md#旧命令迁移)

本文对应当前源码，包含 `v0.2.4` 之后的未发布改动。**默认状态仅保存在当前进程中**；
需要跨重启恢复的客户端应保留完整历史并能切换为 `store: false`。

业务面提供 `POST /v1/responses` 和 `POST /responses`，支持非流式 JSON 与流式 SSE。
请求复用现有 OpenAI → Kiro 翻译、账号池、模型映射、计费、预算、重试和工具参数校验。
流式模式逐段转换上游输出；最终响应仍含完整 output 和 usage。

## Codex 配置

先创建代理服务并导入可用账号，使用 `crosslane ready` 和 `crosslane models list` 核对服务与模型。
base URL 必须包含 `/v1`；`CROSSLANE_API_KEY` 是代理服务允许的客户端 Key，不是上游 `ksk_...`。

在用户级 `~/.codex/config.toml` 中配置 provider；请保留已有的其他配置：

```toml
model = "claude-sonnet-4.5"
model_provider = "kiro"

[model_providers.kiro]
name = "Crosslane"
base_url = "http://127.0.0.1:5580/v1"
wire_api = "responses"
env_key = "CROSSLANE_API_KEY"
requires_openai_auth = false
```

将 `CROSSLANE_API_KEY` 设置为该代理服务生成或允许的 API key，然后启动 Codex。
服务地址和 model 按实际部署及账号可用模型调整。配置字段见
[Codex 官方配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)。

## 最小请求

将 `CROSSLANE_API_KEY` 设置为客户端 Key 后，可用以下请求检查协议。示例携带 Codex 产品标识，
用于冒烟检查；实际第三方客户端可配置单个服务或 Key 的 User-Agent 豁免。
生成请求会调用真实上游并消耗额度；把 model 改为账号可用值。

```bash
curl -sS http://127.0.0.1:5580/v1/responses \
  -H "authorization: Bearer $CROSSLANE_API_KEY" \
  -H 'content-type: application/json' \
  -H 'user-agent: codex_cli_rs/0.144.0' \
  -d '{"model":"claude-sonnet-4.5","input":"Reply with hello.","store":false,"max_output_tokens":64}'
```

流式模式增加 `"stream": true`，curl 使用 `-N`。客户端应等待最终状态事件，不能仅凭 HTTP 200
认为工具调用或整轮响应已成功。

## 客户端准入

`server.enforce_user_agent_check` 默认为 `true`，旧配置无需增加新字段：

| 路由 | 允许的客户端 |
| --- | --- |
| `/v1/messages`、`/messages`、`/anthropic/v1/messages` 及对应 `/count_tokens` | Claude Code |
| `/v1/responses`、`/responses` | Codex |
| `/v1/chat/completions`、`/chat/completions` | Codex |
| `/v1/models`、`/models` | Codex、Claude Code |

检查使用 User-Agent 中的客户端产品标识，覆盖 Codex CLI、exec、编辑器和桌面端。
仅有 `originator` 请求头或 User-Agent 中随意包含 `codex` 不会通过检查。
认证先于客户端检查；拒绝请求使用对应协议的错误格式，且不访问 Kiro 上游。
健康检查及遥测路由保持原有行为。

需要对所有服务开放其他协议客户端时，可将此开关设为 `false` 并重载配置。只豁免一个
服务或 API Key 时，分别在对应的 `[[proxy_service]]` 或 `[[api_key]]` 中设置
`skip_user_agent_check = true`，也可以使用 `crosslane service edit` 或 `crosslane apikey edit`
修改。全局关闭、service 豁免、API Key 豁免依次决定最终策略；任一层豁免都会跳过检查。
API key 认证、服务级 key 白名单、额度和并发限制继续生效。

## 支持范围

| Responses 能力 | 处理方式 |
| --- | --- |
| `input` 字符串 | 转换为 user 消息 |
| 消息数组 | 支持 system、developer、user、assistant；支持 input_text 和 output_text |
| `instructions` | 转换为受保护的 system 上下文 |
| `input_image` | 支持公开 HTTP(S) URL 和 base64 data URL，复用图片下载与校验限制；工具结果也可返回图片 |
| `function_call` / `function_call_output` | 保留 call_id、名称、JSON 参数和结果；校验调用与结果配对 |
| Codex 定时任务启动上下文 | `codex_app.automation_update` 的独立 `function_call_output` 在缺省或 null `call_id` 时转为 developer 文本消息，保留任务上下文 |
| `custom_tool_call` / `custom_tool_call_output` | 自由文本工具映射为 Kiro 的 input 字符串参数，返回时恢复原格式 |
| function、custom、namespace 工具 | 同时读取顶层 `tools` 与 Responses Lite 的 `input[].additional_tools`；展平命名空间后复用名称规范化，返回时恢复 namespace 与名称；拒绝名称冲突 |
| `tool_choice` | 支持 auto、none、required、指定 function/custom 工具，以及仅限 function/custom 的 `allowed_tools` |
| `parallel_tool_calls` | 沿用 Chat Completions 的工具选择与提示约束 |
| `store` / `previous_response_id` | 默认启用的受限进程内续轮；`store: false` 显式关闭，按服务与 API key 隔离，不写磁盘 |
| 工具结果后的空上游轮次 | 无可见文本、无后续工具且未触发输出上限时重试一次；再次为空则返回 502 / `response.failed`，不误报完成 |
| `max_output_tokens` | 映射为 Kiro maxTokens；省略时不补默认生成上限 |
| `temperature`、`top_p` | 复用现有参数校验和映射，保留零值 |
| `reasoning.effort` | 复用现有 effort 映射；none 关闭本次推理 |
| 推理输出与历史 | 返回 reasoning summary 事件/条目；流式摘要按完整 Kiro 段落发布，避免 Codex 丢失增量后只显示空占位；回传的明文摘要保留在 assistant 上下文 |
| `text.verbosity` | 转为回答详略的 system 提示；不提供硬性字数保证 |
| `text.format`、工具 `strict` | 作为兼容提示接收并忽略；不因 JSON Schema 或 strict=true 拒绝请求 |
| `metadata` | 回显为响应元数据，不作为客户端认证凭据 |
| usage | 返回 input_tokens、output_tokens、total_tokens、cached_tokens、reasoning_tokens |

`custom.format` 的 grammar 定义会附加到 custom 工具描述中作为模型输入提示；Kiro 不提供原生语法约束，
因此这不是服务端强校验。
`reasoning.summary/context`、`include`、`service_tier` 和未使用的 `stream_options` 键宽松接收；
`reasoning.summary: "none"` 不返回摘要，其他模式使用 Kiro 可提供的数据，不按官方参数枚举额外拒绝。
空白、纯省略号及空 HTML 注释不会创建 reasoning 条目。未知的 `text.verbosity` 不添加详略提示。
`prompt_cache_key` 用作 Kiro 会话/Prompt Cache 亲和提示，经 API key 隔离后哈希为 UUID，不原样传给
Kiro。`prompt_cache_retention`、`safety_identifier`、`user` 和 `client_metadata` 作为客户端元数据
接受，不传给 Kiro；缓存命中仍由既有缓存规则和上游决定。
可选的 `stream`、`include`，以及 function 工具的 `description`、`parameters`、`strict`
显式传 `null` 时按省略处理；`parameters: null` 使用无参数工具的默认 schema。

## Codex 定时任务兼容

Codex Desktop 启动定时任务时可能注入以下输入项，它承载任务信息，没有对应的模型工具调用：

```json
{
  "type": "function_call_output",
  "id": "fco_bootstrap",
  "namespace": "codex_app",
  "name": "automation_update",
  "output": "Automation: Daily check\nAutomation ID: daily-check\nRead the saved task instructions."
}
```

此前该条目会在本地请求校验阶段触发 `invalid value for input.N.call_id: expected a string`，
尚未选择账号或访问 Kiro。Codex 上游已有相同报告：
[macOS / Azure #41799](https://github.com/openai/codex/issues/41799) 和
[Windows / DeepSeek #41690](https://github.com/openai/codex/issues/41690)。

代理采用上述报告中验证过的 developer 消息转换方式：仅对 `function_call_output`、
`namespace: "codex_app"`、`name: "automation_update"` 且 `call_id` 缺省或为 `null` 的组合，
将 `output` 字符串或文本内容数组保留为 developer 上下文。转换沿用 system/developer 的 Kiro
上下文保护路径，不丢弃任务信息，也不生成虚构的工具调用或 ID。空字符串、非字符串 ID、其他工具
及带 ID 的未配对结果仍按原规则校验；启动上下文也不能替代实际工具调用的结果。

该转换支持两个 Responses 路由、JSON/SSE，以及完整历史回放和 `previous_response_id` 续轮。
debug 事件 `proxy.compatibility.input_normalized` 记录转换类型和输入索引，不记录任务正文。
任务调度仍由 Codex Desktop 执行，代理处理其发来的模型请求。

## 无状态与受限状态续轮

与 OpenAI Responses 一致，`store` 缺省或为 `true` 时，完成的 response 会暂存于代理进程内；
下一轮可只提交新输入和
`previous_response_id`。代理会恢复此前 input/output，并在新请求省略两种工具声明渠道时继承此前的
function/custom 工具及 tool choice。显式 `tools: []` 或 `additional_tools` 控制项会提供新的有效
工具集，而不是叠加进已存目录；此时也不继承此前的 tool choice。`additional_tools` 本身不会作为
对话历史反复存储，解析出的有效工具会单独继承。续轮复用父 response 已解析出的 Kiro conversation ID，不受后续请求缺失或更换
session-affinity 请求头影响。

显式传 `store: false` 时保持无状态：下一轮需把上一轮的 `output` 和对应工具执行结果追加到
`input`。`function_call_output.call_id` 对应调用的 `call_id`，不是 `fc_...` 条目 ID。

这是有边界的进程内实现，不提供持久化存储服务：

| 边界 | 当前值与行为 |
| --- | --- |
| 过期 | 已保存记录空闲 30 分钟后过期；读取会更新访问时间。 |
| 数量 | 整个进程最多保留 256 条 response 状态快照，同一对话的多轮也分别占记录。 |
| 大小 | 单条序列化状态最多 2 MiB，总量最多 32 MiB；不是整个进程的内存上限。 |
| 淘汰 | 数量/总大小不足时淘汰最久未访问记录，因此 30 分钟内也可能失效。 |
| 隔离 | 按代理 service 和已认证 API key 隔离；重启全部清空，多副本不共享。 |
| 无效引用 | 未知、过期、淘汰或其他 service/key 的 `previous_response_id` 返回 HTTP 400。 |

这些上限由[状态存储实现](../crates/crosslaned/src/http/responses.rs)固定，当前没有 TOML 调节项。
无法保存新状态时，非流式请求返回服务端错误，流式请求以 `response.failed` 结束。
客户端应保留可重放历史；引用失效后移除 `previous_response_id`，提交完整 input 并按需设置
`store: false`。不能只删引用、仍只发送孤立的工具结果，否则缺少调用配对。

不提供 response 的查询、删除或取消端点。父 response 的 `instructions` 不自动带入续轮，
续轮需要单独设置新的 `instructions`。

以下参数仍需要尚未实现的执行/数据链路，因此返回 HTTP 400：

- `conversation`、`background: true`。
- `context_management` 的服务器端执行。
- defer_loading 工具、托管工具（如 web_search、file_search、computer、MCP server 执行）。
- Files API 的托管 `file_id`；`input_file` 的内联 base64/data URL 或公共 URL 已转换为 Kiro 文档，支持工具输出内的文件。

输入历史里无法转换的条目**不再拒绝**，而是跳过后继续处理，因为 Codex 会把上一轮收到的条目原样回传，
拒绝会让第二轮起的整段会话不可用：

- `reasoning` 条目的非空 `encrypted_content`（Kiro 无法解密）：丢弃该不透明块，保留同条目内的明文
  `summary`/`content`。
- `item_reference`、托管工具调用条目（如 `web_search_call`）及其他未识别类型：跳过。
- 上述跳过会记录 debug 事件 `proxy.compatibility.controls_ignored`，只含固定字段/类型名。
- 若输入**只**由这些可跳过条目组成、不含任何真实消息，仍按缺少会话内容拒绝。

Codex 常用的 `include: ["reasoning.encrypted_content"]` 可以提交，但本代理只返回明文推理，
不会伪造加密 replay token。这里不提供 WebSocket 或 `/responses/compact`；长上下文仍应由客户端
维护；也可显式启用 `truncation: auto`。

## 自动裁剪

`truncation` 接受 `disabled`（默认）和 `auto`。`auto` 先选择账号并解析实际 Kiro 模型，
按其安全上下文窗口移除最早的完整轮次，保留 system/developer 指令、当前输入以及关联的
工具调用链，不生成摘要。若这些必须保留的内容本身就超限，仍返回明确的上下文错误。
模型回退、换账号重试和内部续写也沿用该设置；切换到更小的窗口时重新裁剪。
无法容纳必须保留内容的候选模型不会破坏供其他候选使用的历史。启用自动裁剪时，
缓存用量采用上游数据，不再根据尚未裁剪的原始请求估算本地缓存命中。
JSON/SSE 响应回显实际请求模式。`max_tool_calls` 可以提交并回显；它限制服务器工具调用，
当前 Responses 不执行托管工具，因此不会用于截断客户端 function/custom 调用。

## 流式事件

流按顺序发送 `response.created`、`response.in_progress`、output item/content part 的 added、
文本/推理/工具参数的 delta 和 done，再发送 `response.output_item.done`。
事件带连续递增的 `sequence_number`，item_id、output_index、call_id 在同一响应中保持稳定。

正常结束使用 `response.completed`；达到输出上限使用 `response.incomplete`，
`incomplete_details.reason` 为 `max_output_tokens`。上游中断或工具 JSON 损坏使用
`response.failed`，不发送成功结束事件，也不把未完成的工具调用标记为可执行。
Responses 不使用 Chat Completions 的 `[DONE]` 结束标记。

协议结构依据 [Responses 官方参考](https://developers.openai.com/api/reference/resources/responses)
和 [流式事件参考](https://developers.openai.com/api/reference/resources/responses/streaming-events)。

## 回归验证

```bash
cargo test -p crosslane-translate responses:: --locked
cargo test -p crosslaned http::responses::tests --locked
cargo test -p crosslaned --test end_to_end responses:: --locked
cargo test -p crosslaned --test end_to_end compatibility_controls:: --locked
```

这些用例使用本地模拟上游，验证转换、状态隔离/过期、工具配对和 JSON/SSE 输出；
生产客户端版本、上游可用性和长时间运行需另外验证。
