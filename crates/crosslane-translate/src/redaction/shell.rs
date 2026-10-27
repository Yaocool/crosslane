//! POSIX shell word escaping for opaque references in known command tools.
//!
//! This preserves common unquoted, single-quoted and double-quoted shell words.
//! It is not a parser for heredocs, nested shell substitutions, or languages
//! embedded inside a shell argument (for example Python passed to `python -c`).

use serde_json::Value;
use std::ops::Range;

use super::{
    marker_pattern, recover_exact_source, restore_string, restore_value, secret_ranges_shell,
    RedactionSession, SessionState,
};

const COMMAND_FIELDS: [&str; 4] = ["command", "cmd", "script", "input"];

/// Decode a literal span inside one recognized authentication option's word.
/// It can originate in prose rather than a historical shell tool invocation.
/// This never evaluates expansions or substitutions.
pub(super) fn literal_word_fragment(word: &str, range: Range<usize>) -> Option<String> {
    let mut position = ShellPosition::default();
    position.literal(word.get(..range.start)?, &mut String::new());
    (!position.comment && !position.escaped)
        .then(|| decode_literal_quoting(word.get(range)?, position.quote))
        .flatten()
}

pub(super) fn command_sources<'a>(tool_name: &str, input: &'a Value) -> Vec<&'a str> {
    if !is_shell_tool(tool_name) || explicitly_non_posix(input) {
        return Vec::new();
    }
    COMMAND_FIELDS
        .iter()
        .filter_map(|field| input.get(field).and_then(Value::as_str))
        .collect()
}

