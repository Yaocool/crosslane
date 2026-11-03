# Kiro 企业订阅管理

管理前缀：`http://127.0.0.1:7651/admin/v1/kiro/enterprise`。
复用 [Admin REST](admin-rest.zh-CN.md) 的独立监听、Bearer 鉴权和角色：
只读令牌可 GET/HEAD；开通、取消和升降级需要完整管理权限。

这里操作的是 AWS/Kiro 的企业用户与订阅。Crosslane 本地账号仍通过 `/admin/v1/accounts`
管理。开通订阅不会自动做浏览器 SSO，也不会把账号导入本地账号池。

## 配置 AWS 管理员凭据

### CLI 配置与热生效（推荐）

通过 daemon 的 Unix socket 配置，无需已有 Admin token：

```bash
crosslane admin kiro configure
crosslane admin kiro show
crosslane --json admin kiro show
```

`configure` 隐藏输入 AK、SK 和可选 STS SessionToken，再填写区域（默认 `us-east-1`）
与可选的 Profile ARN、Application ARN、Identity Center 区域。它会完整替换之前的凭证
和范围，省略的范围字段恢复默认。临时 `ASIA` 凭证必须同时提供 SessionToken。

自动化可使用 `crosslane admin kiro configure --stdin < /secure/kiro-admin-input.json`。
输入文件应限制为 `0600`，也可从凭证管理工具直接管道输入，不需要保存明文文件。JSON 格式：

```json
{
  "access_key_id": "<AccessKeyId>",
  "secret_access_key": "<SecretAccessKey>",
  "region": "us-east-1"
}
```

