use super::{
    alert_event_array_value, anyhow, find_named_table_mut, insert_optional_string, method,
    mutate_config_array, named_value_matches, print_json, remove_named_value, render_table,
    replace_optional_string, replace_or_clear_optional_string, simple_rpc, AdminClient,
    AlertCommand, AlertEvent, AlertPlatform, Result,
};

pub async fn run_alert(client: &mut AdminClient, command: AlertCommand, json: bool) -> Result<()> {
    match command {
        AlertCommand::Config => show_alert_config(json),
        AlertCommand::Events => show_alert_events(json),
        AlertCommand::Platforms => show_alert_platforms(json),
        AlertCommand::List => {
            simple_rpc(client, method::WEBHOOK_LIST, serde_json::json!({}), json).await
        }
        AlertCommand::Add {
            name,
            platform,
            webhook_url,
            events,
            disabled,
            dingtalk_sign,
            telegram_chat_id,
            custom_template,
        } => {
            mutate_config_array(client, "webhook", |array| {
                if array.iter().any(|value| named_value_matches(value, &name)) {
                    return Err(anyhow!("告警目标已存在: {name}"));
                }
                let mut table = toml::map::Map::new();
                table.insert("name".into(), toml::Value::String(name.clone()));
                table.insert("kind".into(), toml::Value::String(platform.as_str().into()));
                table.insert("url".into(), toml::Value::String(webhook_url.clone()));
                table.insert("enabled".into(), toml::Value::Boolean(!disabled));
                table.insert("events".into(), alert_event_array_value(&events));
                insert_optional_string(&mut table, "dingtalk_sign", dingtalk_sign.as_deref());
                insert_optional_string(&mut table, "telegram_chat_id", telegram_chat_id.as_deref());
                insert_optional_string(&mut table, "custom_template", custom_template.as_deref());
                array.push(toml::Value::Table(table));
                Ok(())
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"name": name, "created": true}))
            } else {
                println!("已添加告警目标 {name}");
                Ok(())
            }
        }
        AlertCommand::Edit {
            target,
            name,
            rename,
            platform,
            webhook_url,
            events,
            clear_events,
            enable,
            disable,
            dingtalk_sign,
            clear_dingtalk_sign,
            telegram_chat_id,
            clear_telegram_chat_id,
            custom_template,
            clear_custom_template,
        } => {
            if rename.is_none()
                && platform.is_none()
                && webhook_url.is_none()
                && events.is_empty()
                && !clear_events
                && !enable
                && !disable
                && dingtalk_sign.is_none()
                && !clear_dingtalk_sign
                && telegram_chat_id.is_none()
                && !clear_telegram_chat_id
                && custom_template.is_none()
                && !clear_custom_template
            {
                return Err(anyhow!("没有指定修改项；请至少提供一个 edit 选项"));
            }
            let name = target
                .or(name)
                .ok_or_else(|| anyhow!("需指定告警目标名称"))?;
            mutate_config_array(client, "webhook", |array| {
                let table = find_named_table_mut(array, &name, "告警目标")?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "kind", platform.map(AlertPlatform::as_str));
                replace_optional_string(table, "url", webhook_url.as_deref());
                replace_or_clear_optional_string(
                    table,
                    "dingtalk_sign",
                    dingtalk_sign.as_deref(),
                    clear_dingtalk_sign,
                );
                replace_or_clear_optional_string(
                    table,
                    "telegram_chat_id",
                    telegram_chat_id.as_deref(),
                    clear_telegram_chat_id,
                );
                replace_or_clear_optional_string(
                    table,
                    "custom_template",
                    custom_template.as_deref(),
                    clear_custom_template,
                );
                if clear_events {
                    table.insert("events".into(), toml::Value::Array(Vec::new()));
                } else if !events.is_empty() {
                    table.insert("events".into(), alert_event_array_value(&events));
                }
                if enable || disable {
                    table.insert("enabled".into(), toml::Value::Boolean(enable));
                }
                Ok(())
            })
            .await?;
            if json {
                print_json(&serde_json::json!({
                    "name": name,
                    "renamed_to": rename,
                    "updated": true
                }))
            } else {
                if let Some(rename) = rename {
                    println!("已更新告警目标 {name} -> {rename}");
                } else {
                    println!("已更新告警目标 {name}");
                }
                Ok(())
            }
        }
        AlertCommand::Delete { name, yes } => {
            if !crate::commands::confirm_unless(yes, &format!("确认删除告警目标 {name}？")).await?
            {
                if json {
                    print_json(&serde_json::json!({
                        "name": name,
                        "deleted": false,
                        "cancelled": true
                    }))?;
                } else {
                    println!("已取消");
                }
                return Ok(());
            }
            mutate_config_array(client, "webhook", |array| {
                remove_named_value(array, &name, "告警目标")
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"name": name, "deleted": true}))
            } else {
                println!("已删除告警目标 {name}");
                Ok(())
            }
        }
        AlertCommand::Test { name, all } => {
            if name.is_none() && !all {
                return Err(anyhow!("需指定告警目标名称或 --all"));
            }
            simple_rpc(
                client,
                method::WEBHOOK_TEST,
                serde_json::json!({"name":name}),
                json,
            )
            .await
        }
        AlertCommand::Logs { tail } => {
            simple_rpc(
                client,
                method::WEBHOOK_LOGS,
                serde_json::json!({"tail":tail}),
                json,
            )
            .await
        }
    }
}

