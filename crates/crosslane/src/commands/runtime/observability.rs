use super::{
    anyhow, format_credits, format_timestamp, method, print_json, render_table, AdminClient,
    Context, LogFilesResult, LogTraceResult, Path, PathBuf, Paths, Result,
};

pub async fn show_stats(
    client: &mut AdminClient,
    detail: bool,
    recent: Option<usize>,
    range: (Option<u64>, Option<i64>, Option<i64>),
    by: Option<&str>,
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    let (since_secs, start_secs, end_secs) = range;
    let effective_recent = detail.then_some(recent.unwrap_or(20));
    let value: serde_json::Value = client
        .call(
            method::STATS,
            serde_json::json!({
                "detail":detail,
                "recent":effective_recent,
                "since_secs":since_secs,
                "start_secs":start_secs,
                "end_secs":end_secs,
                "by":by,
                "provider":provider,
            }),
        )
        .await?;
    if json {
        return print_json(&value);
    }
    if value.get("by_apikey").is_some() {
        return print_stats_by_apikey(&value["by_apikey"]);
    }

    print_stats_range(&value);

    let summary = if detail {
        &value["stats"]["total"]
    } else {
        &value["summary"]
    };
    print_stats_summary(summary, &value["latency"]);
    if !detail {
        println!("使用 --detail 查看分组统计和最近请求。");
        return Ok(());
    }
    if let Some(dimension) = by {
        print_stats_group(dimension, &value["grouped"])?;
    }
    print_recent_stats_requests(&value["stats"]["recent_requests"])
}

fn print_stats_range(value: &serde_json::Value) {
    let range = &value["range"];
    let start = range["start"].as_i64();
    let end = range["end"].as_i64();
    if start.is_none() && end.is_none() {
        println!("范围    持久化累计（跨 daemon 重启）");
    } else {
        println!(
            "范围    持久化 {} ～ {}（分钟级聚合）",
            start.map_or_else(|| "最早可用".into(), format_timestamp),
            end.map_or_else(|| "现在".into(), format_timestamp)
        );
    }
    if range["truncated"].as_bool().unwrap_or(false) {
        if range["prefix_truncated"].as_bool().unwrap_or(false) {
            let available = range["available_start"]
                .as_i64()
                .map_or_else(|| "未知".into(), format_timestamp);
            println!("提示    指定范围早于可用时间序列，结果从 {available} 起统计");
        }
        if let Some(gaps) = range["missing_ranges"].as_array() {
            for gap in gaps.iter().take(3) {
                let start = gap["start"]
                    .as_i64()
                    .map_or_else(|| "未知".into(), format_timestamp);
                let end = gap["end"]
                    .as_i64()
                    .map_or_else(|| "未知".into(), format_timestamp);
                println!("提示    {start} ～ {end} 的历史分片不可用，结果不包含该时段");
            }
            if gaps.len() > 3 {
                println!("提示    另有 {} 个历史缺口未逐项展示", gaps.len() - 3);
            }
        }
    }
}

fn print_stats_summary(summary: &serde_json::Value, latency: &serde_json::Value) {
    let requests = summary["requests"].as_u64().unwrap_or(0);
    let successes = summary["successes"].as_u64().unwrap_or(0);
    let success_rate = if requests == 0 {
        0.0
    } else {
        successes as f64 / requests as f64 * 100.0
    };
    let rows = vec![
        vec!["请求总数".into(), requests.to_string()],
        vec!["成功".into(), successes.to_string()],
        vec![
            "失败".into(),
            summary["failures"].as_u64().unwrap_or(0).to_string(),
        ],
        vec!["成功率".into(), format!("{success_rate:.1}%")],
        vec![
            "输入 Tokens".into(),
            summary["input_tokens"].as_u64().unwrap_or(0).to_string(),
        ],
        vec![
            "输出 Tokens".into(),
            summary["output_tokens"].as_u64().unwrap_or(0).to_string(),
        ],
        vec![
            "Credits".into(),
            format_credits(summary["credits"].as_f64().unwrap_or(0.0)),
        ],
        vec![
            "平均延迟".into(),
            format!("{} ms", latency["average_ms"].as_u64().unwrap_or(0)),
        ],
        vec![
            "延迟 P50/P95/P99".into(),
            format!(
                "{}/{}/{} ms",
                latency["p50_ms"].as_u64().unwrap_or(0),
                latency["p95_ms"].as_u64().unwrap_or(0),
                latency["p99_ms"].as_u64().unwrap_or(0)
            ),
        ],
    ];
    println!("{}", render_table(&["指标", "值"], &rows));
}

