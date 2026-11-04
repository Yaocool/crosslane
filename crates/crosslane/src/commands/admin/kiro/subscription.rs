use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use crosslane_ipc::kiro_admin::EnterpriseScope;
use crosslane_ipc::protocol::{method, Response};
use serde_json::{json, Value};
use std::path::Path;

use crate::{
    client::AdminClient,
    output::{print_json, render_table},
};

#[derive(Debug, Args)]
pub struct ScopeArgs {
    /// Kiro 区域；省略时使用 daemon 凭证配置中的值。
    #[arg(long)]
    region: Option<String>,
    /// 指定 Kiro Profile ARN；省略时使用保存的选择或自动发现。
    #[arg(long)]
    profile_arn: Option<String>,
    /// 指定关联的 Identity Center Application ARN。
    #[arg(long)]
    application_arn: Option<String>,
    /// 补充 Identity Center 区域；不能覆盖 Profile 已记录的其他区域。
    #[arg(long)]
    identity_center_region: Option<String>,
    /// 显式使用旧控制面，仅支持 us-east-1；不会自动回退。
    #[arg(long)]
    legacy: bool,
}

impl From<&ScopeArgs> for EnterpriseScope {
    fn from(args: &ScopeArgs) -> Self {
        Self {
            region: args.region.clone(),
            profile_arn: args.profile_arn.clone(),
            application_arn: args.application_arn.clone(),
            identity_center_region: args.identity_center_region.clone(),
            legacy: args.legacy,
        }
    }
}

