# Kiro 出站请求审计（2026-10-15）

依据用户提供的《Kiro IDE 1.2.56 向服务端发送的请求：逆向分析》（2026-10-13
补全，KAS 0.66.32），核对 Crosslane 当前实现的全部 Kiro 出站 HTTP 路径。
报告的静态分析、IDE 日志和推断具有不同证据强度；它描述的是 IDE，不能直接证明
CLI/API key 或兼容网关必须采用相同协议。本次验证使用本地 mock，没有调用真实 Kiro、
执行浏览器登录或部署到 teamsbot。

## 请求清单与处理结果

`{region}` 为 Kiro 服务区域，优先采用账号 Profile 的区域；OIDC 登录区域独立处理。
管理接口内置区域为 `us-east-1`、`eu-central-1`、`us-gov-east-1`、`us-gov-west-1`，
其他区域保留 `q.{region}.amazonaws.com` 管理 REST 回退，不向旧主机发送
ControlPlane RPC。显式管理 URL 仍优先于内置区域表。

| 出站路径 | 对照结论 / 本次处理 | 报告依据 |
| --- | --- | --- |
| OAuth 原生生成：`POST runtime.{region}.kiro.dev/` | 修正 target 为 `KiroRuntimeService.GenerateAssistantResponse`；`origin=AI_EDITOR`；在 body 写 `agentMode`，使 `agentTaskType` 与其一致，并补 `rootConversationId`；保留每轮的 `agentContinuationId`。 | 5.1–5.4 |
| API key 生成：原生 Runtime 上的 CLI 兼容协议 | 保留 CLI target、UA、`KIRO_CLI` origin；不发现或发送 OAuth Profile，不把 IDE 协议强加给 API key。 | 报告未提供 CLI 请求证据 |
| 旧生成：`q.*` / `codewhisperer.*` 的 `/generateAssistantResponse` | 保留兼容路径和已有端点排序；普通 OAuth 生成不会被此次更新强制切换到 Runtime。IDE UA 应用标识更新为报告格式。 | 2、5.3；旧路径的支持情况仍需真实上游验证 |
| OAuth 模型目录：`POST management.{region}.kiro.dev/` | 默认优先原生 ControlPlane；修正 target 为 `KiroControlPlaneBearerService.ListAvailableModels`，body 使用 `AI_EDITOR` 和 Profile；补齐 `nextToken` 分页。 | 4.1、4.2 |
| 兼容模型目录：`GET .../ListAvailableModels` | 保留显式旧端点/端点偏好和 REST 回退，补分页。原生目录返回 404/405 时短期禁用该目录路径，避免每次重复探测；不影响生成端点健康。API key 继续使用订阅安全的静态目录。 | 兼容边界，报告未证明旧协议失效 |
| Profile：`POST management.{region}.kiro.dev/ListAvailableProfiles` | 默认主机由 `q.*` 改为 `management.*`；支持分页，所有分页及排队共用 10 秒预算；新增成功结果缓存，保留既有 Builder ID/Social 固定 ARN 规则。 | 6.4 |
| 用量：`GET management.{region}.kiro.dev/getUsageLimits` | 与 Profile 共用管理端点解析；保留 `origin=AI_EDITOR`、`resourceType=AGENTIC_REQUEST`、`isEmailRequired`、Profile 查询参数和区域回退。 | 6.4 |
| 可选订阅查询：`POST .../listAvailableSubscriptions` | 共用管理主机和 extension REST 身份头，移除额外的 `OPTIN` 头；保留旧路径及失败返回空订阅的兼容行为。报告没有给出该查询的完整契约，不能据此声称已验证上游支持。 | 6.4 仅描述升级入口 |
| 默认 Web Search：`POST runtime.{region}.kiro.dev/` | 原 `/mcp` 改为原生 `KiroRuntimeService.InvokeMCP`；OAuth Profile 写进 JSON-RPC body；API key 不发送 Profile；保留双层 JSON-RPC/MCP 结果校验。 | 4.2、7 |
| 显式 MCP/Web Search URL | 保留旧 JSON-RPC HTTP 协议和 Profile header，避免改变已配置兼容网关；取消 MCP 请求中原先与生成请求相反的 `optout=false`。 | 显式配置兼容；5.2 |
| Social 刷新：`POST prod.us-east-1.auth.desktop.kiro.dev/refreshToken` | 固定使用全局认证域名，避免用服务/Profile 区域选择 token issuer；保留显式测试端点覆盖和 refresh token 轮换持久化。 | 2、6.1、6.3 |
| IdC 刷新：`POST oidc.{login_region}.amazonaws.com/token` | 保留凭证中的登录区域、客户端凭证及 refresh grant；补 `KiroIDE` UA 标识。 | 5.3、6.3 |
| SSO 注册、授权和 code 换 token | 注册和 token HTTP 请求补 `KiroIDE` UA；保留五个 scope、PKCE、state 校验和配套的本地随机回调端口。`clientName=crosslane` 是本应用标识。 | 5.3、6.2 |
| `diagnose endpoints` 的无凭证 POST 探针 | 用于旧生成主机及 OIDC 的 HTTP/TLS 连通性诊断；收到 4xx 也表示网络可达，不能作为业务认证或原生 Runtime/ControlPlane 成功的证明。 | 与 IDE 业务调用区分 |