/// Decode only literal pieces taken from a known shell tool's command. This
/// does not evaluate expansions, substitutions or any embedded language.
pub(super) fn discover_shell_secrets(tool_name: &str, input: &Value) -> Vec<String> {
    let mut values = Vec::new();
    for command in command_sources(tool_name, input) {
        let mut ranges = secret_ranges_shell(command, &[], None);
        // The main collector decodes embedded JSON through data(), including
        // public auth controls. Raw shell precollection must not seed values
        // such as `credentials: "include"` as secrets before that semantic pass.
        let documents: Vec<_> = super::embedded_json::documents(command, 0)
            .map(|document| document.range)
            .collect();
        ranges.retain(|range| {
            let index = documents.partition_point(|document| document.end <= range.start);
            !documents
                .get(index)
                .is_some_and(|document| document.start < range.start && range.end < document.end)
        });
        ranges.sort_unstable_by_key(|range| (range.start, range.end));
        let mut merged: Vec<Range<usize>> = Vec::new();
        for range in ranges {
            if let Some(previous) = merged.last_mut() {
                if range.start < previous.end {
                    previous.end = previous.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        let mut position = ShellPosition::default();
        let mut offset = 0;
        let mut discarded = String::new();
        for range in merged {
            position.literal(&command[offset..range.start], &mut discarded);
            if !position.comment {
                if let Some(value) = decode_literal_quoting(&command[range.clone()], position.quote)
                {
                    values.push(value);
                }
            }
            position.literal(&command[range.clone()], &mut discarded);
            offset = range.end;
            discarded.clear();
        }
    }
    values
}

/// Give shell source occurrences semantic markers of their own. A raw source
/// spelling such as `a\\\\b` must keep its raw marker in ordinary JSON, while
/// a copied reference from a double-quoted command must represent `a\\b`.
pub(super) fn normalize_shell_markers(
    tool_name: &str,
    original: &Value,
    safe: &mut Value,
    session: &RedactionSession,
) {
    if !is_shell_tool(tool_name) || explicitly_non_posix(original) {
        return;
    }
    for field in COMMAND_FIELDS {
        let Some(original) = original.get(field).and_then(Value::as_str) else {
            continue;
        };
        let Some(Value::String(command)) = safe.get_mut(field) else {
            continue;
        };
        let replacements = {
            let state = session
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            shell_marker_values(original, command, &state)
        };
        let Some(replacements) = replacements.filter(|values| !values.is_empty()) else {
            continue;
        };
        let mut normalized = String::with_capacity(command.len());
        let mut offset = 0;
        for (range, value) in replacements {
            normalized.push_str(&command[offset..range.start]);
            normalized.push_str(&session.marker(Value::String(value)));
            offset = range.end;
        }
        normalized.push_str(&command[offset..]);
        *command = session.remember_source_text(original, normalized);
    }
}

fn shell_marker_values(
    original: &str,
    safe: &str,
    state: &SessionState,
) -> Option<Vec<(Range<usize>, String)>> {
    let mut output = Vec::new();
    let mut position = ShellPosition::default();
    let mut original_offset = 0;
    let mut safe_offset = 0;
    let mut discarded = String::new();
    for marker in marker_pattern().find_iter(safe) {
        let literal = &safe[safe_offset..marker.start()];
        if !original[original_offset..].starts_with(literal) {
            return None;
        }
        position.literal(literal, &mut discarded);
        original_offset += literal.len();
        let raw = state.by_marker.get(marker.as_str())?.as_str()?;
        if !original[original_offset..].starts_with(raw) {
            return None;
        }
        if !position.comment {
            if let Some(value) = decode_literal_quoting(raw, position.quote) {
                if value != raw {
                    output.push((marker.range(), value));
                }
            }
        }
        position.literal(raw, &mut discarded);
        original_offset += raw.len();
        safe_offset = marker.end();
        discarded.clear();
    }
    (original[original_offset..] == safe[safe_offset..]).then_some(output)
}

fn decode_literal_fragment(raw: &str, quote: Quote) -> Option<String> {
    let mut output = String::with_capacity(raw.len());
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Quote::Single, '\'')
            | (Quote::Double, '"')
            | (Quote::AnsiSingle, '\'')
            | (
                Quote::Unquoted,
                '\'' | '"' | ';' | '&' | '|' | '<' | '>' | '*' | '?' | '[' | '{' | '}' | '(' | ')'
                | '~' | '#' | ' ' | '\t' | '\n',
            )
            | (Quote::Unquoted | Quote::Double, '$' | '`') => return None,
            (Quote::Unquoted, '\\') => {
                let next = characters.next()?;
                if next != '\n' {
                    output.push(next);
                }
            }
            (Quote::Double, '\\') => {
                let next = characters.next()?;
                if !matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    output.push('\\');
                }
                if next != '\n' {
                    output.push(next);
                }
            }
            (Quote::AnsiSingle, '\\') => {
                let next = characters.next()?;
                match next {
                    'a' => output.push('\u{7}'),
                    'b' => output.push('\u{8}'),
                    'e' | 'E' => output.push('\u{1b}'),
                    'f' => output.push('\u{c}'),
                    'n' => output.push('\n'),
                    'r' => output.push('\r'),
                    't' => output.push('\t'),
                    'v' => output.push('\u{b}'),
                    '\\' | '\'' | '"' | '?' => output.push(next),
                    // Keep uncommon numeric/control escapes outside this
                    // literal subset instead of guessing their semantics.
                    '0'..='7' | 'x' | 'u' | 'U' | 'c' => return None,
                    _ => {
                        output.push('\\');
                        output.push(next);
                    }
                }
            }
            _ => output.push(character),
        }
    }
    Some(output)
}

/// Concatenate adjacent literal quote sections of one word. Keep expansion,
/// command substitution, globbing and numeric ANSI escapes outside the subset.
fn decode_literal_quoting(raw: &str, mut quote: Quote) -> Option<String> {
    let mut output = String::with_capacity(raw.len());
    let mut offset = 0;
    let mut part = 0;
    while offset < raw.len() {
        let ch = raw[offset..].chars().next()?;
        if ch == '\\' && quote != Quote::Single {
            offset += 1;
            offset += raw[offset..].chars().next()?.len_utf8();
            continue;
        }
        let closing = matches!(
            (quote, ch),
            (Quote::Single | Quote::AnsiSingle, '\'') | (Quote::Double, '"')
        );
        let ansi = quote == Quote::Unquoted && raw[offset..].starts_with("$'");
        let opening = quote == Quote::Unquoted && matches!(ch, '\'' | '"');
        if closing || opening || ansi {
            output.push_str(&decode_literal_fragment(&raw[part..offset], quote)?);
            quote = if closing {
                Quote::Unquoted
            } else if ansi {
                Quote::AnsiSingle
            } else if ch == '\'' {
                Quote::Single
            } else {
                Quote::Double
            };
            offset += if ansi { 2 } else { 1 };
            part = offset;
        } else {
            offset += ch.len_utf8();
        }
    }
    output.push_str(&decode_literal_fragment(&raw[part..], quote)?);
    Some(output)
}