#[derive(Debug, Args)]
pub struct ChangePlanArgs {
    /// Identity Center 的精确 userName、UserId 或用户 ARN。
    user: String,
    /// 必须明确选择套餐。
    #[arg(long, value_parser = ["pro", "pro-plus", "pro-max", "power"])]
    plan: String,
    /// 只读预览，不向 AWS 提交订阅写入。
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CancelMode {
    #[value(alias = "end_of_period")]
    EndOfPeriod,
    Immediate,
}

impl CancelMode {
    fn wire_name(self) -> &'static str {
        match self {
            Self::EndOfPeriod => "end_of_period",
            Self::Immediate => "immediate",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum SubscriptionCommand {
    /// 列出支持的套餐名；不访问 AWS，也不验证 AWS 权限。
    #[command(
        after_help = "示例：\n  crosslane admin kiro subscription plans\n  crosslane --json admin kiro subscription plans"
    )]
    Plans,
    /// 验证 AWS 身份并列出当前区域的 Kiro Profile。
    #[command(
        after_help = "示例：\n  crosslane admin kiro subscription profiles\n  crosslane admin kiro subscription profiles --region us-east-1"
    )]
    Profiles {
        /// Kiro 区域；省略时使用 daemon 凭证配置中的值。
        #[arg(long)]
        region: Option<String>,
        /// 显式使用旧控制面，仅支持 us-east-1。
        #[arg(long)]
        legacy: bool,
    },
    /// 发现并验证 Profile、Identity Center 实例、应用和身份库。
    #[command(
        after_help = "示例：\n  crosslane admin kiro subscription scope\n  crosslane admin kiro subscription scope --region us-east-1 --profile-arn PROFILE_ARN"
    )]
    Scope {
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// 列出身份库全部用户、直接订阅和应用的 GROUP 订阅。
    #[command(
        after_help = "用户列表不逐一展开组继承关系；查询某人的组继承订阅请用 subscription show <USER>。\n\n示例：\n  crosslane admin kiro subscription users\n  crosslane admin kiro subscription users --application-arn APPLICATION_ARN"
    )]
    Users {
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// 列出所选应用的 USER/GROUP 订阅及待生效套餐。
    #[command(
        after_help = "示例：\n  crosslane admin kiro subscription list\n  crosslane --json admin kiro subscription list --region us-east-1"
    )]
    List {
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// 查询用户的直接订阅与组继承订阅。
    #[command(
        after_help = "示例：\n  crosslane admin kiro subscription show user@example.com\n  crosslane --json admin kiro subscription show USER_ID --profile-arn PROFILE_ARN"
    )]
    Show {
        /// Identity Center 的精确 userName、UserId 或用户 ARN。
        user: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// 为 Identity Center 用户开通套餐；已有直接订阅时拒绝重复开通。
    #[command(
        visible_aliases = ["create", "activate"],
        after_help = "使用 crosslane admin kiro configure 保存的 AWS 凭证，不接收 AK/SK 或 Admin token。\n默认实际开通，AWS 可以自动分配 Kiro 应用；Crosslane 不发送额外重复的 SSO 写请求。\n--dry-run 只做只读发现与操作校验，不提交开通，也不能验证所有 AWS 写权限。\n请求只提交一次；AWS 已受理不等于权益或计费已生效，超时或中断后先 show 查询，避免重复开通。\n必要的下游权限包括 user-subscriptions:CreateClaim、sso-directory:DescribeUser 和 sso:CreateApplicationAssignment。\n\n示例：\n  crosslane admin kiro subscription add user@example.com --plan power\n  crosslane admin kiro subscription add user@example.com --plan pro-plus --dry-run\n  crosslane --json admin kiro subscription add USER_ID --plan pro --region us-east-1\n  crosslane admin kiro subscription show user@example.com"
    )]
    Add(ChangePlanArgs),
    /// 升级唯一的直接订阅；目标套餐必须高于当前套餐。
    #[command(
        after_help = "默认实际提交；--dry-run 只读预览，不能验证 AWS 写权限。升级预期即时生效，但 AWS 受理结果仍需 show 确认。\n发生超时或中断时先查询，不自动重试。组继承订阅不能通过此命令修改。\n\n示例：\n  crosslane admin kiro subscription upgrade user@example.com --plan power --dry-run\n  crosslane admin kiro subscription upgrade user@example.com --plan power"
    )]
    Upgrade(ChangePlanArgs),
    /// 降级唯一的直接订阅；目标套餐必须低于当前套餐。
    #[command(
        after_help = "默认实际提交；--dry-run 只读预览，不能验证 AWS 写权限。降级预期下月生效，show 可查看待生效套餐。\n发生超时或中断时先查询，不自动重试。组继承订阅不能通过此命令修改。\n\n示例：\n  crosslane admin kiro subscription downgrade user@example.com --plan pro --dry-run\n  crosslane admin kiro subscription downgrade user@example.com --plan pro"
    )]
    Downgrade(ChangePlanArgs),
    /// 取消直接订阅；保留 Identity Center 用户和组继承订阅。
    #[command(
        visible_aliases = ["delete", "remove", "rm"],
        after_help = "默认实际提交，--mode 默认 end-of-period，在周期结束时取消。immediate 预期立即停用，但计费仍可能至当月底。\n--legacy 必须明确使用 --mode immediate；旧控制面不支持周期结束取消。\n--dry-run 只读预览，不提交写入，也不能验证 AWS 写权限。超时或中断后先 show 查询，不自动重试。\n本命令只取消 Kiro 直接订阅，组继承订阅须由组管理员处理。\n\n示例：\n  crosslane admin kiro subscription cancel user@example.com --dry-run\n  crosslane admin kiro subscription cancel user@example.com\n  crosslane admin kiro subscription cancel USER_ID --mode immediate\n  crosslane admin kiro subscription cancel USER_ID --legacy --mode immediate"
    )]
    Cancel {
        /// Identity Center 的精确 userName、UserId 或用户 ARN。
        user: String,
        /// 周期结束取消（默认）或立即取消。
        #[arg(long, default_value = "end-of-period")]
        mode: CancelMode,
        /// 只读预览，不向 AWS 提交订阅写入。
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        scope: ScopeArgs,
    },
}

