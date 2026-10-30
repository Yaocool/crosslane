//! Admin REST credentials use the trusted control socket, not a Bearer token.

use anyhow::Result;
use clap::Subcommand;
use crosslane_ipc::protocol::{
    method, AdminTokenListResult, AdminTokenResult, AdminTokenView, CreatedAdminToken,
};

use crate::client::AdminClient;
use crate::output::{format_timestamp, print_json, render_table};

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// 管理独立 REST 接口的令牌，与业务 apikey 分开。
    #[command(
        after_help = "示例：\n  crosslane admin token create --name ops --enable-rest\n  crosslane admin token create --name monitor --role read-only\n  crosslane admin token list\n  crosslane admin token rotate ops\n  crosslane admin token revoke monitor\n\n命令通过 Unix socket 执行，不需要先有 Admin Bearer token。明文仅在 create/rotate 返回，配置只保存摘要。"
    )]
    Token {
        #[command(subcommand)]
        command: Option<AdminTokenCommand>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AdminTokenCommand {
    /// 创建独立管理令牌；明文只返回一次。
    #[command(
        visible_alias = "add",
        after_help = "示例：\n  crosslane admin token create --name ops --enable-rest\n  crosslane admin token create --name monitor --role read-only\n  crosslane --json admin token create --name ci"
    )]
    Create {
        /// 令牌的唯一可读名称。
        #[arg(long)]
        name: String,
        /// admin 为完整权限，read-only 只允许 GET / HEAD。
        #[arg(long,default_value="admin",value_parser=["admin","read-only","read_only"])]
        role: String,
        /// 同时开启 Admin REST；要求独立状态监听已经启用并重启生效。
        #[arg(long)]
        enable_rest: bool,
    },
    /// 查看令牌元数据，不显示明文或摘要。
    #[command(
        after_help = "示例：\n  crosslane admin token list\n  crosslane --json admin token list"
    )]
    List,
    /// 查看指定令牌元数据，参数接受 ID 或名称。
    #[command(
        after_help = "示例：\n  crosslane admin token show ops\n  crosslane admin token show legacy-admin"
    )]
    Show {
        /// 令牌 ID 或名称，见 admin token list。
        id: String,
    },
    /// 轮换令牌并返回新明文，旧令牌对后续请求立即失效；保留原权限与启停状态。
    #[command(
        after_help = "示例：\n  crosslane admin token rotate ops\n  crosslane --json admin token rotate ops"
    )]
    Rotate {
        /// 令牌 ID 或名称，见 admin token list。
        id: String,
    },
    /// 永久撤销指定令牌；最后一把撤销后关闭 Admin REST，公开状态接口保持可用。
    #[command(visible_aliases=["delete","rm"], after_help="示例：\n  crosslane admin token revoke monitor\n  crosslane admin token revoke adm_0123456789abcdef")]
    Revoke {
        /// 令牌 ID 或名称，见 admin token list。
        id: String,
    },
    /// 启用指定令牌。
    #[command(after_help = "示例：\n  crosslane admin token enable monitor")]
    Enable {
        /// 令牌 ID 或名称，见 admin token list。
        id: String,
    },
    /// 停用指定令牌，保留元数据，之后可重新启用。
    #[command(after_help = "示例：\n  crosslane admin token disable monitor")]
    Disable {
        /// 令牌 ID 或名称，见 admin token list。
        id: String,
    },
}

fn rows(tokens: &[AdminTokenView]) -> Vec<Vec<String>> {
    tokens
        .iter()
        .map(|token| {
            vec![
                token.id.clone(),
                token.name.clone(),
                token.role.as_str().into(),
                if token.enabled { "启用" } else { "停用" }.into(),
                token
                    .created_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "旧配置".into()),
            ]
        })
        .collect()
}
fn print_tokens(tokens: &[AdminTokenView]) {
    println!(
        "{}",
        render_table(&["ID", "名称", "权限", "状态", "创建时间"], &rows(tokens))
    );
}
fn hint(rest: bool, listener: bool) {
    if !listener {
        eprintln!("独立状态监听未启用：请配置 [status_http] enabled=true，并重启 daemon。");
    }
    if !rest {
        eprintln!("Admin REST 当前关闭；监听启用后可用 crosslane admin token create --name NAME --enable-rest，或配置 [admin.rest] enabled=true。");
    }
}

pub async fn run(client: &mut AdminClient, command: AdminCommand, json: bool) -> Result<()> {
    let AdminCommand::Token {
        command: Some(command),
    } = command
    else {
        unreachable!("empty command group handled locally")
    };
    match command {
        AdminTokenCommand::Create {
            name,
            role,
            enable_rest,
        } => {
            let result:CreatedAdminToken=client.call(method::ADMIN_TOKEN_CREATE,serde_json::json!({"name":name,"role":role.replace('-',"_"),"enable_rest":enable_rest})).await?;
            print_created(&result, json)?;
        }
        AdminTokenCommand::Rotate { id } => {
            let result: CreatedAdminToken = client
                .call(method::ADMIN_TOKEN_ROTATE, serde_json::json!({"id":id}))
                .await?;
            print_created(&result, json)?;
        }
        AdminTokenCommand::List => {
            let result: AdminTokenListResult = client
                .call(method::ADMIN_TOKEN_LIST, serde_json::Value::Null)
                .await?;
            if json {
                print_json(&result)?;
            } else {
                print_tokens(&result.tokens);
                hint(result.rest_enabled, result.listener_enabled);
            }
        }
        command => {
            let (method, id, enabled) = match command {
                AdminTokenCommand::Show { id } => (method::ADMIN_TOKEN_SHOW, id, None),
                AdminTokenCommand::Revoke { id } => (method::ADMIN_TOKEN_REVOKE, id, None),
                AdminTokenCommand::Enable { id } => {
                    (method::ADMIN_TOKEN_SET_ENABLED, id, Some(true))
                }
                AdminTokenCommand::Disable { id } => {
                    (method::ADMIN_TOKEN_SET_ENABLED, id, Some(false))
                }
                _ => unreachable!(),
            };
            let params = if let Some(enabled) = enabled {
                serde_json::json!({"id":id,"enabled":enabled})
            } else {
                serde_json::json!({"id":id})
            };
            let result: AdminTokenResult = client.call(method, params).await?;
            if json {
                print_json(&result)?;
            } else {
                print_tokens(&[result.token]);
                if method == method::ADMIN_TOKEN_REVOKE {
                    println!("令牌已撤销，对后续请求立即失效。");
                }
                hint(result.rest_enabled, result.listener_enabled);
            }
        }
    }
    Ok(())
}

fn print_created(result: &CreatedAdminToken, json: bool) -> Result<()> {
    if json {
        return print_json(result);
    }
    print_tokens(std::slice::from_ref(&result.token));
    println!("令牌明文（仅本次显示，请保存）：\n{}", result.secret);
    hint(result.rest_enabled, result.listener_enabled);
    Ok(())
}
