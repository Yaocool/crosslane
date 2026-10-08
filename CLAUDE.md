# CLAUDE.md

This file provides architecture and maintenance guidance for this repository.
Start with [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow and [the README documentation links](README.md#documentation) for user-facing guides.

## 项目定位

`crosslane` 是无 GUI 的 Rust 多 provider 代理服务，内置 Kiro 与 GitHub Copilot，统一提供 Claude Messages、OpenAI Chat Completions 与 Responses 兼容 API。Kiro 支持 **企业 SSO（AWS IAM Identity Center / IdC）** 和显式导入的 headless API key（`ksk_...`）；Copilot 支持 GitHub Device Flow 和 token 导入。上游凭证与代理客户端 API key 分开管理。

## 常用命令

```bash
# 提交前的完整校验集（缺一不可，clippy 以 -D warnings 运行）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker compose config --quiet

# 缩小范围调试（单 crate / 单测试名）
cargo test -p crosslane-kiro
cargo test -p crosslaned http::tests::every_response_has_a_unique_trace_id
cargo test -p crosslane-pool refresh::tests::successful_refresh_preserves_cooling_and_exhausted_health

# 按集成测试 target 缩小范围；上方 workspace 全量命令已经包含这些 target
cargo test -p crosslaned --test end_to_end                              # 全量端到端（wiremock 模拟上游）
cargo test -p crosslaned --test end_to_end compatibility_controls::     # 端到端子模块
cargo test -p crosslane-translate --test compatibility_controls
cargo test -p crosslane-translate --test openai_roundtrip
cargo test -p crosslane --test dotenv
```

`crates/crosslaned/tests/end_to_end.rs` 用 `end_to_end/` 目录挂子模块（`compatibility_controls` / `responses` /
`thinking_controls` / `claude_gateway` / `manual_compaction` / `warning_regressions`），新增端到端场景加子模块文件而不是继续堆主文件。

工具链由 `rust-toolchain.toml` 钉在 1.97.1（edition 2021），无需手动 `rustup override`。

### 本地启动

`.env.example` 里的 `CROSSLANE_HOME=.crosslane-dev` 把配置/数据/日志/admin socket 隔离到仓库内，开发时应保持该约定，避免污染 XDG 目录。

```bash
cp .env.example .env
cargo run -p crosslaned                      # 首次启动生成 config.toml/accounts.json/daily.json/stats.json
cargo run -p crosslane -- health              # 另一个终端；CLI 通过 Unix socket 连 daemon
cargo run -p crosslane -- service create --name main
```

`crosslaned` 在参数解析前加载 `.env`；`crosslane` 先处理 help/guide/completions/version 和组导航，再为业务命令加载 `.env` 并正式解析参数。两者从当前目录向上查找，已有进程环境优先于 `.env`。相对 `CROSSLANE_HOME` 仍相对于进程工作目录，跨目录执行应使用绝对路径。常用开关：`CROSSLANE_HTTP_PORT`（覆盖端口等于 `server.port` 默认值的服务，不创建服务）、`CROSSLANE_DISABLE_HTTP=1`（只保留管理面）、`CROSSLANE_ADMIN_SOCKET`（只影响 CLI）、`CROSSLANE_CODEWHISPERER_URL` / `CROSSLANE_AMAZONQ_URL` / `CROSSLANE_MCP_URL` / `CROSSLANE_RUNTIME_URL` / `CROSSLANE_MANAGEMENT_URL`（受控上游或 wiremock）。

wiremock 与端到端测试会绑定临时 loopback 端口，沙箱 CI 必须允许本地监听。

## 架构

### 业务面与管理面

进程只有一个入口 `crates/crosslaned/src/main.rs`，但对外暴露两个互不相通的平面：

- **业务面**：`crates/crosslaned/src/http/`，由 `ProxyServiceManager` 按 `config.toml` 里的 proxy service 列表动态增删 axum listener。每个 service 有独立 host/port 与 API key 白名单，`router_for_service` 为其单独构造 Router，`ServiceHttpState` 携带 `{app, service, allowed_api_key_ids}`。新增/删除 service 无需重启 daemon。
- **管理面**：`crates/crosslaned/src/admin/`，Unix socket 上的行分隔 JSON-RPC。`crosslane-ipc::protocol` 定义请求/响应与结果 DTO，`admin/handlers.rs` 按 method 分发。新增需要 RPC 的 CLI 能力时，同步 method/DTO、daemon 分发、CLI 定义与执行；复用现有 RPC 或纯本地导航不需要新建 method。CLI 定义分布在 `main.rs` 和 `commands/`，帮助与补全从同一 Clap 命令树生成。

### crate 依赖方向

```
crosslane-core  ← 领域模型、默认值、config 校验（无 I/O 依赖）
crosslane-provider ← provider 接口、共享模型禁用策略
crosslane-copilot ← GitHub Copilot 认证、模型发现和协议转发
crosslane-store ← 原子落盘、.env 加载、bootstrap、config 热重载 watcher
crosslane-ipc   ← daemon/CLI 共享协议
crosslane-translate ← Claude/OpenAI ↔ Kiro 转换、校验、token 估算
crosslane-kiro  ← Kiro HTTP client、Event Stream 解码、endpoint 状态、模型发现（依赖 translate）
crosslane-pool  ← 账号健康、额度预留、并发、加权调度（依赖 core/translate/kiro/store）
crosslane-notify← webhook 投递与抑制
crosslaned / crosslane ← 二进制，组合各自需要的内部 crate
```

### 单请求链路

业务面热区是 `crates/crosslaned/src/http/`，按阶段拆成子模块，改动前先认准落点：

| 文件 | 职责 |
| --- | --- |
| `handlers/entrypoints.rs` | axum handler 本体（`claude_messages` / `openai_chat` / `health` / `readiness`）、准入、读体 |
| `handlers/request.rs` | `handle_claude`：校验、上下文编辑、模型映射、生成 Kiro payload |
| `handlers/execution.rs` + `execution/dispatch.rs` | 选号、打上游、重试、续轮决策 |
| `handlers/compaction.rs` | 压缩决策与摘要子请求 |
| `handlers.rs` | 上述子模块共用的类型与工具函数（不再是单文件巨兽） |
| `http/stream.rs` + `stream/response_loop.rs` | Kiro Event Stream → Claude/OpenAI SSE |
| `http/response.rs` / `responses.rs` | 非流式响应组装 / Responses 端点编码 |

单请求链路：

1. 两级准入：`state.connections.try_acquire()` 与 `state.admission.try_acquire()`，任一失败直接返回 overloaded；再经 `entrypoints.rs::read_bounded_body` 限制 50 MiB（`MAX_BODY_BYTES`）。
2. 反序列化 `ClaudeRequest` → `validate_claude` → `apply_context_management_edits` / `normalize_compaction_boundary`（Claude 上下文压缩在本地模拟，未知 edit 家族是前向兼容 no-op）。
3. Claude 翻译后由 `prepare_upstream` 预选账号并解析实际模型，再决定压缩或拒绝；不需要压缩时复用 `PreparedUpstream`，压缩前释放预选 lease。摘要后重新调度遇到更小窗口时最多复用产物重规划一次。详见[压缩与窗口](docs/protocol-compatibility.zh-CN.md#自动压缩与窗口)。
4. Tool Search 与 catalog 索引跑在 `spawn_blocking`，不占 HTTP runtime 线程；`features.tool_search_max_rounds`（默认 4，硬上限 8）限制续跑轮数，`features.tool_search_max_operations`（默认 32，上限 256）限制同一客户端请求的累计检索操作数。
5. `AccountPool::acquire` 加权选号并预留额度，`KiroClient::generate` 打上游，`stream.rs` 转 SSE；多轮工具调用经 `auto_continue_payload` 续接，上限 `features.auto_continue_rounds`（**默认 0 即关闭**，`.min(30)` 硬夹）。
6. 失败经 `record_failed_request` 计入 `stats` 与 `meter`；账号级错误反馈 `record_error` / `record_quota_error` / `mark_banned`。

`AppState`（`crosslaned/src/state.rs`）是唯一共享状态，内部大量 `RwLock<...>` 字段（pool、kiro、notifier、refresher、tls_config、runtime_config）配合 `account_mutation` / `config_mutation` 两把 `Mutex` 串行化写操作。改动这些字段时优先复用 `apply_config_transaction`，不要新开锁顺序。

### 服务端工具的本地执行

Responses 的入站转换位于 `crosslane-translate/src/responses.rs`，复用 OpenAI 请求执行链；出站 JSON/SSE 编码位于 `crosslaned/src/http/responses.rs`。默认存储受限进程内状态，`store=false` 时客户端需传完整历史；续轮按 service/API key 隔离，过期/淘汰/重启后不可恢复。协议范围见 [Responses 文档](docs/openai-responses.md)。默认 Claude 路由仅限 Claude Code，Responses/Chat Completions 仅限 Codex，模型列表接受两者；全局 `server.enforce_user_agent_check` 和 service/API key 的 `skip_user_agent_check` 决定最终策略。

Kiro 没有原生 Tool Search / Web Search server tool，代理在本地补齐并**续接同一次模型回合**：

- Tool Search（`translate/tool_search.rs`）：接受 `defer_loading`、regex/BM25 检索和 `tool_reference` 回放，按请求 limit 与剩余预算装载。轮次耗尽返回 Claude `pause_turn`；累计搜索操作超限返回响应内 `unavailable` 结果。两种预算的含义不能混淆。
- Web Search（`translate/web_search.rs`）：由 Kiro 决定是否搜索并给出 query，代理调 Kiro `/mcp` JSON-RPC 后把真实结果作为 tool result 回灌。`features.web_search_max_rounds`（默认 20）是代理侧上限，客户端请求值不得超过它。结果附带**代理自有**的 AES-256-GCM replay 内容（密钥 `web-search-replay.key`，0600，永不覆写），被篡改的记录在进入模型上下文前拒绝；Anthropic 自有的 opaque 值照收但不本地解密。

### 上下文压缩

压缩已是**语义摘要**而非纯截断，主逻辑在 `handlers/compaction.rs`：

- 摘要输入先转换为无 tools 的完整可读历史。能装下时直接生成 `<summary>`，超长时无损分段；自动溢出压缩只有全部分段能在同一并发波次启动时才做语义摘要，否则立即走 extractive fallback，避免多波次在一次总超时后丢弃结果。显式客户端 compact 保留完整语义分段行为。最多 16 段、并发最多 2（服从账号并发配置）。
- 摘要子请求独立走 `AccountPool`、额度预留与 stats，内部统计路径 `/internal/compact`，不混入主响应顶层 usage。
- 超时/额度不足/上游失败/摘要非法时恢复原 payload 并退回 extractive fallback，日志 `compaction_mode` 区分 `semantic` 与 `extractive_fallback`。
- 相关配置：`context.auto_compact_on_overflow`（默认开）、`compaction_summary_model`、`compaction_summary_timeout_ms`（默认 120000；保留已有显式配置）、`compaction_preserve_recent_turns`（默认 3，上限 64）。
- Claude Code 2.1.260 忽略 `compaction_delta`，因此对识别出的 Claude Code 客户端在 start 块携带完整 checkpoint 并直接 stop；其他客户端保持 null start + 完整 delta。不要同时发送两份摘要，TypeScript SDK 会拼接重复内容；不要发空 delta，Python SDK 会覆盖已有摘要。
- 进程内 `CompactionReplayTracker` 只缓存 checkpoint、源消息数和源消息前缀的 SHA-256 指纹；客户端漏回传边界时，仅在 service/会话隔离且完整历史前缀哈希匹配后重放，不保存原始请求正文，也不能减少客户端到代理的请求体大小。

### 错误码约定

HTTP `413` / `request_too_large` **只**用于真实入站 body 超过 50 MiB。tool 预算、context 预算、翻译后 payload 超限一律用 `400`，否则 Claude Code 会误报成 32 MB 附件失败。错误响应保持标准 Claude/OpenAI body，诊断信息走 header：`request-id`、`x-crosslane-error-code`、`x-crosslane-error-stage`、`x-crosslane-upstream-status`、`x-crosslane-account-error`。

## 编码约定

- **依赖全部精确锁定**（`=1.0.86` 形式）且集中在根 `[workspace.dependencies]`，crate 内只写 `xxx.workspace = true`。升级依赖要连带评估 `Cargo.lock` 与 Docker 缓存层。
- `chromiumoxide 0.9.1` 与 Chromium 快照 `r1566079` 是**成对**钉住的（CDP 协议兼容），改一个必须同时改另一个并实测。
- `crosslaned` 的 `sso` feature 默认开启（引入 chromiumoxide）；`--no-default-features` 对应 Docker `runtime-slim` 目标，浏览器 SSO 相关代码必须置于 `#[cfg(feature = "sso")]` 之后。
- 注释与文档字符串中英混排是既有风格（`//!` 模块头多为英文，行内解释常为中文），跟随所在文件的既有语言，不要统一改写。
- 配置校验集中在 `crosslane-core/src/config.rs`；新增配置项需同时补默认值、校验分支与热重载路径，非法 TOML 必须保留上一份有效配置而不是崩溃。
- 账号文件 `accounts.json` 含凭据、以 `0600` 创建；导出默认脱敏，仅在受保护的备份场景使用 `--include-secrets`。测试与日志中不得回显 token。

## 文档

`docs` 只维护六份文档，入口在 [README](README.zh-CN.md#文档)。重叠内容合并，删除文档时同步修正链接。

- [协议兼容](docs/protocol-compatibility.zh-CN.md#兼容性维护原则)维护参数基线与固定参考路径。
  output_config.format / response_format / text.format、tool strict、eager_input_streaming 等接收并忽略，
  未知附加字段不单独触发拒绝。**新增拒绝条件前必须阅读**，说明是转换所需、已实测的 Kiro 限制还是资源/安全边界。
- [自动压缩与窗口](docs/protocol-compatibility.zh-CN.md#自动压缩与窗口)统一维护实际模型预选、摘要资源、
  手动 `/compact`、流式回放和降级。分段前无损不代表摘要无损；跨段滚动归并尚未实现。
- [Responses](docs/openai-responses.md)维护 Codex 接入、工具、进程内状态及流式边界。
- [运维指南](docs/startup-and-debugging.zh-CN.md)维护 CLI 迁移、配置、部署和备份恢复。
- [贡献指南](CONTRIBUTING.md#validation)维护验证命令及发布约定。
  不要把历史检查通过当作当前代码已经通过。