impl SubscriptionCommand {
    fn mutation_target(&self) -> Option<(&str, &ScopeArgs, bool)> {
        match self {
            Self::Add(input) | Self::Upgrade(input) | Self::Downgrade(input) => {
                Some((&input.user, &input.scope, input.dry_run))
            }
            Self::Cancel {
                user,
                scope,
                dry_run,
                ..
            } => Some((user, scope, *dry_run)),
            _ => None,
        }
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("-")
}

fn plan(claim: &Value, pending: bool) -> &str {
    let (name, raw) = if pending {
        ("pending_plan", "pending_subscription_type")
    } else {
        ("plan", "subscription_type")
    };
    claim[name]
        .as_str()
        .or_else(|| claim[raw].as_str())
        .unwrap_or("-")
}

fn claim_row(claim: &Value, source: &str) -> Vec<String> {
    vec![
        source.into(),
        text(&claim["principal_id"]).into(),
        plan(claim, false).into(),
        plan(claim, true).into(),
    ]
}

fn array<'a>(value: &'a Value, name: &str) -> Result<&'a [Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("Kiro 响应缺少 {name} 数组"))
}

fn shell_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.@/:".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn query_hint(user: &str, scope: &ScopeArgs, socket: &Path) -> String {
    let mut command = format!(
        "crosslane --socket {} admin kiro subscription show {}",
        shell_arg(&socket.to_string_lossy()),
        shell_arg(user)
    );
    for (flag, value) in [
        ("--region", &scope.region),
        ("--profile-arn", &scope.profile_arn),
        ("--application-arn", &scope.application_arn),
        ("--identity-center-region", &scope.identity_center_region),
    ] {
        if let Some(value) = value {
            command.push_str(&format!(" {flag} {}", shell_arg(value)));
        }
    }
    if scope.legacy {
        command.push_str(" --legacy");
    }
    command
}

