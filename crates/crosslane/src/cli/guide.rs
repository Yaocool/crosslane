//! Long-form operational guides kept separate from generated command help.

use anyhow::{anyhow, Result};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Topic {
    Provider,
    Kiro,
    #[value(alias = "github-copilot")]
    Copilot,
    Status,
    Health,
    Version,
    Account,
    Sso,
    Service,
    #[value(name = "apikey")]
    ApiKey,
    Admin,
    Pool,
    Balance,
    Diagnose,
    Subscriptions,
    Tasks,
    Stats,
    Logs,
    Alert,
    Models,
    ModelMap,
    Config,
    Docker,
}

impl Topic {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Kiro => "kiro",
            Self::Copilot => "copilot",
            Self::Status => "status",
            Self::Health => "health",
            Self::Version => "version",
            Self::Account => "account",
            Self::Sso => "sso",
            Self::Service => "service",
            Self::ApiKey => "apikey",
            Self::Admin => "admin",
            Self::Pool => "pool",
            Self::Balance => "balance",
            Self::Diagnose => "diagnose",
            Self::Subscriptions => "subscriptions",
            Self::Tasks => "tasks",
            Self::Stats => "stats",
            Self::Logs => "logs",
            Self::Alert => "alert",
            Self::Models => "models",
            Self::ModelMap => "model-map",
            Self::Config => "config",
            Self::Docker => "docker",
        }
    }
}

const TOPICS: &[(&str, &str)] = &[
    ("provider", "Kiro/Copilot 命令适用范围与提供源管理"),
    ("kiro", "Kiro 账号、模型与服务接入"),
    ("copilot", "GitHub Copilot 账号、模型与服务接入"),
    ("status", "服务状态与时间范围"),
    ("health", "健康检查与业务就绪"),
    ("version", "版本和默认上游端点"),
    ("account", "账号管理与确认规则"),
    ("sso", "Kiro IAM Identity Center SSO 单账号与批量导入"),
    ("service", "代理服务生命周期"),
    ("apikey", "API key、限额与用量"),
    ("admin", "Admin REST 多令牌、Kiro 管理凭证与企业订阅"),
    ("pool", "Kiro 评分与 Copilot 账号模型支持"),
    ("balance", "Kiro 账号池调度评分原理"),
    ("diagnose", "Kiro 网络和账号真实推理诊断"),
    ("subscriptions", "Kiro 订阅计划查询"),
    ("tasks", "Kiro/Copilot/全局周期任务"),
    ("stats", "持久化统计与时间范围"),
    ("logs", "Service 请求、Admin 审计与日志文件"),
    ("alert", "Kiro 异常事件和通知目标"),
    ("models", "动态模型发现与解析"),
    ("model-map", "模型映射匹配规则"),
    ("config", "Kiro/Copilot/全局配置范围与编辑"),
    ("docker", "Docker 运行方式和数据卷"),
];

pub fn print(topic: Option<&str>, provider: Option<&str>) -> Result<()> {
    let Some(topic) = topic else {
        println!("可用指南主题：");
        for (name, description) in TOPICS {
            println!("  {name:<16}{description}");
        }
        println!("\n新用户可先看 `crosslane guide kiro` 或 `crosslane guide copilot`；命令参数用 `crosslane help <命令路径>` 查看。");
        return Ok(());
    };
    let mut text = topic_text(topic)?.to_owned();
    if topic == "copilot" {
        if let Some(id) = provider.filter(|id| *id != "all" && *id != "kiro") {
            for prefix in [
                "--provider",
                "--default-provider",
                "--id",
                "provider edit",
                "provider show",
            ] {
                text = text.replace(&format!("{prefix} copilot"), &format!("{prefix} {id}"));
            }
            println!("以下示例使用实例 ID：{id}（类型：GitHub Copilot）。\n");
        }
    }
    println!("{text}");
    if topic != "admin" {
        println!("\n--provider/-p 使用 `provider list` 中的实例 ID；--kind/--provider-kind 使用驱动类型。可用 `crosslane guide copilot -p <ID>` 按自定义 ID 生成接入示例。");
    }
    Ok(())
}

