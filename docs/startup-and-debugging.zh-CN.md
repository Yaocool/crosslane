# Crosslane 运维指南

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [项目说明](../README.zh-CN.md)

本文介绍部署、账号、服务、模型、日常排障和备份。命令在仓库根目录执行，
并使用与 daemon 匹配的 `crosslane` CLI。

| 任务 | 入口 |
| --- | --- |
| 部署与升级 | [环境与配置](#环境与配置)、[Docker](#docker) |
| 添加账号与创建服务 | [账号](#账号)、[服务与客户端访问](#服务与客户端访问) |
| 调整模型与额度 | [模型与额度](#模型与额度) |
| 排查运行问题 | [日志与排障](#日志与排障)、[备份与恢复](#备份与恢复) |
| 配置缓存、工具加载、脱敏或压缩 | [请求处理](request-processing.zh-CN.md) |
| 接入客户端、网页搜索或 CRS | [客户端兼容](client-compatibility.zh-CN.md) |
| 自动化管理或管理 Kiro 企业订阅 | [Admin REST](admin-rest.zh-CN.md)、[Kiro 企业订阅管理](kiro-enterprise-admin.zh-CN.md) |

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

`-h`、`--help` 和 `help` 显示命令参数及示例。查看帮助无需启动 daemon，
Docker wrapper 仍需 Docker 和本地部署镜像。

```bash
crosslane service edit -h
crosslane help service edit
crosslane service help edit
crosslane help --all
crosslane help account --all
```

`help --all` 列出命令路径、别名和适用范围。查看具体操作的参数可用
`crosslane help service edit` 这样的命令路径。

| 命令组 | 管理对象 |
| --- | --- |
| `provider` | 上游实例、驱动设置、默认模型和回退策略 |
| `account` | 上游登录账号、凭证、健康状态与标签 |
| `service` | 业务监听、允许来源、客户端 Key 绑定和账号池 |
| `apikey` | 客户端访问凭据、来源/模型权限、限额与用量 |
| `models` / `model-map` | 模型目录与全局禁用 / 客户端模型名映射规则 |
| `admin` | 管理令牌与 Kiro 企业 AWS 凭证、用户和订阅 |
| `tasks` / `alert` | 周期任务 / 共享通知目标及当前 Kiro 告警事件 |

需要子命令的命令组在省略子命令时显示帮助。对这些命令组使用 `--json` 时必须指定操作。
`account overage` 和 `logs admin` 默认执行 `show`。`tasks list` 标明可手工运行的任务，
`tasks run` 的帮助和 Shell 补全会列出这些任务。watcher 和 adaptive_admission 由调度器运行。

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

第一条命令读取 CLI 环境中的 `KIRO_API_KEY`。使用企业 SSO 时，运行
`crosslane config edit sso` 设置组织的登录入口。

```toml
[sso]
start_url = "https://example.awsapps.com/start"
login_timeout_secs = 120
```

`login_timeout_secs` 控制等待浏览器授权回调的时间，单位为秒，须为正整数，默认 120。
通过 CLI 编辑器保存后会热重载，对后续登录生效。

```bash
crosslane config show sso --effective
crosslane account add-sso --email user@example.com --password-stdin < /secure/kiro-sso-password
```

交互、批量和 stdin 参数见 `crosslane account add-sso --help`。使用
`crosslane account list` 检查账号，刷新选项见 `crosslane account refresh --help`。
账号导出默认脱敏；包含凭证的导出只应存放在受保护的备份中。

Kiro 批量添加使用独立的 `email,password` 两列 CSV；支持带引号的字段和 UTF-8 BOM。
`crosslane account add --provider kiro --auth sso --batch accounts.csv` 与单个添加提交
相同的 IAM 登录凭据。逐账号进度写入 stderr，`--json` 的 stdout 保持为一个最终
结果。Copilot 的 Device Flow 和 GitHub/Azure CSV 字段不参与 Kiro 登录。
如果组织登录要求额外的凭据更新，后台登录会立即返回具体原因；需在组织登录页面
完成后，再使用当前登录密码重试。Crosslane 不会自动修改组织密码；`add-sso --headful`
仍可用于手工处理浏览器中的验证步骤。

Kiro 上游端点与协议说明见[客户端兼容](client-compatibility.zh-CN.md#kiro-端点与上游拒绝)。

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

## 独立状态 HTTP

进程状态接口使用独立监听，不依赖任何 API 代理服务，也不使用业务端口的 TLS、客户端 Key
或 User-Agent 策略。在 `crosslane config path` 显示的配置文件中设置：

```toml
[status_http]
enabled = true
host = "127.0.0.1"
port = 7651
```

默认 `enabled = false`，启用后缺省监听 `127.0.0.1:7651`。
`crosslane config edit status-http` 与 `config edit admin` 都会同时展示该监听和
关联的 REST 鉴权，并补齐旧配置缺少的默认字段。本节变更支持热重载，无需重启
`crosslaned`；手工编辑后可执行 `config reload`，绑定失败时保留当前监听和运行配置。需要从其他机器探测时，
将 `host` 改为对应的监听地址。Docker 的 host network 无需额外映射端口。

| 方法与路径 | 含义 |
| --- | --- |
| `GET /` | 进程名称、版本和状态 |
| `GET /health` | 存活检查，仅返回未打标签账号的数量，以及各服务的名称、启用状态和监听运行状态 |
| `GET /ready` | 检查必要后台任务、计量恢复状态，以及每个启用服务的监听和有效账号池；就绪为 `200`，未就绪为 `503` |

```bash
curl http://127.0.0.1:7651/health
curl -i http://127.0.0.1:7651/ready
```

这些接口只读取本地快照，不联系上游、不刷新凭证，也无需客户端 Key 或特定客户端
User-Agent。没有业务服务时，`/health` 仍返回 `200`，`/ready` 返回 `503` 并说明原因。
`/health` 对 Kiro 和 Copilot 都只统计账号自身 `tags` 为空的账号，包含其中停用或不可用
的账号，并分别给出各健康状态的数量；账号被多个服务共用时不会重复计算。没有设置
`account_tag` 的服务同样只使用未打标签的账号，与 `/health` 的统计范围一致（手工绑定的账号除外）。
`/health` 的响应不包含 credits、service ID、来源信息、绑定关系或原始错误；`services` 每项仅包含
`name`、`enabled`、`running`。`/ready` 继续按各服务真实的来源、标签、手工绑定和排除规则
判断就绪，带标签的服务不会因 `/health` 只统计未打标签账号而被错误判断为不可用。

迁移后，业务服务端口上的 `/`、`/health`、`/ready` 返回 `404`，已有 HTTP 探针需要改用
独立状态端口。自动创建业务服务时会避开已启用的状态端口。模型协议接口、模型列表与
客户端兼容端点 `/api/event_logging/batch` 继续使用业务端口；CLI 管理仍使用 Unix socket。

## 服务与客户端访问

独立监听还可开启带独立 Bearer 鉴权的 `/admin/v1` 管理接口，支持按来源和标签管理账号、
创建和启停服务、查询分组账号池及额度。配置与完整接口见 [Admin REST API](admin-rest.zh-CN.md)。

### 创建监听

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

### 来源路由与故障切换

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

### 客户端鉴权

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

## 日志与排障

```bash
crosslane logs show --tail 100
crosslane logs follow --level warn
crosslane logs trace TRACE_ID
crosslane logs admin show --tail 100
crosslane logs admin show --request-id admin_0123456789abcdef0123456789abcdef
crosslane logs files --admin
crosslane status --since 30m
crosslane stats --detail --since 1h
```

Admin 管理面事件写入独立的 `crosslaned-admin-日期-级别.log` 文件，Service 与其他 daemon
事件保留在 `crosslaned-日期-级别.log` 文件。`[log].file_path` 自定义基础路径时，Admin
文件在同一目录，以 `-admin` 后缀区分；两类文件都使用相同的日志级别、轮转和保留配置。
`logs admin show` 和 `logs admin` 都可按响应头 `x-request-id` 中的 `admin_...` 精确筛选。
`--level` 筛选对应级别的文件。HTTP 审计事件记录在 `info` 文件中，包括 4xx 和 5xx 响应。
`logs files --admin` 和 `logs path --admin` 可查看 Admin 文件的位置。
旧版本写入的 Admin 事件保留在原日志文件中。

`logs show` 和 `logs follow` 可按实例 ID、日志中的 `account_id` 及 HTTP 状态筛选近期 Service
请求。查看全部来源时省略 `--provider` 并取消设置 `CROSSLANE_PROVIDER`。
使用 `--json` 时，每条请求输出一行 JSON（NDJSON），其中的 `account_id` 可用于 `--account` 精确筛选。
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

上下文超限与自动压缩见[请求处理](request-processing.zh-CN.md#上下文与自动压缩)；
工具、网页搜索及 CRS 错误传递见[客户端兼容](client-compatibility.zh-CN.md)。
管理接口错误与响应保存见 [Admin REST](admin-rest.zh-CN.md#保存与排查-json-响应)。

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