fn print_stats_group(dimension: &str, grouped: &serde_json::Value) -> Result<()> {
    let Some(object) = grouped.as_object() else {
        return Err(anyhow!("daemon 返回的 stats 分组数据无效"));
    };
    let mut entries = object.iter().collect::<Vec<_>>();
    entries.sort_by(|(left_name, left), (right_name, right)| {
        right["requests"]
            .as_u64()
            .cmp(&left["requests"].as_u64())
            .then_with(|| left_name.cmp(right_name))
    });
    let rows = entries
        .into_iter()
        .map(|(name, counter)| {
            vec![
                name.clone(),
                counter["requests"].as_u64().unwrap_or(0).to_string(),
                counter["successes"].as_u64().unwrap_or(0).to_string(),
                counter["failures"].as_u64().unwrap_or(0).to_string(),
                counter["input_tokens"].as_u64().unwrap_or(0).to_string(),
                counter["output_tokens"].as_u64().unwrap_or(0).to_string(),
                format_credits(counter["credits"].as_f64().unwrap_or(0.0)),
            ]
        })
        .collect::<Vec<_>>();
    println!("\n按 {dimension} 分组：");
    println!(
        "{}",
        render_table(
            &[
                dimension,
                "请求",
                "成功",
                "失败",
                "输入 Tokens",
                "输出 Tokens",
                "Credits",
            ],
            &rows,
        )
    );
    Ok(())
}