fn topic_text(topic: &str) -> Result<&'static str> {
    let text = match topic {
        "admin" => r#"Admin REST 令牌、Kiro 管理凭证与企业订阅（全局，不接受 --provider）
以下监听和令牌配置只用于 REST；admin kiro CLI 通过 Unix socket 工作，无需开启 REST。
1. 运行 crosslane config edit admin（或 config edit status-http），同屏编辑 [admin] 与 [status_http]；旧配置会补齐缺少的默认字段。
   先设置 [status_http] enabled=true，保留 [admin.rest] enabled=false；默认地址 127.0.0.1:7651。
   保存后自动重载，再执行第 2 步；监听的启停、地址和端口均不需要重启 daemon。
2. 创建并启用管理访问：
   crosslane admin token create --name ops --enable-rest
   crosslane admin token create --name monitor --role read-only
3. 查看元数据，不输出明文或摘要：
   crosslane admin token list
   crosslane admin token show ops
4. 轮换、启停和撤销：
   crosslane admin token rotate ops
   crosslane admin token disable monitor
   crosslane admin token enable monitor
   crosslane admin token revoke monitor

命令通过本机 Unix socket 执行，不需要已有 Bearer token。create/rotate 的明文只返回一次，--json 的 secret 字段可供自动化使用；配置仅保存 SHA256 摘要。请求头发送明文 Authorization: Bearer TOKEN。
多把 admin/read-only 令牌可同时有效，read-only 只允许 GET/HEAD。启停、轮换、撤销立即影响后续请求，已受理的请求/批量任务不会被自动取消。rotate 保留 ID、名称、权限和启停状态。
已有令牌时，可在 config edit admin 中直接设置 [admin.rest] enabled=true，无需重复创建。关闭状态监听时须同时关闭 REST；config reset status-http 会完成这两项，并保留令牌。config reset admin 会清除令牌和重置 socket，保留状态监听。
编辑器未改设置直接退出时不写文件、不重载服务，仍提示待重启字段。--enable-rest 要求运行监听与磁盘配置的启用状态、地址和端口一致，监听配置尚未应用时会拒绝创建，请先执行 crosslane config reload。socket 默认值取自 daemon；连接未提供默认值的旧 daemon 时保留当前 socket。完整 config reset 也会清除管理令牌并关闭 REST/状态监听，操作前自动备份。
旧 token_sha256/read_token_sha256 配置继续兼容，在列表中显示为 legacy-admin/legacy-read-only，也可使用这些 ID 管理。最后一把令牌撤销后关闭 Admin REST，公开状态和业务 service 保持原配置。
Kiro 企业用户与套餐也可通过 /admin/v1/kiro/enterprise 管理，REST 调用方只传 Admin token。
AWS 管理员凭证使用以下命令，不写 config.toml：
  crosslane admin kiro configure
  crosslane admin kiro configure --stdin < /secure/kiro-admin-input.json
  crosslane admin kiro show
  crosslane admin kiro clear
  crosslane admin kiro use-env
configure 完整替换凭证及区域/范围，隐藏输入 AK/SK/SessionToken；--stdin 读取完整 JSON，不接受密钥 argv 参数。daemon 将其 AES-256-GCM 加密保存到数据目录的 .secrets/kiro-admin.enc，同目录 kiro-admin.key 保存本机加密密钥；目录 0700、文件 0600，拒绝链接和不安全权限。修改对后续管理请求热生效，不重启 daemon，也不影响已受理操作。
写入结果为本次提交快照；并发修改后的最新状态用 show 查询。若返回 persistence_warning，修改已热生效，但崩溃后的持久性未确认，检查存储后重试原操作。show 只显示状态和来源。clear 删除当前凭证并持久化停用标记，不会回落旧环境变量；use-env 写入显式环境来源标记，恢复 daemon 启动环境中的 CROSSLANE_KIRO_ADMIN_* 来源。文件错误时拒绝远程管理，不自动回落；仅环境变量模式的更新仍需重启/重建进程。
这些凭证不属于 config reset；使用 admin kiro clear 停用。API、日志和普通配置导出不返回 AK/SK/SessionToken。保护加密文件及本机密钥的备份，两者齐全可解密。

通过 CLI 管理 Kiro 企业订阅（覆盖现有 Kiro 管理 REST 的全部操作）：
  crosslane admin kiro subscription plans
  crosslane admin kiro subscription profiles
  crosslane admin kiro subscription scope
  crosslane admin kiro subscription users
  crosslane admin kiro subscription list
  crosslane admin kiro subscription add user@example.com --plan power --dry-run
  crosslane admin kiro subscription add user@example.com --plan power
  crosslane admin kiro subscription show user@example.com
  crosslane --json admin kiro subscription show USER_ID
  crosslane admin kiro subscription upgrade user@example.com --plan power --dry-run
  crosslane admin kiro subscription downgrade user@example.com --plan pro --dry-run
  crosslane admin kiro subscription cancel user@example.com --dry-run
  crosslane admin kiro subscription cancel USER_ID --mode immediate
用户接受 Identity Center 的精确 userName、UserId 或用户 ARN；不需要 URL 编码。add/upgrade/downgrade 的 --plan 必填，支持 pro/pro-plus/pro-max/power。scope 参数 --region/--profile-arn/--application-arn/--identity-center-region/--legacy 放在 scope/users/list/show 或写命令后，省略时使用 daemon 凭证配置与自动发现。profiles 只接受 --region/--legacy。subscriptions 是 subscription 的别名；create/activate 是 add 的别名，delete/remove/rm 是 cancel 的别名。
upgrade/downgrade 要求唯一的直接订阅，并校验目标套餐高于/低于当前套餐；升级预期即时生效，降级预期下月生效。cancel 默认 --mode end-of-period（也接受 end_of_period），周期结束取消；immediate 预期立即停用，但计费仍可能至当月底。--legacy 取消必须明确使用 --mode immediate。取消保留 Identity Center 用户和组继承订阅。users 列出全部身份库用户及其直接订阅，应用 GROUP 订阅另列，不逐一展开用户组成员关系；个人继承情况用 show 查询。
这些命令使用 daemon 保存的 AK/SK，通过 Unix socket 执行，无需 Admin token 或 HTTP 监听；Docker 部署需访问挂载的管理 socket。CLI 与 REST 共享操作锁、并发限制与审计。两端需同时升级，旧 daemon 会返回升级提示。
add/upgrade/downgrade/cancel 默认实际提交；加 --dry-run 只读预览，不能验证 AWS 写权限。开通不导入本地账号、不执行浏览器 SSO；允许 AWS 自动分配 Kiro 应用，Crosslane 不额外重复调用。AWS 已受理不代表订阅或计费已生效。命令不会自动重试；超时、中断或结果不明时先 show 查询，再决定是否重试。
接口说明：docs/admin-rest.zh-CN.md、docs/kiro-enterprise-admin.zh-CN.md。"#,
        "provider" => {
            r#"命令适用范围
  两者共用：provider、status、ready、account add/list/show/rm/enable/disable/tag/export/refresh/probe/reset-health/services、pool、models、model-map、service、apikey、stats、logs show/follow；--provider 可放在任意子命令前后，也可设置 CROSSLANE_PROVIDER；列表与 --all 操作默认聚合全部来源，单账号按 ID 自动识别；export 保持旧 Kiro 格式，需 --provider all 才导出全部。
  Kiro 专项：account add-sso/add-api-key/import/overage/regen-machine-id、diagnose、subscriptions、pool --explain 评分、alert 事件、tasks token_refresh/status_check/health_recheck。
  Copilot 专项：account add --provider copilot --auth device-flow（或 --token-stdin）；OAuth/endpoint 设置属于 Copilot provider 实例。
  全局：health、version、config、logs trace/files/path、daemon 生命周期；其中 config 模块各有适用范围，见 crosslane guide config。

资源归属：provider 是上游实例，account 是上游账号，apikey 是客户端访问凭据，service 是业务监听和账号池，model-map 是模型路由规则。配置的实际实例 ID 用 provider list 查看。
参数说明：crosslane help service edit / crosslane service help edit / crosslane service edit -h；局部命令树：crosslane help account --all。无参数的顶层命令组显示帮助；account overage 保留旧版默认 show 行为。

提供源实例
  crosslane provider list
  crosslane provider show kiro
  crosslane provider show copilot

未显式配置 provider 时内置 kiro；首次添加 Copilot 会保留 Kiro。service/API key/model-map 的创建或编辑命令通过 --provider 设置来源范围，list/test 等读取命令用它过滤或选择来源；account 的写命令用它定位账号，status/ready/account 的读命令及 pool/models/stats/logs 用它过滤或选择来源。
接入步骤：crosslane guide kiro / crosslane guide copilot"#
        }
        "kiro" => {
            r#"Kiro 接入（未显式配置 provider 时默认提供 kiro）
1. 添加账号：
   printf '%s\n' "$PASSWORD" | crosslane --provider kiro account add --auth sso --email user@example.com --start-url https://example.awsapps.com/start --password-stdin
   或 crosslane --provider kiro account add --auth api-key --email user@example.com --token-stdin < /secure/kiro-key
   原 account add-sso/add-api-key 命令继续可用。
   SSO 批量导入：crosslane guide sso
2. 检查账号和模型：
   crosslane account list --provider kiro
   crosslane models list --provider kiro --refresh
3. 创建 Kiro 专用服务（端口自动分配，也可用 --port 指定）：
   crosslane service create --name kiro --provider kiro --default-provider kiro

diagnose all/endpoints、account overage 是 Kiro 专项功能。"#
        }
        "copilot" => {
            r#"GitHub Copilot 接入
1. 添加提供源；命令会将默认公开 Client ID 写入配置文件的 [provider.settings]：
   crosslane provider add --id copilot --kind copilot
   用 crosslane config path 查看配置文件位置；可直接编辑 client_id，或用 crosslane provider edit copilot --setting 'client_id="Iv1.your-app"' 覆盖。
2. SSH 无头服务器可用 full 镜像中的 Chromium 自动完成账号登录及 Device Flow：
   crosslane account add --provider copilot --auth device-flow --headless
   按提示选择 Azure SSO / GitHub 密码并隐藏输入密码；也支持 CSV 逐个登录：crosslane account add --provider copilot --batch copilot.csv
   CSV 必须有 github_username 表头，可选 github_password,sso_username,sso_password,sso_start_url,label；GitHub 密码或 Azure SSO 凭证至少提供一套。
   非交互模式也可从标准输入读取密码：
   crosslane account add --provider copilot --auth device-flow --username USER_SHORTCODE --sso-username user@example.com --password-stdin
   SSO 的 --username 填完整 GitHub 用户名；--sso-username 填 Azure 登录名，此时 stdin 密码仅用于 Azure。普通 GitHub 密码登录省略 --sso-username，--username 可填 GitHub 用户名或已验证邮箱。两套密码不同可用 --credentials-stdin 读取 JSON。密码不保存在账号文件中。
   未提供浏览器凭证时，仍可在自己电脑的无痕/隐私窗口手动授权：
   crosslane account add --provider copilot --auth device-flow
   --provider 始终填创建实例时的 --id；以 provider list 的实例 ID 为准，不要填写 GitHub Copilot 类型名称。远端模式按任务隔离浏览器会话；MFA 推送仍需本人批准，动态验证码用 account login-code 提交。--capture-headers 可在服务器受保护文件中记录完整敏感 header，默认关闭。
   Azure 条件访问拒绝（如 AADSTS53003）需由 Entra 管理员检查失败策略；安全信息注册需先按组织要求完成，不能复制 header 绕过。
3. 检查账号和模型：
   crosslane account list --provider copilot
   crosslane account probe --provider copilot <ACCOUNT_ID>
   crosslane models list --provider copilot --refresh
4. 创建 Copilot 专用服务（端口自动比现有服务的最大端口加 1）：
   crosslane service create --name copilot --provider copilot --default-provider copilot

Copilot API 地址优先使用账号 token 返回的 endpoint；仅当缺失且企业网络要求专属域名时才需配置 api_endpoint_fallback。"#
        }
        "status" => {
            "`crosslane status` 展示 Kiro/Copilot 提供源、代理服务、账号池和本次启动后的请求统计；`--provider <ID>` 只看指定来源，省略时聚合全部；`--since` 或 `--start/--end` 控制时间范围，`--watch` 每 2 秒刷新。"
        }
        "health" => {
            "`crosslane health` 是全局 daemon 管理面检查，不按来源拆分；账号或代理服务为空不会让该检查失败。`crosslane ready --provider <ID>` 检查指定来源引用的服务与账号是否就绪；省略来源时检查整体业务就绪。"
        }
        "version" => "`crosslane version` 显示 CLI 版本、Rust MSRV 和默认 Kiro 上游端点；Copilot API 地址由各账号的 token 响应决定。",
        "account" => {
            "统一用 `account add --provider <ID>` 添加账号：Kiro 的 --auth sso 搭配 --email/--password-stdin，--token-stdin 搭配 --email 导入 API key；Copilot 默认 Device Flow，--token-stdin 导入 GitHub token。旧 add-sso/add-api-key/import 保留。`list/show/rm/enable/disable/tag/export/refresh/probe/reset-health` 覆盖两者，可用 `--provider` 消歧；`account rm <账号...> --force` 自动从所有 service 解绑再删除，跳过确认仍需 --yes；省略 --force 保留绑定保护。`refresh/probe/reset-health --all` 省略来源时覆盖全部来源。Kiro 的 `probe` 做真实推理，Copilot 的 `probe` 刷新凭证与模型目录；两种来源均支持标签筛选与 account services；`overage`、`regen-machine-id` 仅适用于 Kiro。详情见 `crosslane guide kiro` 和 `crosslane guide copilot`。"
        }
        "balance" => {
            "本评分仅用于 Kiro 账号池：score = active_ratio×weight_active + used_credit_ratio×weight_credit + recent_idle_penalty×weight_idle。\n分数越低越优先；随后加入小幅随机抖动。用 `crosslane pool --provider kiro --watch --explain` 查看实时评分。Copilot 的 `pool` 只显示账号认证与模型支持，不提供这组三因子评分。"
        }
        "pool" => {
            "`crosslane pool --provider kiro --model <模型> --explain` 展示 Kiro 账号可调度性、排队和三因子评分；省略 --provider 聚合全部来源。`--provider copilot` 展示 Copilot 账号认证状态及已发现模型，可用 --model 检查某个模型；--explain 不提供 Kiro 式评分。两者均可 --watch；Kiro 评分原理见 `crosslane guide balance`。"
        }
        "model-map" => {
            "模型映射按 priority 从小到大匹配；Kiro/Copilot 都可用 `--provider <ID>` 指定规则范围。add 省略来源时为 Kiro-only，edit 省略时保留原有范围。source_models 支持 `*`；replace/alias 选首个目标，loadbalance 按 weights 随机。`--below-credits-percent` 仅适用于 Kiro，不能与 Copilot 规则或跨来源目标组合。用 `model-map list/test --provider copilot` 核对 Copilot 映射；跨来源映射另需显式开启 provider 的 allow_cross_provider_fallback。"
        }
        "sso" => {
            r#"本主题仅适用于 Kiro 的 IAM Identity Center SSO；GitHub Copilot（即使组织登录经过 Azure SSO）请使用 `crosslane guide copilot` 和 Device Flow，不使用 `account add-sso`。先在配置中设置 `[sso] start_url = "https://..."`。单账号：`printf '%s\n' "$PASSWORD" | crosslane account add-sso --email user@example.com --password-stdin`。
用 `crosslane config edit sso` 设置 `login_timeout_secs`。它是等待浏览器授权回调的秒数，须为正整数，默认 120。保存后热重载，用 `crosslane config show sso --effective` 查看生效值。
批量：CSV 仅含 email,password 两列，运行 `crosslane account add-sso --batch accounts.csv -c 1 --tag team-a`；也可用 `--batch - < accounts.csv` 从 stdin 读取。可重复的 `--tag` 应用于本批全部账号；`--start-url` 可覆盖全局值，`--headful` 可手工完成额外验证。默认/full 构建包含 SSO。"#
        }
        "service" => {
            "`crosslane service list/show/create/edit/enable/disable/apikeys/delete` 管理独立代理监听。create 省略来源时保持 Kiro-only；`service create --name copilot --provider copilot --default-provider copilot` 创建 Copilot-only 服务；`--provider kiro --provider copilot --default-provider kiro` 创建双来源服务。省略 --port 时首个服务使用 server.port，后续取当前最大服务端口加 1；如端口已被外部进程占用，请显式指定 --port。`service apikey <服务>` 默认隐藏明文，使用 `--show-secret` 显示完整密钥。修改 --provider 后原默认来源不在新范围时自动选第一个 ID。`service accounts/add-account/remove-account` 与 `--account-tag` 支持两种来源；Copilot 的手工加入/排除持久化为 provider/account_id。未配置 --account-tag 的服务只使用未打标签的账号。服务标签与账号标签都可随时修改（包括运行中的服务和已绑定账号）；若修改会让启用中的服务没有可用账号则拒绝，可加 --force 强制应用。删除仍被服务使用的账号前需先解绑。API key 也有独立来源范围；删除服务时仅级联删除未共享 key。"
        }
        "config" => {
            r#"配置默认位于 $CROSSLANE_HOME/config.toml；全局设置无需为 Kiro/Copilot 分别复制。
  全局：server、log、admin、status-http。
  Kiro 专项：upstream、pool、features、models、context、storage、sso、model-thinking-mode、notify、webhook。
  Copilot 专项：[[provider]] 中 kind="copilot" 的 [provider.settings]（client_id、OAuth、endpoint、身份头）、[provider.pool]、[provider.models]、[provider.routing]。
  按来源：provider、model-mapping、api-key、proxy-service；用实例 ID、providers/allowed_providers 和 default_provider 限定。
  混合：tasks 中 token_refresh/status_check 属于 Kiro，stats_persist 为全局任务；model_cache_refresh 可按来源运行。

  `config show` 默认脱敏且不显示原文件注释；只有显式使用 `--show-secrets` 才输出完整原文或生效配置。

`crosslane config list` 显示每个模块的适用来源，`show <模块> --effective` 查看合并默认值后的配置。`config edit admin` 与 `config edit status-http` 同时展示管理鉴权及独立监听（默认 127.0.0.1:7651），可原子修改依赖设置；未改设置退出时不写文件、不重载服务，仍提示待重启字段；首次启用详见 `crosslane guide admin`。新建 Copilot 实例会在配置文件中写入默认 Client ID；用 `crosslane config path` 定位文件，或在组织要求自有 OAuth App 时用 `crosslane provider edit copilot --setting 'client_id="Iv1.your-app"'` 覆盖。Kiro SSO 用 `config edit sso`，不是 Copilot 的 Azure SSO URL。`config reset <模块>` 只重置所选模块，status-http 另会关闭 REST 并保留令牌，admin 重置会清除令牌；不指定模块会重置 Kiro 与全局运行参数、清除管理令牌和模型映射、关闭 REST/状态监听，同时保留提供源、API key、服务、模型禁用规则和告警。socket 默认值取自 daemon；旧 daemon 未提供默认值时保留当前 socket。server.host/port 仅是新建服务默认值，已有服务用 service edit 修改；status_http 支持热重载；admin.socket 和 TLS 模式切换需重启 daemon。"#
        }
        "apikey" => {
            "API key 可用 `--provider kiro`、`--provider copilot` 限定允许来源；add 省略时保持 Kiro-only，重复传入可允许两者，并可用 `--model 'copilot/*'` 限制模型。service create 自动生成的 key 继承服务来源范围。`apikey list/usage/history --provider <ID>` 是查询过滤；`limit` 的累计 credits 上限跨该 key 的来源共用，`reset-usage` 清除全部来源累计用量，不单独按来源重置。`show/edit` 可查看或修改范围；删除需确认。"
        }
        "diagnose" => {
            "`crosslane diagnose all/endpoints/account` 是 Kiro 上游专项诊断：检查 CodeWhisperer/AmazonQ/OIDC 端点，并对 Kiro 账号发起真实推理。Copilot 请用 `crosslane account probe --provider copilot <ACCOUNT_ID>` 和 `crosslane models list --provider copilot --refresh` 检查账号与模型；不要把 Kiro 端点诊断结果当成 Copilot 可用性。"
        }
        "subscriptions" => {
            "`crosslane subscriptions --provider kiro [账号]` 用已启用的 Kiro 账号查询上游提供的订阅计划。省略账号时自动选择一个已启用账号。通过 AWS 管理凭证按用户查询或管理企业订阅，请用 `crosslane admin kiro subscription`。写命令支持 --dry-run，取消默认在周期结束。Copilot 来源返回 supported=false。"
        }
        "tasks" => {
            "`crosslane tasks list` 查看全部任务及可手工运行列；`tasks run --help` 列出接受的任务名，Shell 补全也提供这些名称。account_file_watcher/adaptive_admission 仅由调度器运行。`token_refresh/status_check/health_recheck` 操作 Kiro 账号池；`model_cache_refresh` 可用 `tasks run model_cache_refresh --provider <ID>` 按来源刷新；`stats_persist/daily_reset/proxy_service_reconcile` 为全局任务。Kiro 任务只接受 --provider kiro；全局任务不接受 --provider。Copilot API token 在其 provider 内独立按需刷新，不使用 Kiro 的 token_refresh 任务。"
        }
        "stats" => {
            "`crosslane stats` 默认聚合 Kiro/Copilot 的持久化请求统计；`--provider <ID>` 限定来源，`--detail --by provider` 比较来源分组。可用 `--since 1h` 或带时区的 `--start/--end` 查询时间段；`--by model|account|apikey|endpoint` 查看其他维度。"
        }
        "logs" => {
            "`crosslane logs show` 和 `crosslane logs follow` 查看近期 Service 请求。--provider 精确匹配实例 ID，--account 精确匹配日志中的 account_id。查看全部来源时省略 --provider 并取消设置 CROSSLANE_PROVIDER。--level 按 HTTP 状态筛选，--json 每条请求输出一行 JSON。\n`crosslane logs trace TRACE_ID` 从已保留的 Service/daemon 文件查链路，TRACE_ID 来自 x-trace-id 响应头。\n`crosslane logs admin show --request-id ADMIN_ID` 查询独立的 Admin 日志，ADMIN_ID 来自 HTTP Admin 的 x-request-id 响应头。也可直接运行 `crosslane logs admin`。Admin 的 --level 按文件级别筛选，HTTP 审计事件记录在 info 文件中。`logs files` 和 `logs path` 加 --admin 可定位 Admin 文件。"
        }
        "alert" => {
            "`crosslane alert events` 当前列出的额度保护、额度耗尽、服务额度耗尽和 token 刷新失败事件来自 Kiro 账号池；不能作为 Copilot Enterprise 席位、授权或额度监控。`alert platforms/config` 和 `add/edit/delete/list/test/logs` 管理这些事件的通知目标；通知平台本身是共享机制，但当前没有 Copilot 专项告警事件。通知目标通过名称定位，不接受 --provider；CROSSLANE_PROVIDER 不影响告警操作。"
        }
        "models" => {
            "`crosslane models list` 默认聚合全部来源；`--provider kiro --refresh` 和 `--provider copilot --refresh` 分别刷新并查看 Kiro、Copilot 的模型、输入上下文和输出上限，`-` 表示上游未返回对应元数据。`--mapped` 同时显示显式映射结果。`crosslane models resolve <MODEL_ID> --provider <ID>` 显示指定来源中的映射与最终模型；还可配合 `--api-key` 和 `--refresh`。`model` 与 `models` 等价。`model disable/enable <MODEL_ID...>` 全局禁用/启用模型（跨全部 provider，使用不带来源前缀的 ID；若有 CROSSLANE_PROVIDER 请覆盖 --provider all）。禁用规则保存在 models.disabled，热更新生效，配置重置保留。CLI 列表标记不可用，公共列表隐藏；直接请求、映射、默认模型、回退、重试和内部探测均在发送前拦截。已发送请求无法撤回。"
        }
        "docker" => {
            "使用 `./deploy/docker-setup.sh --build` 构建 runtime-full，默认启用全部 Cargo feature（包括 sso 和 crs）并包含 Chromium SSO 运行时。数据保存在 crosslane-data 命名卷。\nDocker 宿主机 wrapper 提供 `crosslane restart`、`crosslane stop` 和 `crosslane uninstall`。卸载先备份数据，默认保留备份。可用 `crosslane uninstall --help` 查看确认方式和备份参数。\n容器停止后，wrapper 可用该容器对应的镜像显示 CLI 帮助，仍需 Docker 和本地镜像。"
        }
        _ => {
            return Err(anyhow!(
                "未知指南主题 {topic}；可用主题：{}",
                TOPICS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_topics_match_clap_values() {
        let values = Topic::value_variants()
            .iter()
            .map(|topic| topic.as_str())
            .collect::<Vec<_>>();
        let documented = TOPICS.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        assert_eq!(values, documented);
    }

    #[test]
    fn provider_guides_cover_both_auth_flows_and_sso_boundary() {
        assert!(topic_text("kiro").unwrap().contains("account add-sso"));
        assert!(topic_text("copilot").unwrap().contains("device-flow"));
        assert!(topic_text("copilot").unwrap().contains("无痕/隐私窗口"));
        assert!(topic_text("copilot").unwrap().contains("SSH 无头服务器"));
        assert!(topic_text("copilot")
            .unwrap()
            .contains("provider add --id copilot --kind copilot\n"));
        assert!(topic_text("copilot")
            .unwrap()
            .contains("--default-provider copilot"));
        assert!(topic_text("sso").unwrap().contains("仅适用于 Kiro"));
        assert!(topic_text("diagnose")
            .unwrap()
            .contains("Kiro 上游专项诊断"));
        assert_eq!(
            Topic::from_str("github-copilot", false).unwrap().as_str(),
            "copilot"
        );
    }

    #[test]
    fn command_and_config_guides_describe_provider_scope() {
        let provider = topic_text("provider").unwrap();
        assert!(provider.contains("Kiro 专项"));
        assert!(provider.contains("Copilot 专项"));
        assert!(provider.contains("全局"));

        let config = topic_text("config").unwrap();
        assert!(config.contains("server、log、admin"));
        assert!(config.contains("upstream、pool"));
        assert!(config.contains("client_id"));

        assert!(topic_text("pool").unwrap().contains("--explain 不提供"));
        assert!(topic_text("model-map").unwrap().contains("仅适用于 Kiro"));
        assert!(topic_text("subscriptions")
            .unwrap()
            .contains("supported=false"));
        assert!(topic_text("alert").unwrap().contains("不能作为 Copilot"));
    }
}