代码入口：[Kiro HTTP 客户端](../crates/crosslane-kiro/src/client.rs)、
[区域与端点](../crates/crosslane-kiro/src/endpoint.rs)、
[身份头](../crates/crosslane-kiro/src/identity.rs)、
[token 刷新](../crates/crosslane-pool/src/refresh.rs)、
[SSO](../crates/crosslaned/src/sso/mod.rs)。普通聊天、内部续答、自动压缩摘要和
Web Search 后续生成共享同一 Kiro 生成传输，修正不局限于某个客户端 API。

## 身份头、配置和耗时

原生 Runtime/ControlPlane 使用 `application/x-amz-json-1.0`，IdC 增加
`TokenType: SSO_OIDC`，Social 不带此头；extension REST 的 IdC/Social 都不带
TokenType，API key 使用 `API_KEY`。原生 agent UA 的应用部分为
`KiroIDE 1.2.56 <machineId> KAS/0.66.32`，extension REST 不带 KAS，Social
刷新仍使用连字符格式。SDK/Node/OS 前缀沿用已有兼容标识；报告未提供这些版本的完整
UA，不能把它们当作已核实的 IDE 原始字节或本机运行环境。

`CROSSLANE_MANAGEMENT_URL` 优先控制原生模型目录与 Profile/用量/订阅 REST
请求，支持 `{region}`；未指定时，旧 `CROSSLANE_AMAZONQ_URL` 仍可覆盖 extension
管理请求。`CROSSLANE_RUNTIME_URL` 控制 Runtime，并可作为默认原生 MCP 的端点。
显式设置 `CROSSLANE_MCP_URL` 或 `upstream.web_search_endpoint` 会选择旧 JSON-RPC
兼容请求；希望采用原生 InvokeMCP 时应留空这两项，而不是把根 URL 填进去。
需要显式选择原生 OAuth 生成时使用已有配置：

```toml
[upstream]
preferred_endpoint = "runtime"
```

Profile 缓存最多 500 项、15 分钟过期，按账号 ID、access token 和解析后的服务区域
隔离，保留并发 singleflight；失败不缓存，显式 Profile 总是优先。token/区域改变会
重新发现。这样可省去后续请求中的重复网络等待，实际节省时间仍需线上测量。

模型目录所有端点、分页、Profile 解析及槽位排队共用 15 秒预算；单页最多 8 MiB。
Profile/模型分页均限制 100 页，并拒绝重复 `nextToken`，避免无限查询或把首个空页
误判为 Builder ID 回退。显式 URL 和已有静态模型回退保留；客户端 service 账号池、
API key 模型权限、全局禁用模型等过滤仍在原有处理层执行。

## 错误与保留的行为

补充原生事件 `error`、`throttlingError`、`validationError`、
`serviceUnavailableError`，保留 `reason` 与有界的 `retryAfterMilliseconds`。
`USAGE_LIMIT_REACHED` / `*_REQUEST_COUNT` / `OVERAGE_REQUEST_LIMIT_EXCEEDED`
归入额度耗尽；请求速率错误保持限流；`INSUFFICIENT_MODEL_CAPACITY` 识别为容量问题；
`PROMPT_TOO_LONG` 和 `MODEL_CONTEXT_WINDOW_EXCEEDED` 进入既有上下文恢复路径。
生成及模型目录的 HTTP 原生限流响应头优先于 body，秒单位的 `Retry-After`
转换为毫秒，上限 300 秒。额度原因优先于 `ThrottlingException` / 429 的通用限流
判断，也不会触发 403 的认证刷新或 Profile 的固定 Builder ID 回退。
错误 JSON 先限制诊断字段再序列化，避免长消息截断后丢失原因码和等待提示。

HTTP 重试提示记录为数值字段 `upstream_retry_after_ms`。本次没有将 IDE 单账号的
长等待策略复制到多账号调度器：既有账号选择、退避和尝试次数继续生效，不让一个账号的
retry-after 阻塞后续其他账号。生成连接数、连续空闲超时和工具延迟加载策略也保持原有
配置，不叠加 SDK 层重试。

系统提示继续走既有受保护历史消息，未启用报告中的实验性顶层 `systemPrompt`。
思考参数仍按模型目录的 schema 校验；工具名别名、工具结果关联、输入大小和禁用模型
限制继续生效。没有仅因报告里 IDE 不填写某字段就移除现有兼容字段，也没有新增遥测、
A/B 查询、cloud config、订阅支付或超额开关写请求。CRS 相关代码未修改。

## 验证边界

新增回归覆盖 OAuth 原生 Runtime/ControlPlane、三种认证方式的 MCP、管理 URL 覆盖、
商业区/GovCloud 管理路由、Profile 缓存及过期、分页与重复页保护、原生目录兼容回退、
token issuer 区域隔离，以及 HTTP/事件流错误分类和重试提示。原有 API key CLI、工具、
模型过滤与跨协议测试继续运行。

这些是源码和本地 mock 的验证结果。真实 Kiro 接受情况、端到端耗时变化以及 teamsbot
运行版本，需要部署后使用对应真实账号另行验证；本次没有据此报告生产验证成功。
