# 多提供源与 GitHub Copilot 接入

`kproxyd` 现在通过 provider registry 管理模型来源。未配置 `[[provider]]` 时仍自动创建
`kiro`，已有配置和请求行为不变；一旦显式配置 provider，需要把仍要使用的 Kiro 实例也保留为
`id = "kiro"`、`kind = "kiro"`。内置 Kiro 的实例 ID 固定为 `kiro`，Copilot 可以创建多个实例，
例如 `copilot-personal`、`copilot-team`，各自拥有独立账号文件、token 缓存、并发限制和模型缓存。

## 命令与配置的适用范围

`kproxy guide provider` 按命令列出来源边界；`kproxy config list` 的“适用来源”列按配置模块标注范围。
全局配置无需为两种来源各建一份。

| 范围 | 命令 | 配置 |
| --- | --- | --- |
| 两者共用 | `provider`、`status`、`ready`、大部分 `account` 操作、`pool`、`models`、`model-map`、`service`、`apikey`、`stats`、`logs show/follow` | `[[provider]]`、`[[model_mapping]]`、`[[proxy_service]]`、`[[api_key]]` 通过实例 ID 或允许来源限定 |
| Kiro 专项 | `account add-sso/add-api-key/import/overage/regen-machine-id`、`diagnose`、订阅详情、`alert` 的当前事件、`pool --explain` 评分 | `[upstream]`、`[pool]`、`[features]`、`[models]` 的发现/缓存配置、`[context]`、`[storage]`、`[sso]`、`[model_thinking_mode]`、当前 `[notify]`/`[[webhook]]` 告警 |
| 来源专用认证 | 统一使用 `account add`；Kiro 的 `--auth sso/api-key`，Copilot 的 `--auth device-flow` 或 `--token-stdin` | Copilot 配置位于对应实例的 `[provider.settings]`、`[provider.pool]`、`[provider.models]`、`[provider.routing]` |
| 全局 | `health`、`version`、`config` 管理、`logs trace/files/path`、服务进程生命周期 | `[server]`、`[log]`、`[admin]`、`models.disabled` |

`tasks` 属于混合范围：`token_refresh/status_check/health_recheck` 只操作 Kiro；
`model_cache_refresh` 可使用 `--provider <ID>`；统计持久化等任务是全局的。
`pool` 默认聚合全部来源；`pool --provider copilot` 不带 `--model` 时检查账号是否有可用模型，
可用 `--model <ID>` 缩小范围。仅 `pool --provider kiro` 提供 Kiro 的三因子评分。
`service accounts/add-account/remove-account` 与 `--account-tag` 均支持 Kiro/Copilot 账号池；
服务与账号仍受允许的实例 ID 范围限制。
`model-map --below-credits-percent` 也仅适用于 Kiro。

## 快速配置 Copilot

`provider list` 的“实例 ID”用于 `--provider`（简写 `-p`）；“类型 (kind)”只用于驱动选择。
例如实例 ID 为 `work-cp`、kind 为 `copilot`，后续命令必须用 `-p work-cp`。
`provider show work-cp` 会给出该实例的下一步命令；`guide copilot -p work-cp` 可生成对应示例。
也可 `export KPROXY_PROVIDER=work-cp` 省略重复参数，显式 `-p` 优先；全局和按资源 ID
定位的操作不使用这个默认值。`help --all` 和各层 `--help` 都标明操作范围。

先添加 provider。此命令创建一个 Copilot 模型来源实例（账号池、模型发现和路由的配置范围），
不会添加或授权 GitHub 用户账号。新建实例会将默认公开 OAuth Client ID 写入配置文件；
用 `kproxy config path` 查看配置文件位置：

```bash
kproxy provider add \
  --id copilot \
  --kind copilot \
  --max-concurrent-per-account 2

kproxy provider show copilot
kproxy config path
```

新增的实例包含可直接编辑的设置：

```toml
[[provider]]
id = "copilot"
kind = "copilot"
enabled = true

[provider.settings]
client_id = "Iv1.b507a08c87ecfe98"
```