fn print_result(command: &SubscriptionCommand, result: &Value, socket: &Path) -> Result<()> {
    match command {
        SubscriptionCommand::Plans => {
            let rows = array(result, "plans")?
                .iter()
                .map(|p| {
                    vec![
                        text(&p["plan"]).into(),
                        text(&p["subscription_type"]).into(),
                    ]
                })
                .collect::<Vec<_>>();
            println!("{}", render_table(&["套餐", "AWS 套餐类型"], &rows));
        }
        SubscriptionCommand::Profiles { .. } => {
            println!(
                "AWS 账号：{}\nKiro 区域：{}",
                text(&result["account_id"]),
                text(&result["kiro_region"])
            );
            let rows = array(&result["profiles"], "profiles")?
                .iter()
                .map(|profile| {
                    vec![
                        text(&profile["arn"]).into(),
                        text(&profile["instance_arn"]).into(),
                        text(&profile["identity_center_region"]).into(),
                        profile["application_arns"]
                            .as_array()
                            .map(|arns| arns.iter().map(text).collect::<Vec<_>>().join(", "))
                            .unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                render_table(
                    &[
                        "Profile ARN",
                        "实例 ARN",
                        "Identity Center 区域",
                        "应用 ARN"
                    ],
                    &rows
                )
            );
        }
        SubscriptionCommand::Scope { .. } => {
            let rows = [
                "account_id",
                "kiro_region",
                "profile_arn",
                "instance_arn",
                "identity_center_region",
                "application_arn",
                "identity_store_id",
            ]
            .map(|key| vec![key.into(), text(&result[key]).into()]);
            println!("{}", render_table(&["范围", "值"], &rows));
            println!("旧控制面：{}", result["legacy"].as_bool().unwrap_or(false));
        }
        SubscriptionCommand::Users { .. } => {
            let rows = array(&result["users"], "users")?
                .iter()
                .map(|user| {
                    let claims = array(&user["direct_subscriptions"], "direct_subscriptions")?;
                    let plans = |pending| {
                        claims
                            .iter()
                            .map(|claim| plan(claim, pending))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    Ok(vec![
                        text(&user["user_name"]).into(),
                        text(&user["user_id"]).into(),
                        text(&user["display_name"]).into(),
                        plans(false),
                        plans(true),
                    ])
                })
                .collect::<Result<Vec<_>>>()?;
            println!(
                "{}",
                render_table(
                    &["用户名", "UserId", "显示名", "直接订阅", "待生效套餐"],
                    &rows
                )
            );
            println!("应用 GROUP 订阅（未逐一展开用户组成员关系）：");
            let groups = array(&result["group_subscriptions"], "group_subscriptions")?
                .iter()
                .map(|claim| claim_row(claim, "GROUP"))
                .collect::<Vec<_>>();
            println!(
                "{}",
                render_table(&["类型", "主体 ID", "当前套餐", "待生效套餐"], &groups)
            );
        }
        SubscriptionCommand::List { .. } => {
            let rows = array(&result["subscriptions"], "subscriptions")?
                .iter()
                .map(|claim| claim_row(claim, text(&claim["principal_type"])))
                .collect::<Vec<_>>();
            println!(
                "{}",
                render_table(&["类型", "主体 ID", "当前套餐", "待生效套餐"], &rows)
            );
        }
        SubscriptionCommand::Show { .. } => {
            let user = &result["user"];
            println!(
                "用户：{}\nUserId：{}",
                text(&user["user_name"]),
                text(&user["user_id"])
            );
            let mut rows = array(&user["direct_subscriptions"], "direct_subscriptions")?
                .iter()
                .map(|claim| claim_row(claim, "直接订阅"))
                .collect::<Vec<_>>();
            rows.extend(
                array(&result["group_subscriptions"], "group_subscriptions")?
                    .iter()
                    .map(|claim| claim_row(claim, "组继承")),
            );
            println!(
                "{}",
                render_table(&["来源", "主体 ID", "当前套餐", "待生效套餐"], &rows)
            );
        }
        SubscriptionCommand::Add(input) => print_mutation("开通", &input.scope, result, socket)?,
        SubscriptionCommand::Upgrade(input) => {
            print_mutation("升级", &input.scope, result, socket)?
        }
        SubscriptionCommand::Downgrade(input) => {
            print_mutation("降级", &input.scope, result, socket)?
        }
        SubscriptionCommand::Cancel { scope, .. } => print_mutation("取消", scope, result, socket)?,
    }
    Ok(())
}

fn print_mutation(action: &str, scope: &ScopeArgs, result: &Value, socket: &Path) -> Result<()> {
    match result["status"].as_str() {
        Some("preview") => println!("预览通过，未提交订阅{action}请求。"),
        Some("api_accepted") => println!("AWS 已受理 Kiro 订阅{action}请求，请查询确认生效状态。"),
        _ => bail!("无法确认 AWS 订阅变更结果，请先查询订阅状态，不要重复提交"),
    }
    println!("UserId：{}", text(&result["user_id"]));
    for (field, label) in [
        ("previous_plan", "当前套餐"),
        ("requested_plan", "目标套餐"),
        ("deletion_mode", "取消方式"),
        ("upstream_request_id", "AWS request ID"),
    ] {
        if let Some(value) = result[field].as_str() {
            println!("{label}：{value}");
        }
    }
    if let Some(effect) = result["expected_effect"].as_str() {
        let effect = match effect {
            "after_activation" => "开通后生效",
            "immediate" => "即时生效",
            "next_month" => "下月生效",
            "end_of_month" => "周期结束时取消",
            "access_immediate_billing_end_of_month" => "立即停用，计费至当月底",
            other => other,
        };
        println!("预期效果：{effect}（以 AWS 实际状态为准）");
    }
    println!(
        "查询：{}",
        query_hint(text(&result["user_id"]), scope, socket)
    );
    Ok(())
}

pub(super) async fn run(
    client: &mut AdminClient,
    command: SubscriptionCommand,
    json_output: bool,
) -> Result<()> {
    let scope_value = |scope: &ScopeArgs| json!(EnterpriseScope::from(scope));
    let (method, params) = match &command {
        SubscriptionCommand::Plans => (method::ADMIN_KIRO_SUBSCRIPTION_PLANS, Value::Null),
        SubscriptionCommand::Profiles { region, legacy } => (
            method::ADMIN_KIRO_SUBSCRIPTION_PROFILES,
            json!({"scope":EnterpriseScope { region:region.clone(), legacy:*legacy, ..Default::default() }}),
        ),
        SubscriptionCommand::Scope { scope } => (
            method::ADMIN_KIRO_SUBSCRIPTION_SCOPE,
            json!({"scope":scope_value(scope)}),
        ),
        SubscriptionCommand::Users { scope } => (
            method::ADMIN_KIRO_SUBSCRIPTION_USERS,
            json!({"scope":scope_value(scope)}),
        ),
        SubscriptionCommand::List { scope } => (
            method::ADMIN_KIRO_SUBSCRIPTION_LIST,
            json!({"scope":scope_value(scope)}),
        ),
        SubscriptionCommand::Show { user, scope } => (
            method::ADMIN_KIRO_SUBSCRIPTION_SHOW,
            json!({"user":user,"scope":scope_value(scope)}),
        ),
        SubscriptionCommand::Add(input)
        | SubscriptionCommand::Upgrade(input)
        | SubscriptionCommand::Downgrade(input) => {
            let method = match &command {
                SubscriptionCommand::Add(_) => method::ADMIN_KIRO_SUBSCRIPTION_ADD,
                SubscriptionCommand::Upgrade(_) => method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
                _ => method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
            };
            (
                method,
                json!({"user":input.user,"plan":input.plan,"dry_run":input.dry_run,"scope":scope_value(&input.scope)}),
            )
        }
        SubscriptionCommand::Cancel {
            user,
            mode,
            dry_run,
            scope,
        } => {
            if scope.legacy && *mode != CancelMode::Immediate {
                bail!("--legacy 仅支持 --mode immediate；不会自动改为立即取消");
            }
            (
                method::ADMIN_KIRO_SUBSCRIPTION_CANCEL,
                json!({"user":user,"mode":mode.wire_name(),"dry_run":dry_run,"scope":scope_value(scope)}),
            )
        }
    };
    let socket = client.socket_path();
    let response = client
        .call_response(method, params)
        .await
        .map_err(|error| {
            if let Some((user, scope, false)) = command.mutation_target() {
                error.context(format!(
                    "未能取得订阅变更结果，请先查询再决定是否重试：{}",
                    query_hint(user, scope, &socket)
                ))
            } else {
                error
            }
        })?;
    let result = match response {
        Response::Ok { result, .. } => result,
        Response::Err { error, .. } => {
            let unsupported = error.code == 404 && error.message.starts_with("unknown method:");
            let details = if unsupported {
                json!({"code":"unsupported_daemon","message":"当前 crosslaned 不支持 Kiro 订阅命令，请同时升级 crosslane 和 crosslaned。"})
            } else {
                serde_json::from_str::<Value>(&error.message).ok().filter(Value::is_object).unwrap_or_else(|| json!({"code":"rpc_error","message":error.message,"rpc_code":error.code}))
            };
            if json_output {
                print_json(&json!({"error":details}))?;
                bail!("Kiro 订阅管理失败（{}），详见 JSON 错误信息", error.code);
            }
            let mut message = format!("{}: {}", text(&details["code"]), text(&details["message"]));
            for key in [
                "operation",
                "upstream_code",
                "upstream_request_id",
                "write_outcome",
            ] {
                if let Some(value) = details[key].as_str() {
                    message.push_str(&format!("\n{key}: {value}"));
                }
            }
            return Err(anyhow!(message));
        }
    };
    if json_output {
        print_json(&result)
    } else {
        print_result(&command, &result, &socket)
    }
}