impl RedactionSession {
    /// Restore native JSON values normally, with shell word escaping only for
    /// command strings belonging to an explicitly recognized POSIX shell tool.
    pub fn restore_tool_input_for_tool(&self, tool_name: &str, input: &mut Value) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !is_shell_tool(tool_name) || explicitly_non_posix(input) {
            restore_value(input, &state);
            return;
        }
        let Value::Object(fields) = input else {
            restore_value(input, &state);
            return;
        };
        let original = std::mem::take(fields);
        for (name, mut value) in original {
            let name = restore_string(&name, &state);
            if matches!(name.as_str(), "command" | "cmd" | "script" | "input") {
                if let Value::String(command) = &mut value {
                    *command = restore_shell_words(command, &state);
                } else {
                    restore_value(&mut value, &state);
                }
            } else {
                restore_value(&mut value, &state);
            }
            fields.insert(name, value);
        }
    }
}

fn is_shell_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "bash",
        "shell",
        "exec_command",
        "run_shell",
        "run_shell_command",
        "run_command",
        "execute_shell",
        "execute_shell_command",
        "terminal",
    ]
    .iter()
    .any(|known| {
        lower == *known
            || lower
                .strip_suffix(known)
                .is_some_and(|prefix| prefix.ends_with(['_', '.', ':', '/']))
    })
}

pub(super) fn explicitly_non_posix(input: &Value) -> bool {
    input
        .get("shell")
        .and_then(Value::as_str)
        .is_some_and(|shell| {
            let shell = shell.to_ascii_lowercase();
            shell.contains("powershell")
                || shell.contains("pwsh")
                || shell
                    .rsplit(['/', '\\'])
                    .next()
                    .is_some_and(|name| matches!(name, "cmd" | "cmd.exe"))
        })
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Quote {
    #[default]
    Unquoted,
    Single,
    Double,
    AnsiSingle,
}

struct ShellPosition {
    quote: Quote,
    escaped: bool,
    comment: bool,
    word_start: bool,
    dollar_prefix: bool,
}

impl Default for ShellPosition {
    fn default() -> Self {
        Self {
            quote: Quote::Unquoted,
            escaped: false,
            comment: false,
            word_start: true,
            dollar_prefix: false,
        }
    }
}

impl ShellPosition {
    fn literal(&mut self, text: &str, output: &mut String) {
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            output.push(character);
            if !self.comment
                && !self.escaped
                && matches!(self.quote, Quote::Unquoted | Quote::Double)
                && character == '\\'
                && characters.peek() == Some(&'\n')
            {
                // POSIX removes this pair before recognizing words and
                // comments. Preserve the preceding quote/word boundary.
                output.push(characters.next().expect("peeked line continuation"));
                continue;
            }
            let dollar_prefix = std::mem::take(&mut self.dollar_prefix);
            if self.comment {
                if character == '\n' {
                    self.comment = false;
                    self.word_start = true;
                }
                continue;
            }
            if self.escaped {
                self.escaped = false;
                self.word_start = false;
                continue;
            }
            match (self.quote, character) {
                (Quote::Unquoted | Quote::Double | Quote::AnsiSingle, '\\') => self.escaped = true,
                (Quote::Unquoted, '\'') if dollar_prefix => self.quote = Quote::AnsiSingle,
                (Quote::Unquoted, '\'') => self.quote = Quote::Single,
                (Quote::Unquoted, '"') => self.quote = Quote::Double,
                (Quote::Single | Quote::AnsiSingle, '\'') | (Quote::Double, '"') => {
                    self.quote = Quote::Unquoted
                }
                (Quote::Unquoted, '#') if self.word_start => self.comment = true,
                _ => {}
            }
            self.dollar_prefix = matches!(self.quote, Quote::Unquoted) && character == '$';
            self.word_start = matches!(self.quote, Quote::Unquoted)
                && (matches!(character, ' ' | '\t' | '\n')
                    || matches!(character, ';' | '|' | '&' | '(' | ')' | '<' | '>'));
        }
    }

    fn value(&mut self, text: &str, output: &mut String) {
        if self.comment {
            // Shell comments do not honor quotes. Keep a multiline secret from
            // creating an executable line while leaving comment semantics inert.
            output.push_str(&text.replace('\r', "\\r").replace('\n', "\\n"));
            return;
        }
        if self.escaped {
            // A backslash before the marker escaped its first underscore in an
            // unquoted word. In double quotes it was a literal backslash; spell
            // it as `\\` so the inserted value cannot change that meaning.
            match self.quote {
                Quote::Unquoted => {
                    output.pop();
                }
                Quote::Double | Quote::AnsiSingle => output.push('\\'),
                Quote::Single => {}
            }
            self.escaped = false;
        }
        match self.quote {
            Quote::Unquoted => {
                output.push('\'');
                output.push_str(&text.replace('\'', "'\\''"));
                output.push('\'');
            }
            Quote::Single => output.push_str(&text.replace('\'', "'\\''")),
            Quote::AnsiSingle => {
                for character in text.chars() {
                    if matches!(character, '\\' | '\'') {
                        output.push('\\');
                    }
                    output.push(character);
                }
            }
            Quote::Double => {
                for character in text.chars() {
                    if matches!(character, '\\' | '$' | '`' | '"') {
                        output.push('\\');
                    }
                    output.push(character);
                }
            }
        }
        self.word_start = false;
        self.dollar_prefix = false;
    }
}

