//! Shared CLI/REST account input parsing. Errors never include credential contents.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AccountCsvError {
    #[error("未闭合的引号")]
    UnclosedQuote,
    #[error("CSV 第 {line} 行无效：{source}")]
    Row { line: usize, source: Box<Self> },
    #[error("Kiro CSV 表头必须为 email,password")]
    Header,
    #[error("Kiro CSV 仅使用 email,password，不使用 Copilot 凭据列")]
    CopilotColumns,
    #[error("CSV 第 {line} 行必须只有 email,password 两列")]
    Columns { line: usize },
    #[error("CSV 中没有账号")]
    Empty,
}

pub fn parse_csv_line(line: &str) -> Result<Vec<String>, AccountCsvError> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                fields.last_mut().expect("initial field").push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(String::new()),
            other => fields.last_mut().expect("initial field").push(other),
        }
    }
    if quoted {
        return Err(AccountCsvError::UnclosedQuote);
    }
    Ok(fields)
}

/// Kiro's existing two-column CSV contract, including BOM, CRLF and quoted passwords.
pub fn parse_kiro_sso_csv(raw: &str) -> Result<Vec<(String, String)>, AccountCsvError> {
    let mut rows = Vec::new();
    let mut first_record = true;
    for (index, line) in raw.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = parse_csv_line(line.trim_start_matches('\u{feff}')).map_err(|source| {
            AccountCsvError::Row {
                line: index + 1,
                source: Box::new(source),
            }
        })?;
        if first_record {
            first_record = false;
            if fields
                .first()
                .is_some_and(|field| field.trim().eq_ignore_ascii_case("email"))
            {
                if fields.len() != 2 || !fields[1].trim().eq_ignore_ascii_case("password") {
                    return Err(AccountCsvError::Header);
                }
                continue;
            }
            if fields.first().is_some_and(|field| {
                matches!(
                    field.trim(),
                    "github_username" | "username" | "sso_username"
                )
            }) {
                return Err(AccountCsvError::CopilotColumns);
            }
        }
        if fields.len() != 2 || fields[0].trim().is_empty() || fields[1].is_empty() {
            return Err(AccountCsvError::Columns { line: index + 1 });
        }
        rows.push((fields[0].trim().to_ascii_lowercase(), fields[1].clone()));
    }
    if rows.is_empty() {
        return Err(AccountCsvError::Empty);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_normalizes_email_without_altering_quoted_passwords() {
        assert_eq!(
            parse_kiro_sso_csv(
                "\u{feff}email,password\r\n\"EXAMPLE@example.com\",\" p,\"\"w \"\r\n"
            )
            .unwrap(),
            vec![("example@example.com".into(), " p,\"w ".into())]
        );
    }

    #[test]
    fn malformed_csv_errors_never_echo_rows_or_passwords() {
        for raw in [
            "email,password\na@example.com,fixture-private-password,extra",
            "email,password\na@example.com,\"fixture-private-password",
        ] {
            let error = parse_kiro_sso_csv(raw).unwrap_err().to_string();
            assert!(!error.contains("fixture-private-password"));
            assert!(!error.contains("a@example.com"));
        }
    }
}