如果企业组织只允许自有 OAuth App，可通过 `--setting 'client_id="Iv1.your-app"'` 覆盖默认值；
已有实例使用 `kproxy provider edit copilot --setting 'client_id="Iv1.your-app"'`。
要恢复默认值且仍让它显示在配置文件中，可用
`kproxy provider edit copilot --setting 'client_id="Iv1.b507a08c87ecfe98"'`。
旧配置未写 `client_id` 时仍会使用相同的运行时默认值；如需显式显示，也可执行上述命令。
`--setting` 接受 `KEY=TOML_VALUE`，字符串值需保留 TOML 引号。
Client ID 与已签发的 OAuth token 关联；如果已有账号通过自有应用登录，不要直接切换或移除该设置，
否则其 refresh token 可能无法继续轮换，需要重新授权账号。

也可以跳过 Device Flow，从标准输入导入已有 GitHub token，避免凭证出现在命令行参数和 shell 历史中：

```bash
kproxy account add --provider copilot --auth device-flow

# 或者
printf '%s\n' "$GITHUB_TOKEN" | \
  kproxy account add --provider copilot --token-stdin
```

`--provider` 是实例 ID：本页示例创建的是 `copilot`，后续也使用 `--provider copilot`；
自定义为 `work-cp` 时改用 `--provider work-cp`，`--kind copilot` 始终表示驱动类型。

不提供浏览器凭证时，Device Flow 会在终端显示授权网址和验证码。用户在自己电脑的
无痕/隐私窗口登录并授权，CLI 保持轮询。添加多个用户时，关闭上一用户的所有无痕窗口再继续。

### 在远端无头 Chromium 中完成授权

full 镜像或启用了 `sso` feature 的 daemon 可以在服务器上启动独立无痕 Chromium，
填写账号、进入 Azure SSO、输入 Device Flow 验证码并确认授权。无需本地浏览器、X11、VNC
或浏览器回调端口。每个登录任务使用临时 profile 和独立 CDP browser context，完成、失败、
取消或过期后关闭并清理；同时最多运行两个 Copilot 授权浏览器。
此模式需要同时更新 CLI 和 daemon。新 CLI 遇到旧 daemon 时会报认证方式不支持，
不会忽略提供的凭证并悄悄退回本地浏览器授权。

交互添加（命令中的 provider ID 按实际配置填写）：

```bash
kproxy account add --provider copilot --auth device-flow --headless
```

在终端填写完整 GitHub 用户名，选择 Azure SSO、GitHub 密码或两套凭证；密码输入不回显。
也可以提前传 `--username` / `--sso-username`，其余凭证按提示填写。Docker wrapper 会保留终端，
无需把密码写在命令行参数中。

CSV 批量添加按顺序逐个完成远端无痕登录，先校验整个文件，遇到账号错误记录后继续，Ctrl-C 终止剩余任务：

```bash
chmod 600 /secure/copilot.csv
kproxy account add --provider copilot --batch /secure/copilot.csv
```

```csv
github_username,github_password,sso_username,sso_password,label
USER_SHORTCODE,,user@example.com,azure-password,enterprise
personal-user,github-password,,,personal
```

仅 `github_username` 是必选列，凭证需提供 `github_password` 或 `sso_username` + `sso_password`。
还支持可选 `sso_start_url` 列；`--sso-start-url` 会覆盖各行入口，行内 `label` 优先于 `--label`。
CSV 支持逗号和双引号转义，不支持多行字段；密码中的空格会保留。`--batch -` 从 stdin 读取，
Docker wrapper 可自动将宿主机 CSV 通过 stdin 传入容器。`--json` 最终输出包含 accounts、errors、
complete、cancelled 和 skipped 的单个结果，进度输出到 stderr。

Enterprise Managed User 非交互示例：

```bash
printf '%s\n' "$AZURE_PASSWORD" | kproxy account add \
  --provider copilot --auth device-flow \
  --username USER_SHORTCODE --sso-username user@example.com --password-stdin
```

`--username` 必须是完整的 **GitHub 用户名**（EMU 通常包含企业后缀），用于登录及最终账号校验。
`--sso-username` 是 Azure 登录名，可以与 GitHub 用户名不同。指定它时，stdin 密码只填写到
微软的 SSO 页面，不会尝试作为 GitHub 密码。普通 GitHub 账号密码登录可省略 `--sso-username`。
默认直接从 GitHub Device Flow 页面登录，由 GitHub 识别企业用户名并跳转到身份提供商，
不要求事先配置 Azure SSO URL。
需要先从组织或企业 SSO 入口登录时，可增加
`--sso-start-url https://github.com/enterprises/ENTERPRISE/sso`，也支持同源的 `/orgs/ORG/sso`。