fn restore_shell_words(command: &str, state: &SessionState) -> String {
    // A replayed source command already has its own shell escapes. Rebuilding
    // it from literal fragments would escape those a second time and change
    // the credential bytes passed to the program.
    if let Some(original) = recover_exact_source(command, state) {
        return original;
    }
    let mut output = String::with_capacity(command.len());
    let mut position = ShellPosition::default();
    let mut offset = 0;
    for matched in marker_pattern().find_iter(command) {
        position.literal(&command[offset..matched.start()], &mut output);
        match state.by_marker.get(matched.as_str()) {
            Some(Value::String(value)) => position.value(value, &mut output),
            Some(value) => position.value(&value.to_string(), &mut output),
            None => position.literal(matched.as_str(), &mut output),
        }
        offset = matched.end();
    }
    position.literal(&command[offset..], &mut output);
    output
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(command: &str) -> String {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("SHELL_REDACTION_TEST", "expanded")
            .output()
            .expect("local POSIX shell");
        assert!(output.status.success(), "synthetic shell invocation failed");
        assert!(output.stderr.is_empty(), "synthetic shell wrote stderr");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn shell_arguments_recover_literal_bytes_in_all_common_quote_contexts() {
        let session = RedactionSession::new();
        let secret = "a'b\"c $SHELL_REDACTION_TEST `printf injected` \\\n;literal";
        let marker = session.marker(json!(secret));
        for command in [
            format!("printf '%s' {marker}"),
            format!("printf '%s' '{marker}'"),
            format!("printf '%s' \"{marker}\""),
            format!("printf '%s' prefix{marker}suffix"),
            format!("printf '%s' 'prefix{marker}suffix'"),
            format!("printf '%s' \"prefix{marker}suffix\""),
        ] {
            let expected = if command.contains("prefix") {
                format!("prefix{secret}suffix")
            } else {
                secret.to_owned()
            };
            let mut input = json!({"command":command});
            session.restore_tool_input_for_tool("Bash", &mut input);
            assert_eq!(run(input["command"].as_str().unwrap()), expected);
        }
    }

    #[test]
    fn marker_escapes_and_namespace_names_preserve_shell_words() {
        let session = RedactionSession::new();
        let secret = "\"'$synthetic; rest";
        let marker = session.marker(json!(secret));
        for tool in ["Bash", "functions.exec_command", "mcp__terminal__run_shell"] {
            let mut input = json!({"cmd":format!("printf '%s' \\{marker}")});
            session.restore_tool_input_for_tool(tool, &mut input);
            assert_eq!(run(input["cmd"].as_str().unwrap()), secret);
            let mut input = json!({"cmd":format!("printf '%s' \"\\{marker}\"")});
            session.restore_tool_input_for_tool(tool, &mut input);
            assert_eq!(run(input["cmd"].as_str().unwrap()), format!("\\{secret}"));
        }
    }

    #[test]
    fn only_known_shell_command_fields_receive_shell_escaping() {
        let session = RedactionSession::new();
        let secret = "a'b\"c";
        let marker = session.marker(json!(secret));
        let mut non_shell = json!({"command":marker,"token":marker});
        session.restore_tool_input_for_tool("http_request", &mut non_shell);
        assert_eq!(non_shell, json!({"command":secret,"token":secret}));
        let mut shell =
            json!({"command":format!("printf '%s' {marker}"),"token":marker,"timeout":10});
        session.restore_tool_input_for_tool("Bash", &mut shell);
        assert_eq!(shell["token"], secret);
        assert_eq!(shell["timeout"], 10);
        assert_eq!(run(shell["command"].as_str().unwrap()), secret);
    }

    #[test]
    fn symbolic_references_unknown_markers_and_original_marker_text_are_not_expanded() {
        let session = RedactionSession::new();
        let other = RedactionSession::new();
        let foreign = other.marker(json!("foreign-secret"));
        let literal = session.marker(json!(foreign));
        let command = format!("printf '%s' \"$SHELL_REDACTION_TEST\" '{foreign}' {literal}");
        let mut input = json!({"command":command});
        session.restore_tool_input_for_tool("Bash", &mut input);
        let restored = input["command"].as_str().unwrap();
        assert!(restored.contains("$SHELL_REDACTION_TEST"));
        assert_eq!(run(restored), format!("expanded{foreign}{foreign}"));
        let mut untouched = json!({"command":"curl -H 'Authorization: Bearer ${API_TOKEN}' https://example.invalid/"});
        let before = untouched.clone();
        session.restore_tool_input_for_tool("Bash", &mut untouched);
        assert_eq!(untouched, before);
        let current_marker = session.marker(json!("must-not-expand"));
        let literal_marker = session.marker(json!(current_marker));
        let mut literal = json!({"command":format!("printf '%s' {literal_marker}")});
        session.restore_tool_input_for_tool("Bash", &mut literal);
        assert_eq!(run(literal["command"].as_str().unwrap()), current_marker);
    }

    #[test]
    fn explicit_non_posix_shells_use_plain_json_restoration() {
        let session = RedactionSession::new();
        let secret = "synthetic $value with 'quotes'";
        let marker = session.marker(json!(secret));
        for shell in [
            "pwsh",
            "powershell.exe",
            "cmd",
            "cmd.exe",
            r"C:\Windows\System32\cmd",
        ] {
            let mut input = json!({"shell":shell,"cmd":marker});
            session.restore_tool_input_for_tool("exec_command", &mut input);
            assert_eq!(input["cmd"], secret);
        }
    }

    #[test]
    fn multiline_references_in_comments_cannot_create_executable_lines() {
        let session = RedactionSession::new();
        let marker = session.marker(json!("comment value\nprintf injected"));
        for comment in ["# note:", "\\\n# note:", "true; \\\n# note:"] {
            let mut input = json!({"command":format!("{comment} {marker}\nprintf '%s' safe")});
            session.restore_tool_input_for_tool("Bash", &mut input);
            assert_eq!(run(input["command"].as_str().unwrap()), "safe");
        }
    }

    #[test]
    fn curl_headers_and_environment_assignments_keep_argument_boundaries() {
        let session = RedactionSession::new();
        let secret = "a'b\\c $SHELL_REDACTION_TEST;literal";
        let marker = session.marker(json!(secret));
        let mut input = json!({"command":format!(
            "CLOUD_TOKEN={marker}; export CLOUD_TOKEN; curl() {{ printf '%s\\n' \"$CLOUD_TOKEN\" \"$#\" \"$@\"; }}; curl -H \"Authorization: Bearer {marker}\" https://example.invalid/path"
        )});
        session.restore_tool_input_for_tool("Bash", &mut input);
        assert_eq!(
            run(input["command"].as_str().unwrap()),
            format!(
                "{secret}\n3\n-H\nAuthorization: Bearer {secret}\nhttps://example.invalid/path\n"
            )
        );
    }

    #[test]
    fn redacted_shell_source_recovers_existing_escape_semantics() {
        for command in [
            r#"printf '%s\n' --token "ksk_existing\\backslash""#,
            r#"printf '%s\n' --token "ksk_existing\"quote""#,
            r#"printf '%s\n' --token ksk_existing\$dollar"#,
        ] {
            let session = RedactionSession::new();
            let masked = session.redact_text(command, &[]);
            assert_ne!(masked, command);
            let mut input = json!({"command":masked});
            session.restore_tool_input_for_tool("Bash", &mut input);
            assert_eq!(run(input["command"].as_str().unwrap()), run(command));
        }
    }

    #[test]
    fn bash_ansi_quoted_references_recover_literal_bytes() {
        let session = RedactionSession::new();
        let secret = "a'b\\n\\t \\\' \\\\ $SHELL_REDACTION_TEST";
        let marker = session.marker(json!(secret));
        for command in [
            format!("printf '%s' $'{marker}'"),
            format!("printf '%s' $'\\{marker}'"),
        ] {
            let leading_slash = command.contains("$'\\");
            let mut input = json!({"command":command});
            session.restore_tool_input_for_tool("Bash", &mut input);
            let output = std::process::Command::new("bash")
                .arg("-c")
                .arg(input["command"].as_str().unwrap())
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                if leading_slash {
                    format!("\\{secret}")
                } else {
                    secret.to_owned()
                }
            );
        }
    }

    #[test]
    fn known_shell_source_markers_move_as_semantic_values_to_other_tools() {
        for (command, expected) in [
            (
                r#"printf '%s' --token "ksk_double\\backslash\"quote\$dollar""#,
                r#"ksk_double\backslash"quote$dollar"#,
            ),
            (
                r#"printf '%s' --token ksk_unquoted\\backslash\$dollar"#,
                r#"ksk_unquoted\backslash$dollar"#,
            ),
            (
                r#"printf '%s' --token 'ksk_single\\backslash'"#,
                r#"ksk_single\\backslash"#,
            ),
            (
                r#"printf '%s' --token $'ksk_ansi\\backslash\nline\'quote'"#,
                "ksk_ansi\\backslash\nline'quote",
            ),
        ] {
            let session = RedactionSession::new();
            let original = json!({"command":command});
            let mut safe = json!({"command":session.redact_text(command, &[])});
            let raw_marker = marker_pattern()
                .find(safe["command"].as_str().unwrap())
                .unwrap()
                .as_str()
                .to_owned();
            let mut raw_before = json!({"literal":raw_marker});
            session.restore_tool_input(&mut raw_before);
            normalize_shell_markers("Bash", &original, &mut safe, &session);
            let mut raw_after = json!({"literal":raw_marker});
            session.restore_tool_input(&mut raw_after);
            assert_eq!(raw_after, raw_before);
            let marker = marker_pattern()
                .find(safe["command"].as_str().unwrap())
                .unwrap()
                .as_str();
            let mut http = json!({"token":marker});
            session.restore_tool_input_for_tool("http_request", &mut http);
            assert_eq!(http["token"], expected);
            let mut next = json!({"command":format!("printf '%s' \"{marker}\"")});
            session.restore_tool_input_for_tool("Bash", &mut next);
            assert_eq!(run(next["command"].as_str().unwrap()), expected);
            session.restore_tool_input_for_tool("Bash", &mut safe);
            assert_eq!(safe, original);
            assert!(discover_shell_secrets("Bash", &original).contains(&expected.to_owned()));
        }
    }

    #[test]
    fn known_shell_environment_assignments_preserve_following_command_structure() {
        let command = r#"CLOUD_TOKEN=opaque\ secret printf '%s' ready"#;
        let mut payload: crate::KiroPayload = serde_json::from_value(json!({
            "conversationState": {
                "chatTriggerType":"MANUAL", "conversationId":"shell-source",
                "history":[{"assistantResponseMessage":{
                    "content":"", "toolUses":[{
                        "toolUseId":"shell-call", "name":"Bash", "input":{"command":command}
                    }]
                }}],
                "currentMessage":{"userInputMessage":{
                    "content":"reuse opaque secret", "modelId":"model", "origin":"AI_EDITOR"
                }}
            }
        }))
        .unwrap();
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        let safe = &payload.conversation_state.history[0]
            .assistant_response_message
            .as_ref()
            .unwrap()
            .tool_uses[0]
            .input;
        let masked = safe["command"].as_str().unwrap();
        assert!(masked.contains("printf '%s' ready"));
        assert!(!masked.contains("opaque"));
        let marker = marker_pattern().find(masked).unwrap().as_str();
        let mut http = json!({"token":marker});
        session.restore_tool_input_for_tool("http_request", &mut http);
        assert_eq!(http["token"], "opaque secret");
        assert!(!payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .contains("opaque secret"));
        let mut replay = safe.clone();
        session.restore_tool_input_for_tool("Bash", &mut replay);
        assert_eq!(replay["command"], command);
        assert_eq!(run(replay["command"].as_str().unwrap()), "ready");
    }
}