#[derive(serde::Serialize)]
struct AlertEventInfo {
    event: &'static str,
    provider_scope: &'static str,
    condition: &'static str,
}

fn alert_event_catalog() -> [AlertEventInfo; 4] {
    [
        AlertEventInfo {
            event: AlertEvent::AccountCreditProtected.as_str(),
            provider_scope: "kiro",
            condition:
                "overage 关闭时，单个启用账号达到 pool.low_credit_min_remaining 保护阈值并暂停调度；overage 开启时不触发此保护告警。",
        },
        AlertEventInfo {
            event: AlertEvent::AccountQuotaExhausted.as_str(),
            provider_scope: "kiro",
            condition:
                "单个启用账号达到代理有效额度上限，或 Kiro 上游实际返回额度耗尽；同一次异常只告警一次，恢复后才允许再次告警。",
        },
        AlertEventInfo {
            event: AlertEvent::ServiceQuotaExhausted.as_str(),
            provider_scope: "kiro",
            condition:
                "API 代理服务共享的全部启用账号达到各自有效额度上限或被 Kiro 判定额度耗尽；服务恢复前只告警一次。",
        },
        AlertEventInfo {
            event: AlertEvent::TokenRefreshFailed.as_str(),
            provider_scope: "kiro",
            condition: "后台或请求触发的 Token 刷新失败；同一账号刷新成功前只告警一次。",
        },
    ]
}

pub fn show_alert_events(json: bool) -> Result<()> {
    let events = alert_event_catalog();
    if json {
        return print_json(&events);
    }
    let rows = events
        .iter()
        .map(|event| {
            vec![
                event.event.into(),
                event.provider_scope.into(),
                event.condition.into(),
            ]
        })
        .collect::<Vec<_>>();
    println!("{}", render_table(&["EVENT", "提供源", "触发条件"], &rows));
    println!("\n多选方式：重复使用 `--event`，或用逗号分隔多个事件。");
    Ok(())
}

#[derive(serde::Serialize)]
struct AlertPlatformInfo {
    platform: &'static str,
    description: &'static str,
    platform_options: &'static str,
}

fn alert_platform_catalog() -> [AlertPlatformInfo; 6] {
    [
        AlertPlatformInfo {
            platform: AlertPlatform::Dingtalk.as_str(),
            description: "钉钉群机器人",
            platform_options: "--dingtalk-sign（机器人启用加签时使用）",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::WechatWork.as_str(),
            description: "企业微信群机器人",
            platform_options: "无",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Feishu.as_str(),
            description: "飞书群机器人",
            platform_options: "无",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Telegram.as_str(),
            description: "Telegram Bot API",
            platform_options: "--telegram-chat-id（必填）",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Discord.as_str(),
            description: "Discord Webhook",
            platform_options: "无",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Custom.as_str(),
            description: "自定义 Webhook",
            platform_options: "--custom-template（可选）",
        },
    ]
}

pub fn show_alert_platforms(json: bool) -> Result<()> {
    let platforms = alert_platform_catalog();
    if json {
        return print_json(&platforms);
    }
    let rows = platforms
        .iter()
        .map(|platform| {
            vec![
                platform.platform.into(),
                platform.description.into(),
                platform.platform_options.into(),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        render_table(&["PLATFORM", "通知平台", "平台专用参数"], &rows)
    );
    println!("\n所有平台都需要 --webhook-url，并通过 --platform 选择通知平台。");
    Ok(())
}

pub fn show_alert_config(json: bool) -> Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "mode":"once_until_recovery",
            "format":"markdown",
            "account_event_aggregation":"same_target_and_event_type",
            "events":alert_event_catalog().map(|event| event.event),
        }));
    }
    println!("告警模式  异常期间只发送一次，恢复后再次异常才重新告警");
    println!("账号聚合  同一目标的同类型多账号事件合并发送");
    println!("消息格式  Markdown");
    println!("事件类型  账号剩余额度保护、单账号额度耗尽、服务全部账号额度耗尽、Token 刷新失败");
    Ok(())
}
