use super::{
    anyhow, clear_key_field_and_reload, generate_key, key_id, matches_key, method,
    mutate_config_array, mutate_key_and_reload, print_json, render_table, show_keys, simple_rpc,
    AdminClient, ApiKeyCommand, Context, Deserialize, Result,
};

pub async fn run_apikey(
    client: &mut AdminClient,
    command: ApiKeyCommand,
    json: bool,
) -> Result<()> {
    match command {
        ApiKeyCommand::List { detail, provider } => {
            show_key_list(client, detail, provider.as_deref(), json).await
        }
        ApiKeyCommand::Show { id } => show_keys(client, Some(&id), None, None, json).await,
        ApiKeyCommand::Usage { id, provider } => {
            show_keys(client, Some(&id), None, provider.as_deref(), json).await
        }
        ApiKeyCommand::History { id, tail, provider } => {
            show_keys(client, Some(&id), Some(tail), provider.as_deref(), json).await
        }
        ApiKeyCommand::ResetUsage { id, yes } => {
            if !crate::commands::confirm_unless(
                yes,
                &format!("确认重置 API key {id} 的全部用量统计？"),
            )
            .await?
            {
                if json {
                    print_json(&serde_json::json!({
                        "id": id,
                        "reset": false,
                        "cancelled": true
                    }))?;
                } else {
                    println!("已取消");
                }
                return Ok(());
            }
            simple_rpc(
                client,
                method::APIKEY_RESET_USAGE,
                serde_json::json!({"id":id}),
                json,
            )
            .await
        }
        ApiKeyCommand::Add {
            name,
            format,
            key,
            credits_limit,
            skip_user_agent_check,
            providers,
            models,
        } => {
            let key = resolve_api_key_value(&format, key.as_deref())?;
            let id = key_id(&key);
            mutate_config_array(client, "api_key", |array| {
                let mut table = toml::map::Map::new();
                table.insert("id".into(), toml::Value::String(id.clone()));
                table.insert("name".into(), toml::Value::String(name.clone()));
                table.insert("key".into(), toml::Value::String(key.clone()));
                table.insert("format".into(), toml::Value::String(format.clone()));
                table.insert("enabled".into(), toml::Value::Boolean(true));
                table.insert(
                    "skip_user_agent_check".into(),
                    toml::Value::Boolean(skip_user_agent_check),
                );
                if let Some(limit) = credits_limit {
                    table.insert("credits_limit".into(), toml::Value::Float(limit));
                }
                if !providers.is_empty() {
                    table.insert(
                        "allowed_providers".into(),
                        super::string_array_value(&providers),
                    );
                }
                if !models.is_empty() {
                    table.insert("allowed_models".into(), super::string_array_value(&models));
                }
                array.push(toml::Value::Table(table));
                Ok(())
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"id":id,"name":name,"key":key}))?;
            } else {
                println!("已创建 {id} ({name})\n请立即保存密钥；之后列表不再显示：\n{key}");
            }
            Ok(())
        }
        ApiKeyCommand::Edit {
            id,
            skip_user_agent_check,
            providers,
            clear_providers,
            models,
            clear_models,
        } => {
            if skip_user_agent_check.is_none()
                && providers.is_empty()
                && !clear_providers
                && models.is_empty()
                && !clear_models
            {
                return Err(anyhow!("没有指定修改项"));
            }
            mutate_config_array(client, "api_key", |array| {
                let table = array
                    .iter_mut()
                    .find(|item| matches_key(item, &id))
                    .and_then(toml::Value::as_table_mut)
                    .ok_or_else(|| anyhow!("API key not found: {id}"))?;
                if let Some(skip) = skip_user_agent_check {
                    table.insert("skip_user_agent_check".into(), toml::Value::Boolean(skip));
                }
                if clear_providers {
                    table.remove("allowed_providers");
                } else if !providers.is_empty() {
                    table.insert(
                        "allowed_providers".into(),
                        super::string_array_value(&providers),
                    );
                }
                if clear_models {
                    table.remove("allowed_models");
                } else if !models.is_empty() {
                    table.insert("allowed_models".into(), super::string_array_value(&models));
                }
                Ok(())
            })
            .await?;
            report_apikey_change(client, &id, "已更新", json).await
        }
        ApiKeyCommand::Rm { id, yes } => {
            if !crate::commands::confirm_unless(yes, &format!("确认删除 API key {id}？")).await?
            {
                if json {
                    print_json(&serde_json::json!({
                        "id": id,
                        "removed": false,
                        "cancelled": true
                    }))?;
                } else {
                    println!("已取消");
                }
                return Ok(());
            }
            mutate_config_array(client, "api_key", |array| {
                let before = array.len();
                array.retain(|value| !matches_key(value, &id));
                (array.len() < before)
                    .then_some(())
                    .ok_or_else(|| anyhow!("API key not found: {id}"))
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"removed":true,"id":id}))
            } else {
                println!("已删除 API key {id}");
                Ok(())
            }
        }
        ApiKeyCommand::Enable { id } => {
            mutate_key_and_reload(client, &id, "enabled", toml::Value::Boolean(true)).await?;
            report_apikey_change(client, &id, "已启用", json).await
        }
        ApiKeyCommand::Disable { id } => {
            mutate_key_and_reload(client, &id, "enabled", toml::Value::Boolean(false)).await?;
            report_apikey_change(client, &id, "已停用", json).await
        }
        ApiKeyCommand::Limit { id, credits, clear } => {
            if clear {
                clear_key_field_and_reload(client, &id, "credits_limit").await?;
            } else {
                let credits = credits
                    .ok_or_else(|| anyhow!("--credits is required unless --clear is used"))?;
                mutate_key_and_reload(client, &id, "credits_limit", toml::Value::Float(credits))
                    .await?;
            }
            report_apikey_change(client, &id, "已更新额度上限", json).await
        }
    }
}