可选字段为 `session_token`、`profile_arn`、`application_arn`、`identity_center_region`。
不接受 AK/SK 命令行参数，避免进入 shell 历史和进程列表；错误信息不回显输入值。
`show` 和设置结果只返回是否配置、来源、区域、范围及 SessionToken 是否存在，不返回任何密钥。
“已配置”只代表本地格式检查通过，AWS 身份与权限需使用 `GET /profiles` 验证；`GET /plans`
是静态清单，不能验证凭证。
Profile ARN 会校验 CodeWhisperer 服务、区域、账号和 profile 资源结构；Application ARN
会校验 IAM Identity Center 的 application 结构，两者不能互换。格式参考
[AWS Profile ARN](https://docs.aws.amazon.com/service-authorization/latest/reference/list_codewhisperer.html)
和 [AWS Application ARN](https://docs.aws.amazon.com/singlesignon/latest/APIReference/API_DescribeApplication.html)。

写入结果是本次提交的配置快照；并发修改时，使用 `show` 查询最新状态。
若结果包含 `persistence_warning`，表示修改已经热生效，但目录同步失败，崩溃或断电后的
持久性无法确认。CLI 会显示提醒，JSON 模式保留该字段；应检查存储后重试原操作，
不能把它当成“修改未生效”。普通 `show` 只查询状态，不确认此前写入的持久性。

保存成功后，后续 Kiro 管理请求立即读取新凭证，不需要 `config reload`、重启 daemon 或重建
容器；已受理的 AWS 操作继续使用其原凭证快照。业务服务、账号池与 REST token 保持原状态。

凭证存放在 **daemon 数据目录**的 `.secrets/kiro-admin.enc`，使用 AES-256-GCM 加密；
同目录的 `kiro-admin.key` 保存随机本机加密密钥。目录权限 `0700`、文件权限 `0600`，校验
属主并拒绝符号链接、硬链接及宽松权限。macOS 还会检查扩展 ACL，拒绝额外 ALLOW
授权（即使 mode 显示为 0700/0600），仅限制权限的 DENY 条目可保留。若目录继承了额外授权，
需要先检查并移除这些授权；CLI 不会自动覆盖已有 ACL 或修改父目录的访问策略。文件损坏或权限异常时拒绝管理操作，不回落旧环境变量。
这些文件不写入 `config.toml`、普通配置导出、账号导出、REST 响应或日志，也不受 `config reset`
修改。原生默认目录为 `~/.local/share/crosslane/.secrets`（遵循 `XDG_DATA_HOME`）；设置
`CROSSLANE_HOME` 后位于其 `.secrets` 子目录。Docker 默认在持久卷 `/var/lib/crosslane/.secrets`，
CLI 通过 socket 修改容器中的文件，不会在调用端保存副本。

备份时须同样保护加密文件和本机密钥，同时持有两者即可解密。密钥丢失时不能解密旧凭证，
需恢复本机密钥或重新执行 `configure` 完整设置。

```bash
# 移除当前凭证并持久化停用，阻止旧环境变量重新生效
crosslane admin kiro clear

# 明确恢复 daemon 启动环境中的旧配置；存在无效环境凭证时拒绝切换
crosslane admin kiro use-env
```

文件凭证优先于环境变量，整组选择，不混用 AK、SK、SessionToken 或范围。`clear` 保留加密
停用标记，重启后仍停用；`use-env` 写入显式环境来源标记，再使用该进程继承的环境变量。

### 兼容环境变量配置

以下方式继续兼容，仅在未设置文件凭证或显式选择 use-env 时生效。CLI 管理凭证时可将 `.env` 中这些字段留空。

把以下变量注入 **crosslaned 进程**，然后启动或重启该进程。容器部署时必须注入容器环境，
仅在宿主机 CLI 的 shell 中 export 不会改变已经运行的守护进程。

```bash
export CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID='替换为管理员AccessKeyId'
export CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY='替换为管理员SecretAccessKey'
export CROSSLANE_KIRO_ADMIN_REGION='us-east-1'

# 临时 STS 凭据必须提供；长期 AK/SK 可以不设置或留空。
export CROSSLANE_KIRO_ADMIN_SESSION_TOKEN='替换为临时SessionToken'

crosslaned
```

环境凭据不写入 Crosslane 的 TOML、账号库或 REST 响应；不会读取默认 AWS profile、
`AWS_ACCESS_KEY_ID`、SSO 缓存或 EC2/ECS 凭据链。请求不接收 AK/SK，调用方只携带 Admin token。
AWS 凭据缺失、不完整或环境变量编码无效时，远程查询/写入返回 `503 kiro_admin_not_configured`；
`GET /plans` 不需要 AWS 凭据。修改部署环境后应重启守护进程；`config reload` 不会更新进程环境。

| 环境变量 | 用途 |
| --- | --- |
| `CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID` | 必填，拥有 Kiro Profile 的 AWS 账号管理员 AK |
| `CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY` | 必填，SK |
| `CROSSLANE_KIRO_ADMIN_SESSION_TOKEN` | 临时 STS 凭据必填；长期密钥可不设置或留空 |
| `CROSSLANE_KIRO_ADMIN_REGION` | Kiro Profile 区域，默认 `us-east-1` |
| `CROSSLANE_KIRO_ADMIN_PROFILE_ARN` | 可选，固定 Profile；多个 Profile 时需要显式选择 |
| `CROSSLANE_KIRO_ADMIN_APPLICATION_ARN` | 可选，选择 Profile 关联的 Identity Center 应用 |
| `CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION` | 可选，仅在 Profile 缺少 `ssoRegion` 时补充，不能覆盖 Profile 已记录的其他区域 |

仓库默认 `docker-compose.yml` 已映射上述全部变量；在宿主机 `.env` 中按 `.env.example`
填写即可。自定义 Compose 文件需要在 `crosslaned` 服务中保留相应映射，例如：

```yaml
services:
  crosslaned:
    environment:
      CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID: ${CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID:-}
      CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY: ${CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY:-}
      CROSSLANE_KIRO_ADMIN_REGION: ${CROSSLANE_KIRO_ADMIN_REGION:-us-east-1}
      CROSSLANE_KIRO_ADMIN_SESSION_TOKEN: ${CROSSLANE_KIRO_ADMIN_SESSION_TOKEN:-}
      CROSSLANE_KIRO_ADMIN_PROFILE_ARN: ${CROSSLANE_KIRO_ADMIN_PROFILE_ARN:-}
      CROSSLANE_KIRO_ADMIN_APPLICATION_ARN: ${CROSSLANE_KIRO_ADMIN_APPLICATION_ARN:-}
      CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION: ${CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION:-}
```

可选环境变量为空字符串时按未设置处理；临时 `ASIA` 凭据仍必须有非空 SessionToken。
查询参数中显式传入的空 Profile/Application ARN 仍会被拒绝，不会静默改选默认目标。

修改 Compose 的 `.env` 或环境映射后用 `docker compose up -d --force-recreate crosslaned` 重建该服务容器。
[`docker compose restart` 不会更新环境变量](https://docs.docker.com/reference/cli/docker/compose/restart/)。

## 接口

下表路径相对于 `/admin/v1/kiro/enterprise`。

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/plans` | `pro`、`pro-plus`、`pro-max`、`power` 与 AWS 套餐名 |
| GET | `/profiles` | 验证 AWS 调用身份并列出该账号、该区域的 Kiro Profile |
| GET | `/scope` | 自动发现并验证 Profile、Identity Center 实例、应用和身份库 |
| GET | `/users` | 列出身份库全部用户，附直接 USER 订阅；应用的 GROUP 订阅另列 |
| GET | `/subscriptions` | 当前应用全部 USER/GROUP 订阅，包括当前套餐与待生效套餐 |
| GET | `/users/{user}/subscription` | 指定用户的直接订阅和当前组成员关系继承的订阅 |
| POST | `/users/{user}/subscription` | 开通直接 USER 订阅；已有直接订阅返回 409 |
| POST | `/users/{user}/subscription/upgrade` | 严格升档 |
| POST | `/users/{user}/subscription/downgrade` | 严格降档 |
| DELETE | `/users/{user}/subscription` | 取消直接 USER 订阅，保留 Identity Center 用户 |

`{user}` 接受精确 `UserId`（含身份库前缀的 UUID）、商业分区的 Identity Store 用户 ARN，
或精确 `userName`；不按显示名模糊匹配。ARN 放入 URL 路径时需做 URL 编码（尤其是 `/`）。
用户、组和身份库的合法 ARN/UUID 表示会先规范化为裸 ID，再做绑定、继承匹配与身份库校验。
发现身份库所属账号后，还会核对返回 ARN 的账号，避免仅凭相同资源尾部接受错误范围。
错误资源类型、其他服务或分区的 ARN 不会被当作用户名查询；实际订阅写入仍只发送裸用户 ID。
用户名按 Unicode 字符数校验，最多 128 个字符；空白、控制字符和超长值在请求 AWS 前拒绝。
返回的用户和组成员关系必须属于发现的身份库；提供了成员 ID 时，还必须对应目标用户。
`/users` 不逐个展开组成员关系；指定用户的订阅详情才会查询其组成员关系。
所有清单最多 100 页、10,000 项、累计 16 MiB 上游响应，单页最多 4 MiB；分页异常或超限会失败，
不会把不完整清单作为成功返回。每个请求的 AWS 处理预算为 120 秒，每次 HTTP 请求最多 40 秒。

可选查询参数 `region`、`profile_arn`、`application_arn`、`identity_center_region` 覆盖对应
凭证文件或环境变量中的默认选择。只在唯一候选时自动选择；有多个候选返回 409，不任取第一个。
传入的应用必须属于选中 Profile，返回的应用实例必须匹配 Profile 的 Identity Center 实例。
Profile 的区域及所属 AWS 账号必须与当前请求和 STS 调用身份一致。

当前实现支持商业 AWS 分区；不支持中国区或 GovCloud。`legacy=true` 显式启用旧控制面，
仅支持 `us-east-1`，且只能立即取消；默认不使用旧控制面或失败后自动切换。

## 调用示例

调用方只设置 Crosslane 的 Admin token，不需要 AWS AK/SK：

```bash
BASE=http://127.0.0.1:7651/admin/v1/kiro/enterprise
ADMIN_TOKEN='替换为Crosslane管理令牌'
USER_ID='替换为IdentityCenter的UserId'

# 发现范围、列举用户和订阅
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/profiles"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/scope"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/users"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/subscriptions"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/users/$USER_ID/subscription"

# 开通预览：执行只读发现与校验，不提交订阅写入
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"plan":"pro-plus","dry_run":true}' \
  "$BASE/users/$USER_ID/subscription"

# 实际开通，不额外创建 Identity Center 应用分配
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-plus"}' \
  "$BASE/users/$USER_ID/subscription"

# 将较低档位升级为 pro-max
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-max"}' \
  "$BASE/users/$USER_ID/subscription/upgrade"

# 将较高档位降级为 pro
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro"}' \
  "$BASE/users/$USER_ID/subscription/downgrade"

# 月底停止访问；省略 mode 时也是 end_of_period
curl -fsS -X DELETE -H "Authorization: Bearer $ADMIN_TOKEN" \
  "$BASE/users/$USER_ID/subscription?mode=end_of_period"

# 立即停止直接订阅的访问；订阅计费仍到月底结束
curl -fsS -X DELETE -H "Authorization: Bearer $ADMIN_TOKEN" \
  "$BASE/users/$USER_ID/subscription?mode=immediate"
```

三个 POST 写操作均支持请求体 `dry_run=true`；DELETE 支持查询参数 `dry_run=true`。
预览返回 `200` 和 `status=preview`，实际写入接受返回 `202` 和 `status=api_accepted`。
两者都需要完整管理令牌；响应中的 `application_assignment_requested` 固定为 `false`。
不接受 `assign_application` 等额外请求体字段。

订阅写操作及预览最多同时运行 8 个，满额时返回 `503 busy`。同一 AWS 账号、区域和解析后
用户 ID 的重叠操作返回 `409`，用户名和 UUID 别名共用保护；不同用户可以并发。
客户端断线不会取消已开始的操作：后台在既定 AWS 超时内继续等待，保留并发名额与用户锁。
结束时记录 `admin.kiro.subscription.completed`，保留服务端请求 ID、管理令牌 ID、角色、
操作、状态、错误码及可用的 AWS 请求 ID，不记录令牌明文、凭据或请求体。
后台执行发生可捕获的 panic 时也会输出完成审计，结果按 `unknown` 处理，不回传 panic 内容。
日志的 `write_outcome` 统一为 `accepted`、`not_submitted`、`rejected` 或 `unknown`。
该互斥只保护当前 Admin REST 实例，不能对 AWS Console 等外部写入提供原子条件更新；
`202` 后应先核验当前/待生效套餐再提交后续变更。

## 订阅语义与错误处理

按 [Kiro 官方管理规则](https://kiro.dev/docs/enterprise/subscription-management/)，升级通常立即
生效，降级次月生效；取消可选择月底停止访问或立即停止访问，两种方式的订阅均到月底结束。
取消 USER 直接订阅不会移除组继承权益。这里只取消订阅，不删除 Identity Center 用户、
Crosslane 本地账号，也不添加/删除直接应用分配。

升级与降级必须有唯一直接 USER claim，并能识别当前档位；未知套餐、多个直接 claim、
目标档位与操作方向不一致时，写入前返回 409。`subscription_type` 与 `plan` 是当前套餐，
`pending_subscription_type` 与 `pending_plan` 是待生效套餐；不能把待生效档位当作已生效档位。

所有写入只提交一次，不自动重试、跟随重定向或回退到其他控制面。`202` 只证明上游接受
请求，实际套餐、访问和计费需要通过订阅查询或 AWS Console 核验。上游操作错误包含
`operation`、`upstream_status`、经筛选的 `upstream_code`、`upstream_request_id` 和
`write_outcome`，不返回上游原始错误正文、凭据或签名。
HTTP 2xx 中的 AWS 错误信封同样按失败处理，无法确认的错误体标记为 `unknown`。
响应体中途断开时，保留已经收到的上游 HTTP 状态与请求 ID，便于回查。

- `not_submitted`：预检失败，未提交订阅写入。
- `rejected`：上游明确拒绝写入。
- `unknown`：写入时超时、传输异常、服务端失败、异常重定向或响应无法解析，结果可能已经生效；先查询，
  不要直接重复写入。

权限依据见 [Kiro IAM](https://kiro.dev/docs/enterprise/iam/)：
订阅写入需要 `q:CreateAssignment`、`q:UpdateAssignment`、`q:DeleteAssignment`，查询需要
`user-subscriptions:ListApplicationClaims`；发现还使用 `codewhisperer:ListProfiles`、
`sso:DescribeInstance`、`sso:DescribeApplication`、`identitystore:ListUsers`、
`identitystore:GetUserId`、`identitystore:DescribeUser`、`identitystore:ListGroupMembershipsForMember`
以及 STS 身份查询。具体资源权限、SCP 和跨账号约束仍由 AWS 决定。
Crosslane 不需要 `sso:CreateApplicationAssignment` 或 `sso:DeleteApplicationAssignment`。

Kiro 的套餐操作及 claim 查询采用 Console 后端协议，并非稳定公开的 AWS SDK 服务模型；
参考 [Kiro 官方仓库的调用记录](https://github.com/kirodotdev/Kiro/issues/10166)。
所有 AWS 请求使用 SigV4，目标域名由已验证的区域生成，不接受调用方指定任意 endpoint。
该实现通过本地模拟协议测试；未在真实 AWS 账号上执行开通、取消或计费变更验证。
