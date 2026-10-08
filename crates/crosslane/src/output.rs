//! 终端输出格式化。

use anyhow::Result;
use serde::Serialize;

/// 以格式化 JSON 打印。
pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print one compact JSON value per line for streaming commands.
pub fn print_json_line<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// 渲染等宽对齐表格。
pub fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|header| display_width(header)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
    }

    let mut output = String::new();
    push_row(
        &mut output,
        &headers
            .iter()
            .map(|header| (*header).to_string())
            .collect::<Vec<_>>(),
        &widths,
    );
    for row in rows {
        push_row(&mut output, row, &widths);
    }
    output
}

fn push_row(output: &mut String, cells: &[String], widths: &[usize]) {
    let last = cells.len().saturating_sub(1);
    for (index, cell) in cells.iter().enumerate() {
        output.push_str(cell);
        if index != last {
            let width = widths
                .get(index)
                .copied()
                .unwrap_or_else(|| display_width(cell));
            output.push_str(&" ".repeat(width.saturating_sub(display_width(cell)) + 2));
        }
    }
    output.push('\n');
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| {
            let codepoint = character as u32;
            let wide = (0x1100..=0x115F).contains(&codepoint)
                || (0x2E80..=0xA4CF).contains(&codepoint)
                || (0xAC00..=0xD7A3).contains(&codepoint)
                || (0xF900..=0xFAFF).contains(&codepoint)
                || (0xFF00..=0xFF60).contains(&codepoint)
                || (0xFFE0..=0xFFE6).contains(&codepoint);
            usize::from(wide) + 1
        })
        .sum()
}

/// Unix 秒渲染为 UTC `YYYY-MM-DDTHH:MM:SSZ`。
pub fn format_timestamp(secs: i64) -> String {
    if secs <= 0 {
        return "-".into();
    }
    let days_since_epoch = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days_since_epoch);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let adjusted = days + 719_468;
    let era = adjusted.div_euclid(146_097);
    let day_of_era = adjusted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// 秒数渲染为粗粒度相对时长。
pub fn format_relative(secs: i64) -> String {
    if secs < 0 {
        return "-".into();
    }
    match secs {
        value if value < 60 => format!("{value}s"),
        value if value < 3_600 => format!("{}m", value / 60),
        value if value < 86_400 => format!("{}h", value / 3_600),
        value => format!("{}d", value / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns_and_handles_empty_rows() {
        let output = render_table(
            &["ID", "EMAIL"],
            &[
                vec!["acc_1".into(), "a@example.com".into()],
                vec!["acc_longer".into(), "b@x.io".into()],
            ],
        );
        let lines: Vec<_> = output.lines().collect();
        let email_column = lines[0].find("EMAIL").expect("email header");
        assert!(lines[1][email_column..].starts_with("a@example.com"));
        assert!(lines[2][email_column..].starts_with("b@x.io"));
        assert_eq!(render_table(&["ID"], &[]).lines().count(), 1);
    }

    #[test]
    fn time_formatters_match_expected_units() {
        assert_eq!(format_timestamp(0), "-");
        assert_eq!(format_timestamp(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(format_relative(30), "30s");
        assert_eq!(format_relative(90), "1m");
        assert_eq!(format_relative(3_700), "1h");
        assert_eq!(format_relative(90_000), "1d");
        assert_eq!(format_relative(-5), "-");
    }
}