GitHub 个人账号和 Azure 各有一套密码时，使用 `--credentials-stdin`，从受保护的文件或程序管道
输入以下 JSON。不要把密码写进命令参数、provider 配置或版本库：

```json
{
  "github_username": "your-github-login",
  "github_password": "your-github-password",
  "sso_username": "user@example.com",
  "sso_password": "your-azure-password",
  "sso_start_url": "https://github.com/orgs/ORG/sso"
}
```

```bash
kproxy account add --provider copilot --auth device-flow \
  --credentials-stdin < /secure/copilot-login.json
```

凭证仅在本次登录期间驻留于 daemon 内存，不写入账号文件或普通日志。Azure 页面之外不填写
Azure 密码；暂不支持的外部联邦身份提供商会明确报错。MFA 推送需要本人在手机批准；需要动态
验证码时，原命令会显示任务 ID，可在另一个远端终端输入：

```bash
printf '%s\n' "$MFA_CODE" | kproxy account login-code \
  --provider copilot --task login_TASK_ID --code-stdin
```

CAPTCHA、Passkey、安全密钥、强制改密和受管设备要求不会被自动跳过。组织策略要求这些步骤时，
应使用符合要求的登录方式；复制本地浏览器的 header 不能替代身份验证。

若 Azure 显示“无法立即访问此资源”或 `AADSTS53003`，这是条件访问拒绝，不代表密码错误。
登录任务会停止，避免重复提交密码直到超时。请把错误页的时间、Request ID、Correlation ID
交给 Entra 管理员，在 **Entra ID → Monitoring & health → Sign-in logs → Conditional Access**
查看具体失败策略。跳转至 `mysignins.microsoft.com` / `registerMfaMethods` 时，还需检查
**Register security information** 用户操作相关策略；首次 MFA/安全信息注册须按组织要求完成，
不会由代理自动绑定或修改验证方式。设备显示 `Unregistered` 只是诊断线索，不能据此判断具体策略。
是否允许远端无头无痕登录取决于组织的设备、网络、认证强度和应用要求。
参考：[Microsoft 条件访问排障](https://learn.microsoft.com/en-us/entra/identity/conditional-access/troubleshoot-conditional-access)、
[安全信息注册策略](https://learn.microsoft.com/en-us/entra/identity/conditional-access/policy-all-users-security-info-registration)。

排查时可在账号添加命令增加 `--capture-headers`。浏览器导航之前即开始记录请求、响应、重定向和
CDP `requestWillBeSentExtraInfo` / `responseReceivedExtraInfo`，包括实际发送的 Cookie 和其它请求头。
记录位于服务器 `$KPROXY_HOME/providers/<ID>/login-traces/<任务ID>.headers.jsonl`，目录权限 `0700`、
文件权限 `0600`。它包含可用的会话凭证，不记录密码表单正文，不进入普通日志；仅在诊断期间开启。
Cookie、CSRF token、nonce、Origin、Referer 和客户端提示由当前远端浏览器及页面自行生成，
不把一次本地登录的 header 固定重放到另一会话。

Device Flow 会先取得 GitHub 用户 token，再通过
`GET /copilot_internal/v2/token` 交换短期 Copilot API token。动态返回的 API endpoint 会校验
HTTPS 与主机名。账号 token、刷新 token 和探测结果保存在
`$KPROXY_HOME/providers/<provider-id>/accounts.json`，使用原子写入和 `0600` 权限；逐账号模型目录
持久化到同目录的 `models.json`，成功返回空目录时也会清除旧缓存，失败时只在配置的 stale 窗口内
使用最后一次成功结果。

GitHub.com 上通过 Azure SSO 登录、并逐用户分配 Copilot Enterprise 席位的场景，不需要配置
`account_type`：Device Flow 的浏览器页面完成组织要求的 SSO 后，代理按每个
账号的 token 响应中的 `endpoints.api` 选择 Copilot API 域名。若响应给出
`*.enterprise.githubcopilot.com`，该域名已在默认允许范围内；即使同一个 provider 下有不同套餐的账号，
也不会因全局 `account_type` 而把它们固定到同一个域名。如果 token 响应缺少 `endpoints.api`，
默认回退到 `https://api.githubcopilot.com`。仅当企业网络要求使用套餐专属域名、且响应确实缺少
该字段时，可以为企业专用 provider 配置回退地址：

```bash
kproxy provider edit copilot \
  --setting 'api_endpoint_fallback="https://api.enterprise.githubcopilot.com"'
```

该配置只在响应缺少 `endpoints.api` 时生效，不覆盖 GitHub 返回的地址。`allowed_endpoint_hosts`
是额外允许的主机列表，不会限制默认允许的 GitHub Copilot 域名。若账号实际位于独立的
`*.ghe.com` 数据驻留站点，则需另外配置 GitHub/OAuth 地址及对应的 endpoint 主机；Azure SSO
本身不代表使用 `ghe.com`。

Device Flow 签发的 expiring user token 可使用返回的 refresh token 轮换，GitHub 对该流程不要求
`client_secret`。只有自有 OAuth 应用确实要求时，才应与其 `client_id` 成对配置：

```bash
kproxy provider edit copilot \
  --setting 'client_secret="replace-me"'
```

生产环境不要启用 `allow_insecure_http`。独立的 GitHub Enterprise 站点或自建测试端点可配置
`github_host`、`github_api_base`、`oauth_base` 和 `allowed_endpoint_hosts`；完整字段和默认值见
`kproxy config show provider` 或生成的默认配置文件。

adapter 会发送 Copilot 要求的 `Editor-Version`、`Editor-Plugin-Version`、
`Copilot-Integration-Id` 和 `User-Agent` 身份头。Claude 原生请求只转发已经验证可用的 beta，
并移除 Copilot 明确拒绝的 `fallbacks`、`container`、`mcp_servers`；图片请求自动添加 vision
标记，Chat 流式请求自动请求最终 usage 事件。

## 创建单来源或多来源服务

创建 Copilot-only 服务时，服务与自动生成的 API key 会使用相同 provider 范围：

```bash
kproxy service create \
  --name copilot \
  --host 127.0.0.1 \
  --port 5581 \
  --provider copilot \
  --default-provider copilot
```

一个监听也可以同时开放 Kiro 和 Copilot：

```bash
kproxy service create \
  --name unified \
  --host 127.0.0.1 \
  --port 5580 \
  --provider kiro \
  --provider copilot \
  --default-provider kiro
```

多来源服务的 `GET /v1/models` 会返回 `kiro/<model>`、`copilot/<model>` 形式的 ID，避免不同
来源的同名模型冲突。请求也可显式使用该前缀；未带前缀时使用服务的 `default_provider`。
provider 范围同时受服务和 API key 限制，取两者交集。API key 还可限制模型 glob：

```bash
kproxy apikey edit <KEY_ID> \
  --provider copilot \
  --model 'copilot/claude-*' \
  --model 'copilot/gpt-*'
```

模型权限按映射、别名解析与回退后的最终模型判断；受限 API key 不能借助 Kiro 的账户级映射、默认模型或容量回退访问白名单外模型。

## 协议和模型映射

Copilot 的所有非 embedding 生成模型均提供以下三种接口，支持 JSON 和 SSE：

- `POST /v1/messages`
- `POST /v1/chat/completions`
- `POST /v1/responses`

根据选中账号的模型目录选择上游接口；有原生能力时优先透传，否则在三种协议间转换。
公开目录的 `protocols` 表示代理提供的接口，`capabilities.native_protocols` 保留上游原生能力；
账号内部目录仍按真实原生能力调度。embedding 模型不出现在生成模型列表中。

转换覆盖完整会话、system/developer、函数工具及并行结果、图片/内联文件、文本和思考流、
结束原因、usage/cache 与原始 `copilot_usage`。SSE 分片、UTF-8、工具索引及终止事件均增量处理；
中途断流或上游错误不会补发成功结束。`count_tokens` 对 Messages 原生模型使用原生接口，
其他模型返回本地估算，并以 `estimated: true` 和 `x-kproxy-token-count-source: estimate`
标记；估算不作为上游生成用量计费。

协议转换不会为模型增加视觉、推理或托管工具能力。需要转换时，Responses
`previous_response_id`/`conversation`、后台任务、托管工具和远端文件 ID 等缺少执行链路的
能力会明确报错；客户端应发送完整会话。原生接口保留其原有字段与能力。
结构化输出、缓存提示和思考控制的效果取决于选中的模型；转换不承诺完整托管 API 等价。
具体边界与参考项目见[协议兼容说明](protocol-compatibility.zh-CN.md#copilot-三协议转换)。

模型映射共用同一套规则引擎，可以按 provider、代理服务和 API key 限定：

```bash
kproxy model-map add \
  --name copilot-fast \
  --provider copilot \
  --service svc_example \
  --source team-fast \
  --target gpt-5-mini

kproxy model-map test team-fast \
  --provider copilot \
  --service svc_example
```

旧规则没有 `providers` 时只作用于 Kiro，新增 Copilot 不会意外继承旧映射。跨 provider 的映射
需要在源 provider 上显式开启，再把目标写成 `provider/model`：

```bash
kproxy provider edit kiro --allow-cross-provider-fallback true
kproxy model-map add \
  --name kiro-to-copilot \
  --provider kiro \
  --source team-copilot \
  --target copilot/gpt-5-mini
```

跨 provider 规则会在账号调度前完成一次目标选择，包括同时含本地与跨源目标的
`loadbalance`。`max_remaining_credit_percent` 依赖已经选中的 Kiro 账号，因此只能映射到
Kiro 本地模型；配置成跨 provider 目标会在配置校验时被拒绝，避免把 provider 前缀误发给
Kiro 上游。

`[provider.routing].default_model_id` 是该来源没有命中显式规则时的默认目标。Kiro 仍保留原有的
账号额度条件、动态模型解析，以及 `enable_model_fallback` 控制的上游容量错误同族 fallback。
Copilot 映射在 provider adapter 前执行，按服务账号池和账号的原生模型目录过滤，再选择原生
接口或转换路径。转换不会绕过账号模型权限，也不会对上游容量错误自动更换模型；仍需使用
显式映射或默认目标控制该行为。

## 统一和按来源管理

统一采用“资源 + 动作 + provider 范围”：`account`、`service`、`models`、`apikey`、`stats`
等共用一棵命令树。`--provider` 可放在根命令、命令组或最终子命令上，也可通过
`KPROXY_PROVIDER` 设置当前 shell 的默认范围；显式参数优先于环境变量。
编辑已有 service、API key 或模型映射时，只有显式 `--provider` 才会替换来源权限，
环境变量不会隐式修改权限；该参数与 `--clear-providers` 互斥。

列表默认聚合来源，单账号操作按 ID/邮箱/登录名自动定位；有重名时使用
`<provider>/<account>` 或 `--provider` 消歧。`account export` 为保持旧导出格式，省略
范围仍导出 Kiro；全量导出使用 `--provider all`。`refresh/probe/reset-health --all` 和
`pool` 默认范围为全部 provider，可用 `--provider kiro` 保持旧范围。

```bash
# 以下写法均可使用；裸 ID 在唯一匹配时自动定位 provider
kproxy --provider copilot account tag acc_b85b28b7 --add copilot-test
kproxy account --provider copilot tag acc_b85b28b7 --add copilot-test
kproxy account tag --add copilot-test acc_b85b28b7

export KPROXY_PROVIDER=copilot
kproxy account list
kproxy models list
kproxy account refresh --all
unset KPROXY_PROVIDER

# 共用账号添加入口，原 add-sso/add-api-key 命令继续保留
kproxy --provider kiro account add --auth sso --email user@example.com --password-stdin
kproxy --provider kiro account add --auth api-key --email user@example.com --token-stdin
kproxy --provider copilot account add --auth device-flow
```

服务标签池对两种 provider 都生效，规则为“标签并集/全局池 + 手工加入 − 排除”。
手工绑定的 Copilot ID 持久化为 `provider/account_id`，旧裸 ID 仍表示 Kiro。
Copilot 删除绑定账号、使账号退出现有标签池的改标操作会提示先调整绑定；给全局池或手工
绑定账号增加标签可直接执行。Kiro 保持原有绑定保护规则。

```bash
kproxy service accounts <SERVICE> --provider copilot
kproxy service add-account <SERVICE> copilot/acc_b85b28b7
kproxy service remove-account <SERVICE> copilot/acc_b85b28b7
kproxy account services copilot/acc_b85b28b7
```

其他常用命令：

全局模型开关适用于全部来源：

```bash
kproxy model disable claude-sonnet-4.6 gpt-4.1
kproxy model list
kproxy model enable gpt-4.1
```

使用模型列表中的 ID，不带 `kiro/`、`copilot/` 等来源前缀；支持一次指定多个模型，
不支持通配符。`model` 是 `models` 的别名。若设置了 `KPROXY_PROVIDER`，全局开关可显式
加 `--provider all`。CLI 的人类可读列表标记“不可用（已禁用）”，JSON 条目带
`disabled: true`、`available: false`；上游不再提供的禁用 ID 仍作为全局规则展示。

配置保存于 `[models].disabled`，可通过配置热更新修改，普通配置重置会保留禁用规则。
公共 `/models`、`/v1/models` 的 OpenAI、Anthropic 和混合格式均不展示禁用项。
原始请求和最终下游模型均检查禁用规则，因此别名、映射、默认模型和回退不能绕过。
底层发送前还会校验共享的最新规则，覆盖流式续接、压缩摘要、重试、token 计数与账户探测。
全部探测候选被禁用时跳过推理探测；模型目录/凭证刷新仍可进行，它们不调用模型。

禁用更新会等待已经开始发送的请求取得响应头，再对排队和后续发送生效。
已发送的请求无法撤回，其响应可以结束；后续重试或续接必须重新通过检查。

```bash
kproxy provider list
kproxy status --provider all
kproxy ready --provider copilot
kproxy account list --provider all
kproxy account list --provider copilot
kproxy account show copilot/<ACCOUNT_ID>
kproxy account tag --provider copilot <ACCOUNT_ID> --add team-a
kproxy account probe --provider copilot <ACCOUNT_ID>
kproxy account reset-health --provider copilot <ACCOUNT_ID>
kproxy account refresh --provider copilot --all
kproxy account export --provider all --redact
kproxy models list --provider all
kproxy models list --provider copilot --refresh
kproxy tasks run model_cache_refresh --provider copilot
kproxy pool --provider copilot --model gpt-5-mini
kproxy service list --provider copilot
kproxy apikey list --provider copilot
kproxy apikey usage <KEY_ID> --provider copilot
kproxy stats --provider copilot --detail --by model
kproxy logs show --provider copilot
kproxy subscriptions --provider all
```

provider 本身可通过 `add`、`edit`、`enable`、`disable`、`delete` 管理。删除 provider 只删除配置，
不会删除其账号文件；配置校验会阻止删除仍被服务、API key 或映射规则引用的 provider。

Copilot 响应中的标准 token usage 会写入 API key 用量和持久化统计。流式响应按 SSE 事件增量
解析，没有 usage 的上游响应会标记为 `unreported`，不会伪造 token 数。`copilot_usage` 会以带
schema 版本的 provider 原生计费对象原样保存；存在 `total_nano_aiu` 时同时记录精确的
`nano_aiu` 整数。Copilot 的 Kiro credits 维度保持 `0`，两种计量单位不会相互换算。

`status`、`ready`、账号、模型、服务、API key 用量、统计和日志都支持 provider 范围。订阅查询
在 `--provider all` 中会明确标记每个来源是否支持；Copilot 当前没有权威订阅接口，因此返回
`supported = false`，不会伪造空套餐。Kiro 的 `diagnose all/endpoints` 仍是 Kiro 上游专项诊断；
Copilot 使用 `account probe --provider <id>` 验证 GitHub 用户、token exchange 和模型目录链路。

## 配置示例

等价 TOML 如下：

```toml
[[provider]]
id = "kiro"
kind = "kiro"
enabled = true

[[provider]]
id = "copilot"
kind = "copilot"
enabled = true

[provider.settings]
client_id = "Iv1.b507a08c87ecfe98"
user_agent = "GitHubCopilotChat/0.31.0"

[provider.pool]
max_concurrent_per_account = 2

[provider.models]
cache_ttl_ms = 300000
max_stale_ms = 1800000

[provider.routing]
default_model_id = ""
enable_model_fallback = false
allow_cross_provider_fallback = false
```

默认 Client ID 为 `Iv1.b507a08c87ecfe98`，与
[Alorse/copilot-to-api](https://github.com/Alorse/copilot-to-api#about-the-client-id) 和
[ericc-ch/copilot-api](https://github.com/ericc-ch/copilot-api/blob/master/src/lib/api-config.ts) 的
Device Flow 实现一致；它是公开的 OAuth 应用标识，不是用户凭证。企业可能限制第三方 OAuth App，
此时仍可配置组织批准的 `client_id`。没有真实 Copilot 账号时可以完成编译、配置、mock 协议与持久化测试，
但最终上线前仍应使用目标 GitHub 组织的真实账号验证 Device Flow、token exchange、模型列表、
三种生成协议和流式 usage。