fn print_recent_stats_requests(requests: &serde_json::Value) -> Result<()> {
    let Some(requests) = requests.as_array() else {
        return Err(anyhow!("daemon 返回的最近请求数据无效"));
    };
    if requests.is_empty() {
        return Ok(());
    }
    let rows = requests
        .iter()
        .map(|request| {
            let account = request["account_name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .or_else(|| request["account_id"].as_str())
                .unwrap_or("-");
            vec![
                format_timestamp(request["timestamp"].as_i64().unwrap_or(0)),
                request["status"].as_u64().unwrap_or(0).to_string(),
                request["model"].as_str().unwrap_or("-").into(),
                account.into(),
                request["duration_ms"].as_u64().unwrap_or(0).to_string(),
                request["input_tokens"].as_u64().unwrap_or(0).to_string(),
                request["output_tokens"].as_u64().unwrap_or(0).to_string(),
                format_credits(request["credits"].as_f64().unwrap_or(0.0)),
                request["error"].as_str().unwrap_or("").into(),
            ]
        })
        .collect::<Vec<_>>();
    println!("\n最近请求：");
    println!(
        "{}",
        render_table(
            &[
                "时间",
                "状态",
                "模型",
                "账号",
                "耗时(ms)",
                "输入",
                "输出",
                "Credits",
                "错误",
            ],
            &rows,
        )
    );
    Ok(())
}

fn print_stats_by_apikey(value: &serde_json::Value) -> Result<()> {
    let Some(entries) = value.as_array() else {
        return Err(anyhow!("daemon 返回的 API key stats 无效"));
    };
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                entry["id"].as_str().unwrap_or("-").into(),
                entry["name"].as_str().unwrap_or("-").into(),
                entry["usage"]["total_requests"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
                entry["usage"]["total_input_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
                entry["usage"]["total_output_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
                format_credits(entry["usage"]["total_credits"].as_f64().unwrap_or(0.0)),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        render_table(
            &[
                "ID",
                "名称",
                "请求",
                "输入 Tokens",
                "输出 Tokens",
                "Credits"
            ],
            &rows,
        )
    );
    Ok(())
}

pub(super) fn print_human_value(value: &serde_json::Value) {
    match value {
        serde_json::Value::Array(values)
            if values.iter().all(serde_json::Value::is_object) && !values.is_empty() =>
        {
            let mut headers = Vec::<String>::new();
            for object in values.iter().filter_map(serde_json::Value::as_object) {
                for key in object.keys() {
                    if !headers.contains(key) && headers.len() < 10 {
                        headers.push(key.clone());
                    }
                }
            }
            let rows = values
                .iter()
                .filter_map(serde_json::Value::as_object)
                .map(|object| {
                    headers
                        .iter()
                        .map(|key| {
                            compact_value(object.get(key).unwrap_or(&serde_json::Value::Null))
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let labels = headers.iter().map(String::as_str).collect::<Vec<_>>();
            println!("{}", render_table(&labels, &rows));
        }
        serde_json::Value::Object(object) => {
            let rows = object
                .iter()
                .map(|(key, value)| vec![key.clone(), compact_value(value)])
                .collect::<Vec<_>>();
            println!("{}", render_table(&["字段", "值"], &rows));
        }
        _ => println!("{}", compact_value(value)),
    }
}

fn compact_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "-".into(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Array(values)
            if values
                .iter()
                .all(|value| value.is_string() || value.is_number()) =>
        {
            values
                .iter()
                .map(compact_value)
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => serde_json::to_string(value).unwrap_or_else(|_| "<invalid>".into()),
    }
}

pub fn parse_duration(value: &str) -> Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let amount = value[..split]
        .parse::<u64>()
        .with_context(|| format!("无效时间窗口: {value}"))?;
    let multiplier = match &value[split..] {
        "s" | "" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        unit => return Err(anyhow!("不支持的时间单位: {unit}")),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("时间窗口过大: {value}"))
}

/// Parse Unix seconds or an RFC 3339 timestamp with an explicit timezone.
pub fn parse_timestamp(value: &str) -> Result<i64> {
    let value = value.trim();
    if let Ok(timestamp) = value.parse::<i64>() {
        return Ok(timestamp);
    }
    let (date, time_and_zone) = value
        .split_once('T')
        .or_else(|| value.split_once(' '))
        .ok_or_else(|| {
            anyhow!(
                "无效时间 {value}；请使用 Unix 秒或带时区的 RFC 3339，例如 2026-08-27T10:00:00+08:00"
            )
        })?;
    let (time, offset_seconds) = if let Some(time) = time_and_zone
        .strip_suffix('Z')
        .or_else(|| time_and_zone.strip_suffix('z'))
    {
        (time, 0i64)
    } else {
        let offset_index = time_and_zone
            .char_indices()
            .rfind(|(_, character)| matches!(character, '+' | '-'))
            .map(|(index, _)| index)
            .ok_or_else(|| anyhow!("时间必须包含时区：{value}；例如使用 Z 或 +08:00"))?;
        let (time, offset) = time_and_zone.split_at(offset_index);
        (time, parse_timezone_offset(offset, value)?)
    };
    let (year, month, day) = parse_date(date, value)?;
    let (hour, minute, second) = parse_clock(time, value)?;
    let days = days_from_civil(year, month, day);
    days.checked_mul(86_400)
        .and_then(|timestamp| timestamp.checked_add(i64::from(hour) * 3_600))
        .and_then(|timestamp| timestamp.checked_add(i64::from(minute) * 60))
        .and_then(|timestamp| timestamp.checked_add(i64::from(second)))
        .and_then(|timestamp| timestamp.checked_sub(offset_seconds))
        .ok_or_else(|| anyhow!("时间超出支持范围: {value}"))
}

fn parse_date(value: &str, original: &str) -> Result<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next().and_then(|value| value.parse::<i64>().ok());
    let month = parts.next().and_then(|value| value.parse::<u32>().ok());
    let day = parts.next().and_then(|value| value.parse::<u32>().ok());
    if parts.next().is_some() {
        return Err(anyhow!("无效日期: {original}"));
    }
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(anyhow!("无效日期: {original}"));
    };
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return Err(anyhow!("无效月份: {original}")),
    };
    if day == 0 || day > maximum_day {
        return Err(anyhow!("无效日期: {original}"));
    }
    Ok((year, month, day))
}

fn parse_clock(value: &str, original: &str) -> Result<(u32, u32, u32)> {
    let value = value.split('.').next().unwrap_or(value);
    let mut parts = value.split(':');
    let hour = parts.next().and_then(|value| value.parse::<u32>().ok());
    let minute = parts.next().and_then(|value| value.parse::<u32>().ok());
    let second = parts.next().and_then(|value| value.parse::<u32>().ok());
    if parts.next().is_some() {
        return Err(anyhow!("无效时间: {original}"));
    }
    let (Some(hour), Some(minute), Some(second)) = (hour, minute, second) else {
        return Err(anyhow!("无效时间: {original}"));
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(anyhow!("无效时间: {original}"));
    }
    Ok((hour, minute, second))
}

fn parse_timezone_offset(value: &str, original: &str) -> Result<i64> {
    let sign = match value.as_bytes().first().copied() {
        Some(b'+') => 1i64,
        Some(b'-') => -1i64,
        _ => return Err(anyhow!("无效时间时区: {original}")),
    };
    let Some((hours, minutes)) = value[1..].split_once(':') else {
        return Err(anyhow!("无效时间时区: {original}"));
    };
    let hours = hours
        .parse::<u32>()
        .map_err(|_| anyhow!("无效时间时区: {original}"))?;
    let minutes = minutes
        .parse::<u32>()
        .map_err(|_| anyhow!("无效时间时区: {original}"))?;
    if hours > 23 || minutes > 59 {
        return Err(anyhow!("无效时间时区: {original}"));
    }
    Ok(sign * (i64::from(hours) * 3_600 + i64::from(minutes) * 60))
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub async fn show_logs(
    client: &mut AdminClient,
    tail: usize,
    follow: bool,
    level: Option<&str>,
    account: Option<&str>,
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut after_request_id: Option<String> = None;
    loop {
        let value: serde_json::Value = client
            .call(
                method::LOGS,
                serde_json::json!({
                    "after_request_id":after_request_id,
                    "tail":tail,
                    "wait_ms":if follow {30_000} else {0},
                    "level":level,
                    "account":account,
                    "provider":provider
                }),
            )
            .await?;
        for request in value["entries"].as_array().into_iter().flatten() {
            let request_id = request["request_id"].as_str().unwrap_or_default();
            if !request_id.is_empty() {
                after_request_id = Some(request_id.to_string());
            }
            if json {
                println!("{}", serde_json::to_string(request)?);
            } else {
                let account = log_account(request);
                let models = log_model_route(request);
                println!(
                    "{} {:>3} {:>6}ms provider={} account={} model={}",
                    format_timestamp(request["timestamp"].as_i64().unwrap_or_default()),
                    request["status"].as_u64().unwrap_or_default(),
                    request["duration_ms"].as_u64().unwrap_or_default(),
                    request["provider_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("kiro"),
                    account,
                    models.original,
                );
                if let Some(rule) = models.mapping_rule {
                    println!(
                        "  mapping rule={} path={} -> {}",
                        rule, models.original, models.routed
                    );
                } else if models.original != models.routed {
                    println!(
                        "  fallback_routing path={} -> {}",
                        models.original, models.routed
                    );
                }
                if models.routed != models.resolved {
                    println!(
                        "  auto_resolution path={} -> {}",
                        models.routed, models.resolved
                    );
                }
                println!(
                    "  request path={} endpoint={} trace={} request_id={}",
                    request["path"].as_str().unwrap_or("-"),
                    request["endpoint"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("-"),
                    request["trace_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("-"),
                    request["request_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("-"),
                );
                if let Some(error) = request["error"].as_str().filter(|value| !value.is_empty()) {
                    println!("  error: {error}");
                }
                let diagnostics = &request["diagnostics"];
                let error_code = diagnostics["error_code"]
                    .as_str()
                    .filter(|value| !value.is_empty());
                let error_stage = diagnostics["error_stage"]
                    .as_str()
                    .filter(|value| !value.is_empty());
                if error_code.is_some() || error_stage.is_some() {
                    let upstream_status = diagnostics["upstream_status"]
                        .as_u64()
                        .map_or_else(|| "-".into(), |status| status.to_string());
                    println!(
                        "  diagnostics code={} stage={} client_status={} upstream_status={} account_error={}",
                        error_code.unwrap_or("-"),
                        error_stage.unwrap_or("-"),
                        diagnostics["client_status"]
                            .as_u64()
                            .unwrap_or_else(|| request["status"].as_u64().unwrap_or_default()),
                        upstream_status,
                        diagnostics["account_error"].as_bool().unwrap_or(false),
                    );
                    if error_code == Some("model_not_available") {
                        println!("  hint: crosslane models resolve {}", models.original);
                    }
                }
                if let Some(context) = diagnostics["context_overflow"].as_object() {
                    let tokens = |name: &str| {
                        context
                            .get(name)
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or_default()
                    };
                    println!(
                        "  context_tokens total={} protected_prefix={} tools={} current={} history={} overhead={} minimum={} maximum={}",
                        tokens("total_input_tokens"), tokens("protected_prefix_tokens"),
                        tokens("tool_definition_tokens"), tokens("current_message_tokens"),
                        tokens("history_tokens"), tokens("overhead_tokens"),
                        tokens("minimum_input_tokens"), tokens("maximum_input_tokens"),
                    );
                    if tokens("minimum_input_tokens") > tokens("maximum_input_tokens") {
                        println!("  hint: protected_prefix includes system instructions and long tool documentation; reduce that prefix, loaded tool definitions, or the current message to fit the model window");
                    }
                }
                for attempt in request["attempts"].as_array().into_iter().flatten() {
                    let status = attempt["status"]
                        .as_u64()
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "-".into());
                    let available_models = attempt["available_models"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",");
                    let available_models = if available_models.is_empty() {
                        String::new()
                    } else {
                        format!(" available_models=[{available_models}]")
                    };
                    println!(
                        "  attempt={} account={} model={} endpoint={} upstream_status={} error={}{}",
                        attempt["attempt"].as_u64().unwrap_or_default(),
                        log_account(attempt),
                        attempt["model"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .unwrap_or("-"),
                        attempt["endpoint"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .unwrap_or("-"),
                        status,
                        attempt["error"].as_str().unwrap_or(""),
                        available_models,
                    );
                }
            }
        }
        if !follow {
            return Ok(());
        }
    }
}

pub async fn show_log_files(
    client: &mut AdminClient,
    level: Option<&str>,
    paths_only: bool,
    json: bool,
) -> Result<()> {
    let mut result: LogFilesResult = client
        .call(method::LOG_FILES, serde_json::json!({}))
        .await?;
    let host_data_dir = std::env::var_os(WRAPPER_HOST_DATA_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    populate_host_log_paths(
        &mut result,
        host_data_dir.as_deref(),
        &Paths::from_env().data_dir,
    );
    if let Some(level) = level {
        result.files.retain(|file| file.level == level);
    }
    if json {
        if paths_only {
            let mut output = serde_json::to_value(&result)?;
            if let Some(output) = output.as_object_mut() {
                output.remove("files");
            }
            return print_json(&output);
        }
        return print_json(&result);
    }
    println!("日志目录    {}", result.directory);
    if let Some(host_directory) = &result.host_directory {
        println!("宿主机目录  {host_directory}");
    }
    println!("基础路径    {}", result.base_path);
    if let Some(host_base_path) = &result.host_base_path {
        println!("宿主机基础路径 {host_base_path}");
    }
    println!("格式/过滤   {} / {}", result.format, result.level_filter);
    if paths_only {
        return Ok(());
    }
    if result.files.is_empty() {
        println!("暂无匹配的日志文件；对应级别产生日志后会自动创建。");
        return Ok(());
    }
    let has_host_paths = result.files.iter().any(|file| file.host_path.is_some());
    let rows = result
        .files
        .into_iter()
        .map(|file| {
            let display_path = file.host_path.unwrap_or(file.path);
            vec![
                file.date,
                file.level,
                format_bytes(file.size_bytes),
                file.modified_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "-".into()),
                display_path,
            ]
        })
        .collect::<Vec<_>>();
    let path_heading = if has_host_paths {
        "宿主机文件路径"
    } else {
        "文件路径"
    };
    println!(
        "{}",
        render_table(&["日期", "级别", "大小", "修改时间", path_heading], &rows)
    );
    Ok(())
}

pub async fn show_trace_logs(
    client: &mut AdminClient,
    trace_id: &str,
    tail: usize,
    level: Option<&str>,
    json: bool,
) -> Result<()> {
    let result: LogTraceResult = client
        .call(
            method::LOG_TRACE,
            serde_json::json!({
                "trace_id":trace_id,
                "tail":tail,
                "level":level,
            }),
        )
        .await?;
    if json {
        return print_json(&result);
    }

    println!(
        "trace={}  命中={}  显示={}  扫描={} 个文件 / {}",
        result.trace_id,
        result.matched_records,
        result.entries.len(),
        result.files_scanned,
        format_bytes(result.bytes_scanned),
    );
    if result.entries.is_empty() {
        println!("未找到对应链路；可用 `crosslane logs files` 确认日志保留范围和级别分片。");
        return Ok(());
    }
    if result.truncated {
        println!("提示：结果受 --tail 或安全扫描上限限制，只显示最近的匹配记录。");
    }
    let host_data_dir = std::env::var_os(WRAPPER_HOST_DATA_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let container_data_dir = Paths::from_env().data_dir;
    for entry in result.entries {
        let timestamp = entry.record["timestamp"].as_str().unwrap_or(&entry.date);
        let level = entry
            .record
            .get("level")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&entry.level);
        let target = entry
            .record
            .get("target")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        let message = trace_log_message(&entry.record);
        println!(
            "{timestamp} {:>5} {target} {message}",
            level.to_ascii_uppercase()
        );
        let display_path = host_data_dir
            .as_deref()
            .and_then(|host| host_log_path(&entry.path, host, &container_data_dir))
            .unwrap_or(entry.path);
        println!("  {display_path}:{}", entry.line);
        if let Some(context) = trace_log_context(&entry.record) {
            println!("  {context}");
        }
    }
    Ok(())
}

fn trace_log_message(record: &serde_json::Value) -> &str {
    record
        .get("fields")
        .and_then(|fields| fields.get("message"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| record.as_str())
        .unwrap_or("-")
}

fn trace_log_context(record: &serde_json::Value) -> Option<String> {
    let mut context = serde_json::Map::new();
    if let Some(fields) = record.get("fields").and_then(serde_json::Value::as_object) {
        let mut fields = fields.clone();
        fields.remove("message");
        if !fields.is_empty() {
            context.insert("fields".into(), serde_json::Value::Object(fields));
        }
    }
    for key in [
        "span",
        "spans",
        "filename",
        "line_number",
        "threadName",
        "threadId",
    ] {
        if let Some(value) = record.get(key) {
            context.insert(key.into(), value.clone());
        }
    }
    (!context.is_empty())
        .then(|| serde_json::to_string(&context))
        .transpose()
        .ok()
        .flatten()
}

const WRAPPER_HOST_DATA_DIR_ENV: &str = "CROSSLANE_WRAPPER_HOST_DATA_DIR";

pub(super) fn populate_host_log_paths(
    result: &mut LogFilesResult,
    host_data_dir: Option<&Path>,
    container_data_dir: &Path,
) {
    let Some(host_data_dir) = host_data_dir else {
        return;
    };
    result.host_base_path = host_log_path(&result.base_path, host_data_dir, container_data_dir);
    result.host_directory = host_log_path(&result.directory, host_data_dir, container_data_dir);
    for file in &mut result.files {
        file.host_path = host_log_path(&file.path, host_data_dir, container_data_dir);
    }
}

pub(super) fn host_log_path(
    path: &str,
    host_data_dir: &Path,
    container_data_dir: &Path,
) -> Option<String> {
    let relative = Path::new(path).strip_prefix(container_data_dir).ok()?;
    Some(host_data_dir.join(relative).display().to_string())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

pub(super) fn log_account(value: &serde_json::Value) -> String {
    let id = value["account_id"]
        .as_str()
        .filter(|value| !value.is_empty());
    let name = value["account_name"]
        .as_str()
        .filter(|value| !value.is_empty());
    match (name, id) {
        (Some(name), Some(id)) if name != id => format!("{name} ({id})"),
        (Some(name), _) => name.to_owned(),
        (_, Some(id)) => id.to_owned(),
        _ => "-".into(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct LogModelRoute<'a> {
    pub(super) original: &'a str,
    pub(super) routed: &'a str,
    pub(super) resolved: &'a str,
    pub(super) mapping_rule: Option<&'a str>,
}

pub(super) fn log_model_route(request: &serde_json::Value) -> LogModelRoute<'_> {
    let original = request["original_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .or_else(|| request["model"].as_str())
        .unwrap_or("-");
    let routed = request["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .unwrap_or(original);
    let resolved = request["kiro_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .unwrap_or(routed);
    LogModelRoute {
        original,
        routed,
        resolved,
        mapping_rule: request["model_mapping_rule"].as_str(),
    }
}
