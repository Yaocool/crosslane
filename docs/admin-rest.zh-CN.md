# Admin REST API

[English](admin-rest.md) | [简体中文](admin-rest.zh-CN.md) | [项目说明](../README.zh-CN.md)

管理前缀：`http://127.0.0.1:7651/admin/v1`。接口与公开的 `/health`、`/ready`
共用独立状态监听，不依赖业务 service；业务端口不提供管理接口。
Admin token 与模型客户端 Key 分开管理。

本文介绍 Crosslane 本地账号、服务、账号池、额度及运行管理。AWS 凭证、企业用户和
套餐变更见 [Kiro 企业订阅管理](kiro-enterprise-admin.zh-CN.md)，这些资源与本地
代理账号不同；Kiro CLI 也可直接走 Unix socket，无需开启 REST。

`GET /admin/v1/openapi.json` 返回带鉴权定义的 OpenAPI 3.1 文档，
[仓库内的 schema](../crates/crosslaned/src/admin/rest/openapi.json) 是完整字段参考，
下文示例覆盖常用操作。

[启用](#启用与鉴权) · [账号](#账号管理) · [批量添加](#批量增加与自动-sso) · [服务](#service-管理) · [额度](#额度含义) · [响应](#响应与审计) · [保存 JSON](#保存与排查-json-响应)

## 启用与鉴权

### 首次启用

新配置的 `[status_http]` 与 `[admin.rest]` 默认均关闭。运行
`crosslane config edit admin`（或 `config edit status-http`）一起编辑两个模块。
先开启监听，保持 `[admin.rest] enabled = false`，保存：

```toml
[status_http]
enabled = true
host = "127.0.0.1"
port = 7651
```

随后创建第一把令牌，并显式开启 REST：

```bash
crosslane admin token create --name ops --enable-rest
crosslane admin token create --name monitor --role read-only
crosslane admin token list
```

CLI 通过 daemon 的属主专用 Unix socket 操作，无需已有 Bearer token。
`--enable-rest` 要求运行中的监听启停状态、地址和端口与磁盘配置一致；监听变更失败或
尚未应用时先重载。该参数不新增端口或修改业务服务；省略时保留当前 REST 开关。

已有令牌时，将 `[status_http]` 与 `[admin.rest]` 一起开启即可，无需重复创建。
也可先在监听关闭时创建令牌，再一并启用；没有监听或没有配置令牌时不能开启 REST。

将创建或轮换返回的令牌明文放入环境变量 `CROSSLANE_ADMIN_TOKEN`，通过请求头发送：

```bash
BASE=http://127.0.0.1:7651/admin/v1
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/providers"
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/openapi.json"
```

不能发送摘要或通过查询参数传 token。只读令牌允许 GET/HEAD，写请求返回 `403`。
缺失、错误、停用或已撤销的令牌返回 `401`，管理接口关闭返回 `404`。普通模型客户端
Key 不授予管理权限；OpenAPI、未知路径和不支持的方法同样先鉴权。该监听不复用业务
TLS，跨机器访问需使用 HTTPS 反向代理或 SSH 隧道。

### 令牌管理

| 命令（前缀 `crosslane`） | 用途 |
| --- | --- |
| `admin token create --name NAME` | 创建，默认角色 `admin`，`add` 为别名 |
| `admin token list` | 令牌元数据及 REST/监听启用状态 |
| `admin token show ID_OR_NAME` | 单把令牌元数据 |
| `admin token rotate ID_OR_NAME` | 返回新明文、旧明文失效；保留 ID、名称、角色及启停状态 |
| `admin token disable ID_OR_NAME` | 停用并保留记录 |
| `admin token enable ID_OR_NAME` | 重新启用 |
| `admin token revoke ID_OR_NAME` | 永久删除，`delete`、`rm` 为别名 |

明文只在创建或轮换成功时返回一次。配置仅保存 SHA256 摘要；列表、详情及其 JSON
均不包含明文或摘要。自动化可从 `crosslane --json admin token create --name ci`
的 `secret` 字段取得新明文，不应记录完整创建响应。可按调用方独立命名、轮换和撤销，
最多支持 128 把命名令牌及两项旧凭证。命令不接受 `--provider`，不受
`CROSSLANE_PROVIDER` 默认值影响。

变更在配置写入成功后立即影响后续请求。写入失败时核对或恢复原文件，无法验证回滚
时明确报错，并记录不含秘密的 `config.persistence_failed`。撤销最后一把令牌会关闭
REST，保留公开状态监听及业务配置；已鉴权请求和已受理批量任务不会自动取消。
审计记录包含令牌 ID、角色及动作。

### 重载与重置

监听启停、地址和端口支持热重载。编辑器保存时一并校验并应用，手工编辑后执行
`crosslane config reload`。绑定失败保留原监听和运行配置；同端口更改 host 时短暂重绑，
失败恢复原监听。业务服务、已受理批量任务、并发限额和 Kiro 用户锁跨监听变更保留。
首次启用 REST 会等待目标监听生效，期间修改凭证也不会提前在旧地址开启；已经启用的
REST 可在监听重载失败后继续轮换令牌。旧 daemon 可能需要升级以支持热重载。

| 命令 | 作用 |
| --- | --- |
| `config show admin --effective` | 当前生效的管理配置 |
| `config show status-http --effective` | 当前生效的监听配置 |
| `config reset status-http` | 关闭监听和 REST，保留令牌与 socket |
| `config reset admin` | 清除令牌并重置 socket，保留监听 |
| `config reset` | 先备份再重置完整配置，清除令牌并关闭 REST/状态监听 |

关闭监听时同次编辑也要关闭 REST。修改 `admin.socket` 仍需重启；socket 重置默认值
取自 daemon 环境，旧 daemon 未提供默认值时保留当前路径。编辑器未改设置退出时不重载
provider/service；完整配置编辑失败时恢复原文件。

### 兼容手工摘要配置

旧式摘要继续兼容：

```toml
[admin.rest]
enabled = true
token_sha256 = "替换为完整权限令牌的64位SHA256十六进制摘要"
# read_token_sha256 = "替换为另一把只读令牌的64位SHA256十六进制摘要"
```

CLI 中显示为 `legacy-admin` / `legacy-read-only`，支持相同的令牌管理命令。
修改其中一把时仅将它转入 `[[admin.rest.tokens]]`，除本次变更外保留原身份、角色及状态，
另一把不受影响。

手工修改后重载配置。完整帮助：`crosslane help admin token`；启用指南：
`crosslane guide admin`。

## 账号管理

`provider` 始终指配置中的来源实例 ID，例如 `kiro`、`copilot-team-a`，不是任意的驱动类型名。
先通过 `/providers` 查询实例。账号路径使用 `/accounts/{provider}/{account}`，其中
`account` 推荐使用创建响应的 `id`；绑定账号时推荐使用 `provider/id`，避免不同来源重名。

| 方法与路径（相对于 `/admin/v1`） | 功能 |
| --- | --- |
| `GET /accounts` | 列表、筛选、分页与分组 |
| `POST /accounts` | 按 provider、认证方式、初始 tag 创建账号 |
| `GET /accounts/{provider}/{account}` | 账号状态、标签和额度 |
| `DELETE /accounts/{provider}/{account}` | 删除账号；仍被 service 使用时返回 `409`，`?force=true` 可自动解绑后删除 |
| `POST /accounts/{provider}/{account}/enabled` | `{"enabled":false}` 停用，`true` 启用 |
| `POST /accounts/{provider}/{account}/tags` | `{"add":["team-a"],"remove":["old"]}` 修改标签 |
| `POST /accounts/{provider}/{account}/refresh` | 刷新凭证及可获取的额度数据 |
| `POST /accounts/{provider}/{account}/probe` | 探测账号，可能产生上游请求和用量 |
| `POST /accounts/{provider}/{account}/reset-health` | 重置运行时健康状态 |
| `GET /accounts/{provider}/{account}/services` | 查询使用该账号的 service |

单账号路径中的 provider 必须是具体实例 ID；`all` 仅用于列表筛选。
删除成功时两种来源统一返回 `data.removed`（实际账号 ID）、`provider_id` 和 `reference`，
不返回已删除账号的原始 provider 元数据。

账号启停、标签、删除和 service 绑定操作使用本地快照定位账号及返回元数据。
账号/账号池查询、显式 refresh/probe 仍按原有逻辑访问上游；新增账号的鉴权也继续向
provider 校验。慢额度接口因此不会在这些本地变更期间占住配置事务。

### 创建与导入

创建 Kiro API Key 账号：

```bash
curl -X POST "$BASE/accounts" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"provider":"kiro","auth":"api-key","email":"account@example.com","token":"ksk_REPLACE_ME","tags":["team-a"]}'
```

创建 Copilot 账号时使用 GitHub token，会先向上游校验并交换 Copilot token，期间不持有配置锁。
如果热更新在校验期间替换了该 provider，创建返回 `409`，不保存账号；按当前 provider 配置重试即可。

```bash
curl -X POST "$BASE/accounts" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"provider":"copilot-team-a","auth":"token","token":"REPLACE_WITH_GITHUB_TOKEN","label":"team account","tag":"team-a"}'
```

`tags` 为字符串数组，`tag` 为单个标签，两者可同时提交并取并集；会去除首尾空白并去重。
首次持久化就包含标签，避免新账号短暂进入错误的 service 池。最多 64 个标签，每项 1–128
字符。Kiro 可选 `region`，默认 `us-east-1`；`machine_id` 自动生成，也可指定 64 位 hex。

Kiro 账号写入报错后会核对磁盘内容。如果完整变更已保存，仅旧增量文件清理失败，操作仍返回成功，
并记录 `account.persistence_committed_after_error`。部分写入会恢复到之前的账号版本；无法核实恢复成功时，
会明确返回错误。

Kiro OAuth 导入使用 `auth: "oauth"` 和以下 `credentials`，还可提供 `profile_arn`、
`machine_id`、`label`、`tags`。这不是交互式 OAuth 登录接口。

```json
{
  "provider": "kiro",
  "auth": "oauth",
  "email": "account@example.com",
  "tags": ["team-a"],
  "credentials": {
    "auth_method": "idc",
    "access_token": "REPLACE_ME",
    "refresh_token": "REPLACE_ME",
    "client_id": "REPLACE_ME",
    "client_secret": "REPLACE_ME",
    "region": "us-east-1",
    "expires_at": 2000000000
  }
}
```

`auth_method` 可为 `idc` 或 `social`，刷新能力取决于所导入凭证是否完整有效。
启用 `sso` feature 的构建还接受 `auth: "sso"`，配合 `email`、`password`、HTTPS
`start_url`、可选 `region` 和 `tags`；省略 `start_url` 时使用 `[sso]` 默认值。该操作
同步等待现有 SSO 流程，客户端应预留登录等待时间。无 `sso` feature 时返回 `501`。
新增或删除账号会写入既有账号存储并应用到运行时。

### 筛选与分组

查询示例：

```bash
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  "$BASE/accounts?provider=kiro&tag=team-a&group_by=tag&offset=0&limit=100"
```

可用筛选参数：`provider`、`tag`、`status`（健康状态）、`enabled_only`、`untagged_only`。
`sort` 支持 `name`、`id`、`credit`；缺省按显示名称自然排序。`untagged_only=true` 与
`tag` 不能同时使用。`offset` 默认 0，`limit` 默认 100，范围 1–1000。
响应包含唯一账号总数 `total`、`has_more`、`complete`、按来源记录的 `errors` 和 `groups`。
先对唯一账号分页，再按 `group_by=provider`（默认）或 `tag` 分组。多标签账号会出现在
每个对应标签组中；未打标签的组为 `tag: null`，分组计数之和可能大于唯一账号数。

## 批量增加与自动 SSO

`POST /account-batches` 支持批量创建账号。Kiro SSO 与
`crosslane account add --provider kiro --auth sso --batch accounts.csv` 使用同一个
CSV 解析器和 `account.addSso` 登录入口：自动启动浏览器完成 IAM Identity Center 登录，
交换凭证、校验上游身份和额度，通过后才持久化账号。无需提前手工取得 access token。
需要 `sso` feature 和与 CLI 相同的 Chromium 运行环境；租户要求人工处理的 MFA 等步骤仍遵循现有登录流程。

直接上传 CLI 使用的 CSV 文件即可，文件由客户端上传，不是服务器本地路径：

```csv
email,password
user1@example.com,REPLACE_ME
user2@example.com,"REPLACE_WITH_PASSWORD_CONTAINING,COMMA"
```

```bash
curl -X POST \
  "$BASE/account-batches?provider=kiro&auth=sso&tag=team-a&concurrency=4" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: text/csv' \
  --data-binary @accounts.csv
```

未指定入口时使用 `[sso].start_url`，与 CLI 一致。也可在查询参数中指定经过 URL 编码的
`start_url`，以及 `region`（默认 `us-east-1`）。支持 UTF-8 BOM、CRLF、可选表头、
逗号及双引号转义；密码中的空格原样保留。CSV 仍为 `email,password` 两列，公共标签
通过查询参数 `tag` 设置；多个公共标签可使用下方 JSON 形式。

JSON 也可以提交 `csv` 字符串，或者提交 `accounts` 数组，两者必须且只能提供一个：

```json
{
  "provider": "kiro",
  "auth": "sso",
  "start_url": "https://example.awsapps.com/start",
  "tags": ["team-a", "production"],
  "concurrency": 4,
  "accounts": [
    {"email": "user1@example.com", "password": "REPLACE_ME"},
    {"email": "user2@example.com", "password": "REPLACE_ME", "tag": "special"}
  ]
}
```

`accounts` 同时支持前文的 Kiro API Key、OAuth 凭据导入和 Copilot token 导入。
行内的 `provider`、`auth`、`start_url`、`region` 可以覆盖公共默认值；行内标签与公共
`tags` / `tag` 取并集。CSV 仅用于 Kiro `sso`，其他认证方式使用 JSON 数组。

| 方法与路径 | 功能 |
| --- | --- |
| `POST /account-batches` | 提交任务，返回 `202`、`data.id` 和 `Location` 查询地址 |
| `GET /account-batches` | 查询保留的任务摘要，不包含逐行结果 |
| `GET /account-batches/{batch}` | 查询进度、输入顺序的逐行结果、成功账号 ID 和引用、脱敏错误 |
| `POST /account-batches/{batch}/cancel` | 停止尚未开始的账号；已开始的登录完成当前操作并正常清理浏览器 |

```bash
curl -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  "$BASE/account-batches/batch_REPLACE_WITH_RETURNED_ID"
```

整份输入会先校验，格式或参数错误时不会开始任何登录。开始后单条账号失败不阻止后续
账号，成功账号保留，不整体回滚，与 CLI 批量处理一致。响应提供 `pending`、`running`、
`succeeded`、`failed`、`cancelled`、`completed` 计数；`rows` 保持输入顺序，包含
`account_id`、`reference`、逐行状态及错误。`complete=true` 仅表示所有行均成功完成。
结束状态为 `completed`、`completed_with_errors` 或 `cancelled`，处理中为 `queued`、
`running` 或 `cancelling`；可用 `finished_at` 判断任务是否结束。

每批 1–1000 个账号、最多 2 MiB，请求立即返回任务信息。默认并发 1，可配置 1–8；
REST 单账号创建与所有批量任务合计最多同时执行 8 条创建操作，最多同时接受 4 个未结束
批次，超限返回 `503`。任务报告最多保留 32 份，结束后最长保留一小时。
创建名额用尽时，单账号创建立即返回 `503`，避免占满管理请求名额；已接受的批量任务继续排队。
公共登录字段会在逐行复制前校验：`region` 为 1–64 字节且不含空白，`start_url` 最多
8192 字节（包括使用配置中的 SSO 默认地址时），避免大字段随账号行数放大内存占用。
创建/取消需要完整管理权限，任务查询允许只读令牌。

任务和待处理密码仅在内存中，报告和审计日志不保存 CSV、密码或 token。
daemon 重启会清空任务和待处理输入，不自动重放；已成功增加的账号及标签仍持久化保留。
客户端断开提交连接不会取消已接受的任务，可通过返回的 ID 继续查询。

## Service 管理

service 变更基于磁盘最新配置，保留尚未热重载的客户端 Key 修改，并同时校验当前运行
配置和磁盘目标配置。自动端口分配同时避开正在使用及尚未成功重载的状态监听端口。

| 方法与路径 | 功能 |
| --- | --- |
| `GET /services` | 服务列表，可按 `provider` 筛选 |
| `POST /services` | 创建业务监听及配套客户端 Key |
| `GET /services/{service}` | 服务配置、运行状态、分组账号池、各账号额度、客户端 Key 元数据 |
| `PATCH /services/{service}` | `{"enabled":false}` 停止监听，`true` 启动监听并持久化 |
| `DELETE /services/{service}` | 删除服务，清理该服务独占的客户端 Key；共用 Key 保留 |
| `GET /services/{service}/accounts` | 查询实际生效账号池 |
| `POST /services/{service}/accounts` | 增加手工绑定、清除相应排除项 |
| `DELETE /services/{service}/accounts` | 移除手工绑定并排除相应账号，不删除账号 |
| `GET /services/{service}/api-keys` | 客户端 Key 元数据，不返回明文 |

`service` 接受服务 ID 或名称。精确 ID 优先于其他 service 的同名名称；需要操作后者时使用
后者自己的 ID。CLI service 命令遵循同一规则，重命名后的编辑响应仍返回原来的稳定 ID。
写操作在配置事务内解析原始目标。账号池和 Key 查询会核对返回的 service ID；旧 ID 如果被解析成
另一个 service 的名称，会返回 `404`，避免返回其他 service 的数据。
详情与账号池查询支持 `group_by=provider|tag`，以及可选
`provider` 筛选。账号池准确遵循 service 的允许来源、标签、手工绑定及排除项。未设置
`account_tag` 的 service 只使用未打标签的账号，带标签账号只能通过匹配的标签或手工绑定加入；
设置多个标签时匹配其中任一标签。账号标签可以随时修改或移除，即使账号已被 service 使用，
新请求会按新标签重新计算账号池；删除仍被 service 使用的账号前需要先解绑。修改 service 标签
（包括运行中的 service）或账号标签时，如果会让某个启用中的 service 从有可用账号变为没有可用账号，
请求会被拒绝（REST 返回 `409`）；`POST /accounts/{provider}/{id}/tags` 传 `"force": true`、
CLI 的 `account tag`/`service edit`/`config edit`/`config reload` 加 `--force` 可强制应用。
直接编辑 `config.toml` 后由 SIGHUP 或文件监听触发的重载没有强制选项：被拒绝时，被拒绝的文件另存为
`config.toml.rejected-<毫秒时间戳>`，`config.toml` 恢复为当前运行配置，磁盘与运行配置始终一致。
确认后用 `crosslane config edit --force` 重新应用该修改。

Service 修改读取磁盘上的最新配置，保留待生效的客户端 Key 变更，并校验运行配置及重启后的
磁盘配置；自动分配端口会避开当前和待启用的状态端口。写入失败时会核对或恢复原文件，无法验证
回滚成功时会明确返回错误。

```bash
curl -X POST "$BASE/services" \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"team-a","host":"127.0.0.1","providers":["kiro"],"default_provider":"kiro","tags":["team-a"]}'
```

`providers` 是 `allowed_providers` 的别名，`tags` 是 `account_tag` 的别名；`account_tag`
也可接受单个字符串。省略端口时自动分配并避开启用的状态端口。还可指定 `port`、
`skip_user_agent_check`、`api_key_name`、`api_key_format`（`sk`、`simple`、`token`）。
创建成功返回 `201`，`data.service` 是服务信息，`data.api_key.key` 是新客户端 Key。
**该明文只在创建响应中返回，请保存好。** 后续查询不提供明文读取；管理 Bearer token
与这个客户端 Key 用途不同。监听创建或启用失败会回滚，不留下半创建的 service 或 Key。

绑定与解绑请求均使用以下 JSON：

```json
{"accounts":["kiro/acc_12345678","copilot-team-a/copilot_ACCOUNT_ID"]}
```

每次 1–1000 个引用，来源必须在该服务允许范围内。也可以提供顶层 `provider`，配合
未加来源前缀的账号 ID。解绑会写入排除列表，因此全局池或标签池不会立即把账号重新加回。
默认禁止删除被任何 service 使用的账号。传入 `?force=true` 可先自动从所有 service
解绑再删除，支持 Kiro 和 Copilot、全局池、标签池和手工绑定（包括停用的 service）。
服务排除记录会保留，防止同 ID 账号重新导入后自动加入这些服务；其他账号与服务保持原有配置。
删除失败时回滚解绑；磁盘故障导致回滚失败时会返回错误。此操作不取消已经开始的请求。

```bash
curl -X DELETE 'http://127.0.0.1:7651/admin/v1/accounts/kiro/acc_12345678?force=true' \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN"
```

CLI 等价命令为 `crosslane account rm acc_12345678 --provider kiro --force --yes`，
也可传多个账号；`--force` 只控制自动解绑，跳过确认仍需 `--yes`。
会导致账号离开已绑定标签池的标签修改仍需先调用 service 解绑接口。

## 额度含义

所有账号响应使用相同安全投影，不返回 access token、refresh token、client secret、
密码、机器标识或原始 provider details。`quota` 中未知数量为 `null`，不会冒充零余额。

| 来源 | `quota.unit` | `remaining` 的含义 |
| --- | --- | --- |
| Kiro | `kiro_credits` | 当前配置下有效总额度减去已用额度，最低为零；包含配置允许的超额额度上限 |
| Copilot 按 token 计费 | `ai_credits` | 上游个人额度，优先使用保留小数的 `quota_remaining` |
| Copilot 旧版配额 | `premium_requests` | 上游个人高级请求剩余次数 |

Kiro 查询读取本地用量缓存，`quota.updated_at` 是用量更新时间；可调用账号 `refresh`
或运行 `status_check` 任务更新。Kiro API Key 的 `refresh` 只更新额度，不尝试续期 Key。
Copilot 复用既有五分钟额度查询缓存，`updated_at` 是
最近额度查询时间，`cached=true` 表示查询失败后保留的旧快照。不限额或上游零占位数据
不会被解释成组织余额，`total`、`remaining` 保留 `null`。

Copilot 的 `local_aiu_usage` 是本机观察到的累计计费数据，与上游额度分开。
`total_nano_aiu` 保持整数精度（1 AIU = 1,000,000,000 nano AIU），同时给出
`billed_requests`、`first_recorded_at`、`last_recorded_at`；从未观察到计费数据时为 `null`。
接口不把不同计量单位相加。刷新或探测失败返回 `502`，不会将一次失败包装为成功；成功
动作的账号响应附带 `operation`，按实际情况包含 `refreshed`、`persisted`、`usage_refreshed`。

## 其他管理接口

| 分组 | 方法与路径 | 功能 |
| --- | --- | --- |
| Providers | `GET /providers`、`GET /providers/{provider}` | 配置中的来源实例、运行状态和能力 |
| Providers | `GET /models` | 模型目录，支持 `provider` 筛选 |
| Operations | `GET /runtime/status` | 完整管理状态，支持 `provider` 筛选 |
| Operations | `GET /runtime/stats` | 用量统计，支持 `provider`、`since_secs` 或 `start_secs` / `end_secs`、`detail` |
| Operations | `GET /runtime/tasks` | 后台任务状态 |
| Operations | `POST /runtime/tasks/{task}/run` | 手工运行既有任务，可用 `provider` 查询参数限定来源（任务自身支持时） |
| APIKeys | `GET /api-keys` | 客户端 Key 元数据与用量，支持 `provider` 筛选 |
| APIKeys | `POST /api-keys/{key}/reset-usage` | 按 Key ID 或名称清空该客户端 Key 的统计，不清空账号累计 AIU |

客户端 Key 同样优先匹配精确 ID，再按名称查找，适用于 REST 用量重置和 CLI 查询、修改、启停、
额度设置及删除。删除仅作用于选中的一把 Key，不会连带删除名称与该 ID 相同的另一把 Key。

任务名称由 `/runtime/tasks` 返回，例如 `status_check`、`token_refresh`、`model_cache_refresh`。
公开的 `/health` 仍只返回未打标签账号数量和各 service 的简要状态，不增加额度或 service ID。

## 响应与审计

成功响应为 `{"data":...}`；查询通常返回 `200`，单个资源创建返回 `201`，批量任务提交及
Kiro 订阅写入受理返回 `202`，以对应接口契约为准。OpenAPI 文档直接返回
标准文档对象。错误为 `{"error":{"code":"...","message":"..."}}`，使用 `400` 参数错误、
`401` 未认证、`403` 只读权限限制或 Kiro AWS 权限不足、`404` 资源不存在或管理接口关闭、`405` 方法不支持、
`409` 绑定或重复资源冲突、账号校验期间 provider 被替换，`413` 请求体过大、`502` 上游动作失败、`503` 暂时不可用、
`501` 构建不支持。上游错误文本沿用既有脱敏处理。

管理请求体最多 2 MiB，同时最多处理 16 个已认证请求，超出返回 `503`。
响应带 `Cache-Control: no-store` 和 `x-request-id: admin_...`。
审计日志事件为 `admin.http.request`，包含请求 ID、HTTP 方法、路由模板、权限角色、
状态码与耗时；不记录 Bearer token、请求正文、查询字符串或原始账号路径参数。

## 保存与排查 JSON 响应

导出时将 HTTP 响应头与正文分开保存。沿用上文的 `BASE` 和 `CROSSLANE_ADMIN_TOKEN`：

```bash
curl -sS --connect-timeout 10 --max-time 150 \
  --dump-header crosslane-response.headers \
  --output crosslane-response.raw \
  --write-out '\nHTTP %{http_code}\n' \
  -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Accept: application/json' \
  "$BASE/providers"
```

导出 Kiro 用户时将 URL 改为 `$BASE/kiro/enterprise/users?region=us-east-1`，并先配置
daemon 的 AWS 凭证。确认 curl 成功完成且 HTTP 为 200 后，再格式化并生成最终文件：

```bash
python3 -m json.tool --no-ensure-ascii crosslane-response.raw > crosslane-response.json.tmp \
  && mv crosslane-response.json.tmp crosslane-response.json
```

只有 JSON 解析成功才替换最终文件。失败时先查看：

```bash
cat crosslane-response.headers
head -c 2000 crosslane-response.raw
```

`Expecting value: line 1 column 1` 表示输入为空或不是 JSON，本身不能定位 HTTP 错误。
将 `curl -f` 直接管道给格式化工具可能隐藏 HTTP 错误正文；`-i` 会将响应头混进标准输出，
导出 JSON 应使用 `--dump-header` 与 `--output` 分开保存。401 表示 Admin token 缺失或
无效，000 表示未收到有效 HTTP 响应；其他错误应查看已保存的正文。