fn resolve_api_key_value(format: &str, provided: Option<&str>) -> Result<String> {
    match format {
        "sk" | "token" | "simple" => {}
        other => return Err(anyhow!("unsupported format: {other}")),
    }
    let Some(key) = provided else {
        return generate_key(format);
    };
    if key.is_empty() {
        return Err(anyhow!("--key must not be empty"));
    }
    if key.trim() != key {
        return Err(anyhow!(
            "--key must not contain leading or trailing whitespace"
        ));
    }
    if key.chars().any(char::is_control) {
        return Err(anyhow!("--key must not contain control characters"));
    }
    Ok(key.to_owned())
}

async fn report_apikey_change(
    client: &mut AdminClient,
    id: &str,
    message: &str,
    json: bool,
) -> Result<()> {
    if json {
        show_keys(client, Some(id), None, None, true).await
    } else {
        println!("{message} API key {id}");
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ApiKeyListEntry {
    id: String,
    name: String,
    enabled: bool,
    #[serde(default)]
    skip_user_agent_check: bool,
    credits_limit: Option<f64>,
    #[serde(default)]
    reserved_credits: f64,
    #[serde(default)]
    usage: ApiKeyListUsage,
}

#[derive(Debug, Default, Deserialize)]
struct ApiKeyListUsage {
    #[serde(default)]
    total_requests: u64,
    #[serde(default)]
    total_credits: f64,
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
}

#[derive(Debug, Default)]
pub(super) struct ApiKeyListSummary {
    total: usize,
    enabled: usize,
    total_requests: u64,
    total_credits: f64,
    reserved_credits: f64,
    total_input_tokens: u64,
    total_output_tokens: u64,
}

impl ApiKeyListSummary {
    pub(super) fn from_entries(entries: &[ApiKeyListEntry]) -> Self {
        entries.iter().fold(Self::default(), |mut summary, entry| {
            summary.total += 1;
            summary.enabled += usize::from(entry.enabled);
            summary.total_requests += entry.usage.total_requests;
            summary.total_credits += entry.usage.total_credits;
            summary.reserved_credits += entry.reserved_credits;
            summary.total_input_tokens += entry.usage.total_input_tokens;
            summary.total_output_tokens += entry.usage.total_output_tokens;
            summary
        })
    }

    fn disabled(&self) -> usize {
        self.total.saturating_sub(self.enabled)
    }
}

async fn show_key_list(
    client: &mut AdminClient,
    detail: bool,
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    let value: serde_json::Value = client
        .call(
            method::APIKEY_LIST,
            serde_json::json!({"provider":provider}),
        )
        .await?;
    let mut entries = serde_json::from_value::<Vec<ApiKeyListEntry>>(value)
        .context("daemon 返回的 API key 列表无效")?;
    entries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    let summary = ApiKeyListSummary::from_entries(&entries);

    if json {
        return print_json(&apikey_list_json(&entries, &summary, detail));
    }
    if entries.is_empty() {
        println!("暂无 API key。");
        return Ok(());
    }

    let rows = entries
        .iter()
        .map(|entry| {
            let mut row = vec![
                entry.id.clone(),
                entry.name.clone(),
                if entry.enabled {
                    "已启用"
                } else {
                    "已停用"
                }
                .into(),
                if entry.skip_user_agent_check {
                    "跳过".into()
                } else {
                    "校验".into()
                },
                entry
                    .credits_limit
                    .map(format_credits)
                    .unwrap_or_else(|| "-".into()),
            ];
            if detail {
                row.extend([
                    entry.usage.total_requests.to_string(),
                    entry.usage.total_input_tokens.to_string(),
                    entry.usage.total_output_tokens.to_string(),
                    format_credits(entry.usage.total_credits),
                    format_credits(entry.reserved_credits),
                ]);
            }
            row
        })
        .collect::<Vec<_>>();
    let headers = if detail {
        vec![
            "ID",
            "名称",
            "状态",
            "User-Agent",
            "Credits 上限",
            "请求",
            "输入 Tokens",
            "输出 Tokens",
            "Credits",
            "预留 Credits",
        ]
    } else {
        vec!["ID", "名称", "状态", "User-Agent", "Credits 上限"]
    };
    println!("{}", render_table(&headers, &rows));
    if detail {
        println!(
            "总计：{} 个 API key，{} 启用 / {} 停用，{} 请求，{} 输入 tokens，{} 输出 tokens，{} credits，{} 预留。",
            summary.total,
            summary.enabled,
            summary.disabled(),
            summary.total_requests,
            summary.total_input_tokens,
            summary.total_output_tokens,
            format_credits(summary.total_credits),
            format_credits(summary.reserved_credits),
        );
    } else {
        println!(
            "总计：{} 个 API key，{} 启用 / {} 停用，{} 请求，{} 输入 tokens，{} 输出 tokens，{} credits，{} 预留。使用 --detail 查看分 key 消耗。",
            summary.total,
            summary.enabled,
            summary.disabled(),
            summary.total_requests,
            summary.total_input_tokens,
            summary.total_output_tokens,
            format_credits(summary.total_credits),
            format_credits(summary.reserved_credits),
        );
    }
    Ok(())
}

pub(super) fn apikey_list_json(
    entries: &[ApiKeyListEntry],
    summary: &ApiKeyListSummary,
    detail: bool,
) -> serde_json::Value {
    let api_keys = entries
        .iter()
        .map(|entry| {
            let mut value = serde_json::json!({
                "id":entry.id,
                "name":entry.name,
                "enabled":entry.enabled,
                "skip_user_agent_check":entry.skip_user_agent_check,
                "credits_limit":entry.credits_limit,
            });
            if detail {
                value["total_requests"] = entry.usage.total_requests.into();
                value["total_input_tokens"] = entry.usage.total_input_tokens.into();
                value["total_output_tokens"] = entry.usage.total_output_tokens.into();
                value["total_credits"] = entry.usage.total_credits.into();
                value["reserved_credits"] = entry.reserved_credits.into();
            }
            value
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "summary":{
            "total":summary.total,
            "enabled":summary.enabled,
            "disabled":summary.disabled(),
            "total_requests":summary.total_requests,
            "total_input_tokens":summary.total_input_tokens,
            "total_output_tokens":summary.total_output_tokens,
            "total_credits":summary.total_credits,
            "reserved_credits":summary.reserved_credits,
        },
        "api_keys":api_keys,
    })
}

pub(super) fn format_credits(value: f64) -> String {
    format!("{value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provided_api_key_is_preserved_and_keeps_its_stable_id() {
        let original = "sk-restored-secret";
        let restored = resolve_api_key_value("sk", Some(original)).expect("restore key");

        assert_eq!(restored, original);
        assert_eq!(key_id(&restored), key_id(original));
    }

    #[test]
    fn provided_api_key_rejects_ambiguous_or_invalid_values() {
        assert!(resolve_api_key_value("sk", Some("")).is_err());
        assert!(resolve_api_key_value("sk", Some(" sk-secret")).is_err());
        assert!(resolve_api_key_value("sk", Some("sk-secret\n")).is_err());
        assert!(resolve_api_key_value("unknown", Some("secret")).is_err());
    }
}
