# 部署、运维与 CLI 迁移

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md) | [项目说明](../README.zh-CN.md)

本文对应当前源码，包含 `v0.2.4` 之后的未发布改动。首次接入见项目 README；
这里维护环境、账号、持久化、Docker/systemd、CLI 迁移和恢复流程。命令在仓库根目录执行，
`kproxy` 指匹配版本的原生二进制或已安装的 Docker 包装器。

- [环境与路径](#2-环境变量加载)、[账号](#4-添加或导入账号)、[配置](#6-配置与热重载)
- [Docker](#8-docker-compose)、[systemd](#9-systemd)、[备份与恢复](#备份与恢复)
- [CLI 迁移](#旧命令迁移)、[日志](#7-日志与-trace-id)、[排障](#常见问题)

## 1. 环境要求

原生构建需要 rustup、C 工具链和链接器；仓库固定 Rust 1.97.1。
Docker 部署需要 Engine 和 Compose 插件；使用 host network，Docker Desktop 需启用对应设置。
默认构建包含 Chromium SSO，`--no-default-features` 关闭浏览器登录。构建、测试和调试入口见
[贡献指南](../CONTRIBUTING.md)及仓库内的 [VS Code 配置](../.vscode)。

## 2. 环境变量加载

本地启动前复制示例文件：

```bash
cp .env.example .env  # First setup only; preserve an existing .env
```

`kproxyd` 会在解析启动参数前读取 `.env`。`kproxy` 会先解析帮助、指南、补全和版本等本地
导航，再在重新解析和执行业务命令前读取 `.env`。两者都从当前目录向上查找，因此从
workspace 子目录启动时也能复用仓库根目录的文件。

环境变量优先级如下：

1. 进程环境中已经存在的变量；
2. 从向上查找时遇到的最近一个 `.env` 加载的值；
3. 存在持久化配置项时使用 `config.toml` 中的值；
4. 应用内置默认值。

`.env` 不会覆盖已存在的进程变量。找不到 `.env` 可以正常启动；文件格式错误或无法读取时，
daemon 启动和业务命令会失败并返回错误，本地 CLI 导航仍然可用。

示例使用 `KPROXY_HOME=.kproxy-dev` 隔离开发数据。主要进程级变量如下：

| 变量 | 作用 |
| --- | --- |
| `KPROXY_HOME` | 将配置、数据、日志和自动生成的管理 socket 放到同一目录。 |
| `KPROXY_HTTP_PORT` | 覆盖端口等于 `server.port` 默认值的已配置代理服务；不会创建服务。 |
| `KPROXY_DISABLE_HTTP=1` | 阻止所有已配置代理服务监听，但保留其配置并继续运行 Unix 管理 socket。 |
| `KPROXY_ADMIN_SOCKET` | 覆盖 `kproxy` CLI 连接的 socket，不会重新配置 `kproxyd`。 |
| `KPROXY_CODEWHISPERER_URL` | 在集成测试或受控代理环境中覆盖 CodeWhisperer 上游地址。 |
| `KPROXY_AMAZONQ_URL` | 在集成测试或受控代理环境中覆盖 Amazon Q 上游地址。 |
| `RUST_LOG` | 设置控制台和应用诊断的 tracing 过滤器。 |
| `RUST_BACKTRACE` | 设置为 `1` 或 `full` 时启用 Rust 调用栈。 |

从不同目录启动时应使用绝对路径 `KPROXY_HOME`。加载同一份 `.env` 不会重定位相对路径：
`.kproxy-dev` 仍相对于进程工作目录。CLI socket 优先级为 `--socket`、已有进程环境、
`.env`、配置/默认值。[环境模板](../.env.example)另列出 MCP/runtime/management 覆盖项与 CLI 凭证输入。

持久化的服务、账号池、模型、API Key、TLS、通知和日志配置应写入 `config.toml`，而不是
`.env`。

## 3. 使用本地二进制启动

```bash
cargo build --workspace --locked
cargo run -p kproxyd
```

另一个终端在相同目录运行 `cargo run -p kproxy -- health`。后续命令可使用
`./target/debug/kproxy`，或将编译目录加入 `PATH`。发布构建使用 `cargo build --release --locked`。
同一个 socket 只能运行一个 daemon；崩溃遗留的失效 socket 会自动清理，仍可连接的 socket 不会被删除。
`KPROXY_DISABLE_HTTP=1` 可阻止业务监听，同时保留配置和管理 socket。

## 4. 添加或导入账号

从 JSON 文件或 stdin 导入企业 SSO 凭证：

```bash
kproxy account import --file accounts.json --tag team-a
cat accounts.json | kproxy account import --stdin --tag team-a --tag prod
```

`id`、`machine_id` 和 `created_at` 可以省略，CLI 会自动生成。`--tag` 可重复或使用逗号分隔，
并会合并到本次导入的全部账号；JSON 中每个账号已有的 `tags` 会保留。
对已经存在的账号，也可以一次传入多个 ID 或邮箱来修改标签：

```bash
kproxy account tag --add test acc_2c36cfad acc_332c7cb2 acc_41c6e3ad
```

批量操作会先校验全部账号再统一保存：只要有账号不存在，或需要修改标签的账号已绑定代理服务，
整批都不会变更。原有的单账号用法仍可使用。

下面只展示 JSON 结构，不能直接作为可用凭证导入。Token 和 `expires_at`（Unix 秒）
必须替换为上游签发的实际值。

```json
[
  {
    "email": "user@example.com",
    "credentials": {
      "access_token": "...",
      "refresh_token": "...",
      "client_id": "...",
      "client_secret": "...",
      "region": "us-east-1",
      "expires_at": 1893456000,
      "auth_method": "idc"
    }
  }
]
```

### Kiro headless API key 与区域 runtime

先在 CLI 环境中安全设置 `KIRO_API_KEY`，再导入；也可从标准输入读取，避免把密钥写进命令参数：

```bash
kproxy account add-api-key --email ci@example.com --region us-east-1 --tag ci
kproxy account add-api-key --email ci@example.com --region eu-central-1 --key-stdin < /secure/kiro-key
```

密钥保存为 `credentials.access_token`，`auth_method` 为 `api_key`，`expires_at` 为 0。
不可附带 OAuth refresh/client secret 或 profile ARN。此类账号使用区域 runtime 和
`TokenType: API_KEY`，不执行 OAuth 刷新，也不产生 Token 刷新失败告警；密钥撤销后需手动更换，
重复导入不会覆盖已有账号。由于 management API
要求 OAuth profile，API key 使用静态模型目录；无发现到的 effort 元数据时仍按保守策略省略
thinking 控制。只在 daemon 环境中设置 `KIRO_API_KEY` 不会自动导入账号。
错误清理和上游错误格式化（包括后台诊断）会遮蔽回显的 Kiro key，但仍不可分享原始凭证。

OAuth 默认保留区域 Q/CodeWhisperer 路由，可设置 `upstream.preferred_endpoint = "runtime"`
优先使用 Kiro runtime/management RPC。GovCloud 与 API key 只使用 runtime，不回退到旧端点；
GovCloud 缺少 profile 时不会替换为商业区 Builder ID profile。测试/部署覆盖项
`KPROXY_RUNTIME_URL`、`KPROXY_MANAGEMENT_URL` 支持 `{region}` 占位符。

账号导出默认包含凭证。分享诊断结果前应使用 `--redact`：

```bash
kproxy --json account export --redact
```

### 企业 SSO 认证

`kproxyd` 的默认构建和 Docker Compose 都启用全部 feature，包含企业 IAM Identity Center
登录所需的 SSO 支持。先用 `kproxy config edit` 设置全局 start URL：

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

然后手动添加账号时无需重复传 `--start-url`：

```bash
printf '%s\n' "$PASSWORD" | kproxy account add-sso \
  --email user@example.com \
  --tag team-a \
  --password-stdin

kproxy account add-sso --batch accounts.csv -c 1 --tag team-a

# 显式从 stdin 读取，适合管道和自动化：
kproxy account add-sso --batch - -c 1 < accounts.csv
```

单次登录仍可用 `--start-url` 覆盖全局值。若明确需要更小且不含浏览器 SSO 的二进制，可用
`cargo build --workspace --no-default-features` 或 Docker 的 `runtime-slim` target。
Docker 宿主机 wrapper 会自动识别可读的宿主机 CSV，并通过 stdin 流式传入容器，不复制或
残留密码文件；容器内路径在宿主机没有同名文件时仍按原样读取。密码只从 stdin 或两列 CSV
文件读取。批量模式下，命令上的全部 `--tag` 会应用到本批所有账号。遇到 MFA 或上游页面变化需要手工操作时，增加
`--headful`。每个账号都会使用独立的 Chromium 无痕 context 和临时 profile，并在处理下一个
账号前销毁；写入账号前会记录 Kiro 返回的稳定用户 ID，并拒绝把同一真实身份重复登记到
其他邮箱。IAM Identity Center 的显示名不一定与登录邮箱一致，因此显示名仅用于诊断，不作为
拒绝入库的条件。该流程不会增加对非企业账号或非 SSO 认证方式的支持。

## 5. 创建并验证代理服务

全新 daemon 不创建业务监听。`health` 只检查存活；`ready` 检查账号、监听、计量恢复模式和
后台任务心跳，不能代替真实生成验证。创建服务后保存返回的客户端 API Key：

```bash
kproxy service create --name main --host 127.0.0.1 --port 5580
kproxy service list
kproxy service apikeys main
kproxy ready
kproxy models list
```

创建服务时可指定一个或多个账号标签；多个标签取并集，账号匹配任一标签即可进入基础账号池。
未提供 `--account-tag` 的服务继续使用全局账号池。请在账号绑定服务前设置标签；如果已经按上面的
示例创建了全局池服务 `main`，需要先从该服务排除这些账号：

```bash
kproxy service remove-account main alice@example.com bob@example.com
kproxy account tag alice@example.com --add team-a
kproxy account tag bob@example.com --add team-b
kproxy service create --name team-a --host 127.0.0.1 --port 5581 --account-tag team-a team-b
kproxy service accounts team-a
kproxy account services alice@example.com

# 手工加入任意标签的账号，或从当前服务排除账号：
kproxy service add-account team-a carol@example.com
kproxy service remove-account team-a alice@example.com
```

服务的有效账号池为“匹配任一所选标签的账号 + 手工加入账号 - 排除账号”；排除优先。服务未指定标签时，
基础集合是全部账号。健康检查、就绪检查、模型列表、首次调度、重试及压缩摘要请求都限定在
该服务的有效账号池内。Kiro 账号绑定服务后不能修改标签或删除；Copilot 也保护绑定账号的
删除和会使其退出现有池的标签修改，但允许向全局池/手工绑定账号增加标签。需要解除绑定时，
先通过 `account services` 查看全部绑定，再使用 `service remove-account`。
修改服务的账号标签必须先停用服务：

```bash
kproxy service disable team-a
kproxy service edit team-a --account-tag team-b team-c  # 或 --clear-account-tag 恢复全局池
kproxy service enable team-a
```

省略 `--host` 时默认绑定 `0.0.0.0`。`service apikey <服务>`（兼容 `service apikeys`）
默认显示完整密钥，JSON 输出相同；`--redact` 可隐藏明文。旧 `--show-secret` 参数仍可使用。
服务只接受已绑定的 API Key。默认 Messages 只接受 Claude Code，Chat/Responses 只接受 Codex；
第三方客户端可通过 `service edit` 或 `apikey edit` 设置 `--skip-user-agent-check true`。
这项豁免不关闭 Key 认证、额度和并发限制。请求示例见 [README](../README.zh-CN.md)和
[Responses 接入](openai-responses.md)。

## 6. 配置与热重载

### 文件与持久化

`.env` 用于启动路径选择和临时进程级覆盖；`config.toml` 用于持久化服务、账号池、模型、
API Key、TLS、日志和通知配置。所有示例环境变量及其作用见
[`.env.example`](../.env.example)。

设置 `KPROXY_HOME` 后，配置、数据、日志和管理 socket 会统一放到该目录。未设置时遵循
XDG 目录：

| 文件 | 默认位置 | 说明 |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/kproxy/` | 人工维护的 daemon 配置。 |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 包含凭证，创建权限为 `0600`。 |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 按 UTC 日期重置的每日额度记录。 |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 持久化请求聚合统计。 |
| `stats-history/` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 按 UTC 小时分片保存的分钟级请求聚合。 |
| `alert-incidents.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 持久化告警去重状态，应随数据目录备份。 |
| `web-search-replay.key` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | AES-256-GCM 回放密钥，以 `0600` 创建且永不覆盖。 |
| `admin.sock` | `${XDG_RUNTIME_DIR}/kproxy/` 或 `/run/kproxy/` | 本地管理面。 |
| 日志 | `${XDG_DATA_HOME:-~/.local/share}/kproxy/logs/` | 按 UTC 日期和级别拆分。 |

首次启动只创建缺失文件，不覆盖已有数据。有效配置修改会自动热重载；TOML 格式错误或校验
失败时继续使用上一份有效配置。`server.host` 和 `server.port` 是新建代理服务时使用的
默认值。修改 `admin.socket` 或共享的 HTTP/HTTPS 监听模式需要重启 daemon；包括代理服务
列表在内的其余大部分配置无需重启。

外部修改账号文件也会自动载入，损坏的账号数据不会替换内存中的有效快照。账号数较多时，
可根据存储配置使用 gzip envelope 和增量 sidecar。

### 配置操作

```bash
kproxy config path
kproxy config show --effective
kproxy config validate
kproxy config edit
kproxy config reload
kproxy models resolve claude-sonnet-4.5
```

`config edit` 及 service/apikey/alert/model-map 等修改命令使用校验、事务锁、原子写入和热重载。
启动环境覆盖项需要修改环境并重启。升级保留既有值，不会自动换成新默认值。
模型探测在启动、账号变化时触发，之后遵循缓存 TTL；账号状态任务仅刷新额度。
条件 model-map 按实际选中账号的额度判断；带 `--below-credits-percent` 的规则未设 schedule 时全天生效。
告警使用 `--platform`、`--webhook-url`，平台专用字段见 `kproxy alert platforms`；
`alert edit --event` 整体替换订阅。持续异常会去重，恢复后再次发生才重发，同类账号事件会短时聚合。
完整命令和配置主题直接使用 `kproxy help --all` 与 `kproxy guide`。

## 7. 日志与 Trace ID

```bash
kproxy logs show --tail 100
kproxy logs follow --level warn
kproxy logs trace <TRACE_ID>
kproxy logs path
kproxy logs files --level error
kproxy status --since 30m
kproxy stats --detail --since 1h --by endpoint
```

响应头 `x-trace-id` 和 `request-id` 可用于跨级别、跨日期追踪。日志按精确级别和 UTC 日期拆分，
默认每片 100 MB、保留三天；`info.log` 不包含 WARN/ERROR。`logs trace` 有扫描和输出上限。
Docker wrapper 会为卷内日志增加宿主机路径，卷外自定义路径无法映射。
`RUST_LOG` 或 `log.level` 可提高诊断级别；应用日志不记录提示词、回复正文或 Key 值。

`status` 统计本次启动以来的数据；`stats` 默认使用跨重启累计值。两者支持 `--since` 或带时区的
`--start/--end`。分钟级历史按 UTC 小时写入 `stats-history/`，持续保留，应监控磁盘增长；
旧版本已淘汰的历史不能恢复。逐条故障仍查日志，`stats --detail` 不是完整请求审计。

## 8. Docker Compose

当前发布目标是 Linux amd64 full；`v0.2.4` 仅为版本选择示例，使用前确认仓库存在该镜像。
验证未发布功能需构建当前源码。setup 脚本会先拉取，再替换容器、等待健康并安装匹配的 wrapper：

```bash
./deploy/docker-setup.sh --image ghcr.io/yaocool/kiro-proxy:v0.2.4
kproxy version
kproxy ready
```

默认安装到 `/usr/local/bin/kproxy`，也可传 `--target "$HOME/.local/bin/kproxy"`。
后续 `./deploy/docker-upgrade.sh` 追踪 `latest`；固定版本继续用 `--image`。
单独安装 wrapper 用 `./deploy/install-kproxy-wrapper.sh`，默认拒绝覆盖其他同名命令。

Compose 使用 host network；Linux Engine 原生支持，Docker Desktop 4.34+ 需在设置中启用。
服务监听直接使用宿主机端口。默认服务地址为 `0.0.0.0`，本机服务应显式绑定 `127.0.0.1`。
数据卷 `kproxy-data` 挂到 `/var/lib/kproxy`，不要挂入开发用 `.kproxy-dev` 或 `.env.example`。
原 bridge 部署需要重建容器以应用新网络模式；卷数据保留。日常排查和主动源码构建分别使用：

```bash
docker compose config --quiet
docker compose ps
docker compose logs -f kproxyd
# Source build only:
docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build
```

full 包含配对固定的 Chromium `r1566079` 和 `chromiumoxide 0.9.1`，容器设置了 no-sandbox。
源码可将 build override 的 target 改为 `runtime-slim`；该目标没有浏览器 SSO，当前 CI 不发布它。
本地构建默认 `CARGO_BUILD_JOBS=1`。镜像的编辑器为 vim，可用 `EDITOR` 选择已安装的其他编辑器。

wrapper 需要可用的 Docker 引擎和可识别的部署。多部署时设置 `KPROXY_COMPOSE_PROJECT` 或
`KPROXY_DOCKER_CONTAINER`。停服后导航命令使用精确本地镜像，以无网络、无业务卷方式运行；
缺少镜像、目标不明确或旧镜像不支持本地导航会失败。业务动作需要运行中的 daemon。
wrapper 保留退出码和 stdin，按交互场景分配 TTY，并传递或回退 `TERM`。

`kproxy restart` 等待健康，`kproxy stop` 保留部署；`kproxy uninstall` 会停服、先备份再删除
容器、数据卷、未共享镜像和 wrapper。默认备份位于 `~/.kproxy/backups`，备份失败则恢复容器，
不删除原数据。`uninstall --yes` 保留备份，只有显式 `--delete-backup` 才删除备份。
`docker compose down` 保留卷，`down -v` 删除卷。

若卷元数据存在但实际目录丢失，setup 仅对本项目标记且可确认目录缺失的卷提供
`--repair-volume`；交互模式会确认。数据盘挂载或 Docker 根目录异常必须先恢复存储。
部署健康失败会尝试回退旧镜像；旧镜像缺失或回退不健康仍需人工恢复。
**镜像回退复用原卷，不撤销数据写入或迁移。** 升级前按[备份与恢复](#备份与恢复)留存完整状态，
重启会丢失 Responses 进程内续轮。升级后核对版本、ready、账号、服务、配置、统计和生成请求。

## 9. systemd

构建 release 二进制并安装 unit：

```bash
cargo build --release --locked

sudo useradd --system --user-group --home-dir /var/lib/kproxy --shell /usr/sbin/nologin kproxy
sudo install -m 0755 target/release/kproxyd target/release/kproxy /usr/local/bin/
sudo install -m 0644 deploy/kproxyd.service /etc/systemd/system/kproxyd.service
sudo systemctl daemon-reload
sudo systemctl enable --now kproxyd
sudo systemctl status kproxyd
```

如果 `kproxy` 用户已存在，跳过 `useradd`。unit 通过 systemd 管理的目录使用 `/etc/kproxy`、
`/var/lib/kproxy` 和 `/run/kproxy`。

```bash
sudo -u kproxy kproxy --socket /run/kproxy/admin.sock status
sudo journalctl -u kproxyd -f
sudo systemctl reload kproxyd
```

reload 会发送 `SIGHUP`。需要重启的配置仍要执行 `sudo systemctl restart kproxyd`。

使用 `kproxy account add-sso` 前还要在宿主机安装 Chrome 或 Chromium。提供的 unit 支持
默认 full 构建：它会为 Chromium 保留用户命名空间和 JIT 可执行内存，同时继续启用
`NoNewPrivileges`、文件系统保护、空 capability 集合等加固项。如果宿主机内核禁用了非特权
用户命名空间，优先在系统层启用；最后的兼容手段是通过 `systemctl edit kproxyd` 设置
`KPROXY_CHROMIUM_NO_SANDBOX=1`。该选项会关闭 Chromium 自身的 sandbox，只应在评估宿主机
隔离边界后使用。

## 导航与脚本

`kproxy`、`kproxy help` 和 `kproxy logs` 等无参命令组显示帮助。嵌套参数使用
`kproxy help logs trace`，完整命令树使用 `kproxy help --all`，操作主题使用 `kproxy guide`。
这些导航入口以及 `completions`、`version` 不要求 daemon 或有效的 `.env`。

业务输出支持全局 `--json`。无参命令组带 `--json` 时退出码为 2，stdout 为空，stderr 提示补充动作。
显式 `--help` 仍输出文本并以 0 退出。通常成功为 0、参数错误为 2、连接或执行失败为 1；
`ready` 在业务未就绪时也返回失败。TTY、管道和重定向不改变命令解析含义。

下方命令是相互独立的操作示例，不应整段依次执行；ID、名称、路径和密钥需要替换。
`diagnose all`、`diagnose account` 会向上游发起真实推理并消耗额度；初步检查可先用
`health`、`ready`、`account list` 和 `logs show`。

## 旧命令迁移

`--provider` 现在可放在任意层级，例如 `kproxy --provider copilot account list`、
`kproxy account --provider copilot list` 与末尾写法等价。`KPROXY_PROVIDER` 设置当前
shell 的默认来源，显式参数优先；`-p` 是简写。参数必须使用 `provider list` 的实例 ID，
不能使用驱动类型或“GitHub Copilot”显示名称。例如 `--id work-cp --kind copilot` 创建的
实例要用 `--provider work-cp`。`provider show work-cp` 会输出使用实际 ID 的操作命令；
`guide copilot -p work-cp` 可生成对应的接入示例。
Docker wrapper 会转发该环境变量，并在 provider 参数放于任意命令层级时正确转发宿主机
批量 CSV 文件；使用这些写法时需要同步更新已安装的 wrapper。

子命令帮助和 `help --all` 都标明共用、仅 Kiro、仅 Copilot、全局或按资源定位的范围。
全局操作和按位置参数中的 ID/名称定位资源的命令忽略 `KPROXY_PROVIDER`，并拒绝显式
`--provider`，避免参数被悄悄忽略。全局模型禁用/启用仍保留更严格的检查：有默认来源时
用 `--provider all` 覆盖。Kiro 任务仅接受 `kiro`，模型刷新任务可选具体实例，全局任务
不接受来源参数；`tasks list` 直接显示各任务适用范围。

`service edit`、`apikey edit` 和 `model-map edit` 修改来源权限必须显式传入 `--provider`；
环境变量不会覆盖资源现有权限。`--provider` 与 `--clear-providers` 不能同时使用。
`service edit --provider ...` 排除原默认来源时会自动选第一个新 ID；原默认仍在范围内则
保留，也可用 `--default-provider` 显式指定。API key 权限仍需独立配置。

账号添加统一使用 `account add`：Kiro 选
`--provider kiro --auth sso` 或 `--auth api-key`，Copilot 选
`--provider copilot --auth device-flow`；旧 Kiro 认证命令仍保留。
完整示例见[按来源管理](providers-and-copilot.zh-CN.md#统一和按来源管理)。

`pool` 和 `account refresh/probe/reset-health --all` 默认覆盖全部 provider；加
`--provider kiro` 可保留旧范围。`account export` 仍默认旧 Kiro 格式，全量需
`--provider all`。服务账号列表、标签池、手工绑定和排除均支持 Copilot；账号重名时
使用 `provider/account_id` 消歧。

| 旧写法 | 当前写法 |
| --- | --- |
| `kproxy logs --tail 100` | `kproxy logs show --tail 100` |
| `kproxy logs -f` / `--follow` | `kproxy logs follow` |
| `kproxy models --refresh --mapped` | `kproxy models list --refresh --mapped` |
| `kproxy tasks`（查询数据） | `kproxy tasks list` |
| `kproxy diagnose`（完整诊断） | `kproxy diagnose all` |
| `kproxy help balance` | `kproxy guide balance` |
| `account add-sso-batch --file FILE` | `account add-sso --batch FILE` |
| `alert add --kind ... --url ...` | `alert add --platform ... --webhook-url ...` |
| 告警平台 `wechat` | `wechat-work`，平台专用选项见 `alert platforms` |

旧参数在业务初始化前报错。裸 `logs`、`models`、`tasks`、`diagnose` 改为组帮助，重定向时也相同。
`logs show` 和 `logs follow` 在改造前就已存在，这次移除的是父命令快捷写法。
发布差异见[变更记录](../CHANGELOG.md)。

删除 daemon 业务资源必须交互输入 `y` 或 `yes`，没有通用 `--yes`；Docker uninstall 的选项单独定义。

## 备份与恢复

记录源版本/commit、镜像 ID/digest、Compose 项目及实际配置/数据路径。分开使用 XDG 目录时，
配置和数据必须同时备份。账号导出不是完整备份；至少保留 `config.toml`、账号文件及压缩/增量
sidecar、`daily.json`、`stats.json`、`stats-history/`、`alert-incidents.json`、
`web-search-replay.key` 和排障需要的日志。不要遗漏回放密钥，否则旧搜索回放无法解密。

标准 Docker 部署在维护窗口停写后复制，逐步确认执行结果：

```bash
umask 077
backup_dir="$HOME/kproxy-backups/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$backup_dir"
docker compose stop kproxyd
docker compose cp kproxyd:/var/lib/kproxy/. "$backup_dir/"
docker compose start kproxyd
```

检查复制退出状态及关键文件。复制失败应恢复原服务，获得有效备份后再升级。
原生部署先停止 daemon，再保留权限复制其实际配置/数据目录。

恢复时保留故障数据供排查，把升级前备份放入**独立空目录或新卷**，保留服务账号所有权和限制性
权限（Docker UID/GID 为 10001），使用原镜像启动。不要混合新版本写入的数据和旧备份。
核对账号、Key 绑定、额度计数、统计、ready 和测试请求后再切回流量。管理 socket 由 daemon 重建。
目前不保证任意原地降级；可支持的升级来源和恢复演练要求见[发布修复方案](release-readiness-1.0.0.zh-CN.md)。

## 常见问题

| 现象 | 核查动作 |
| --- | --- |
| 端口占用 | `service list` 对照实际进程监听；修改服务端口，而非仅修改新服务的默认端口。 |
| 无法连接 `admin.sock` | 核对 daemon、运行用户、绝对 `KPROXY_HOME`、`config path` 和 `--socket`。 |
| 配置未生效 | `config validate`、`config show --effective`，检查环境覆盖、保留的旧默认值及需重启项。 |
| 401 / 客户端拒绝 | 核对服务绑定的 Key、客户端产品 User-Agent 和豁免配置。 |
| 503 | `ready`、账号额度/保护阈值、并发、后台任务与 `logs trace`。 |
| 流式中断 | 核对协议终止事件和 Trace ID；HTTP 200 不代表最终成功。 |
| 上下文超限 | `models resolve` 查看实际模型，检查 `error.context`；见[压缩边界](protocol-compatibility.zh-CN.md#自动压缩与窗口)。 |
| 容器仍为旧版本 | 核对实际镜像 ID 与 `kproxy version`；拉取目标发布镜像或主动构建源码。 |
