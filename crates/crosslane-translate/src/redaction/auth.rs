//! Authentication syntax that needs more context than a sensitive field name.
//! No shell expansion, SDK evaluation, credential lookup, or network I/O occurs.

use std::{collections::BTreeMap, ops::Range, sync::OnceLock};

use base64::Engine;
use regex::Regex;
use serde_json::{Map, Value};

use super::{regex, sensitive_name, symbolic_pattern, value_range, ValueContext};

#[derive(Default)]
pub(super) struct Scan {
    pub secrets: Vec<Range<usize>>,
    pub literal_defaults: Vec<Range<usize>>,
    pub posix_literals: Vec<Range<usize>>,
    // A recognized option has its own grammar. Do not let the generic field /
    // long-option scanner consume its path, prompt flag, or following command.
    managed: Vec<Range<usize>>,
    literals: BTreeMap<(usize, usize), String>,
}

impl Scan {
    pub fn manages(&self, offset: usize) -> bool {
        let index = self.managed.partition_point(|range| range.start <= offset);
        index > 0 && self.managed[index - 1].contains(&offset)
    }

    pub fn literal(&self, range: &Range<usize>) -> Option<&str> {
        self.literals
            .get(&(range.start, range.end))
            .map(String::as_str)
    }

    pub fn literal_secrets(&self) -> impl Iterator<Item = &str> {
        self.literals.values().map(String::as_str)
    }

    pub fn literal_entries(&self) -> impl Iterator<Item = (Range<usize>, &str)> {
        self.literals
            .iter()
            .map(|(&(start, end), value)| (start..end, value.as_str()))
    }
}

fn compact(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

pub(super) fn encoded_basic(value: &str) -> bool {
    let value = value.trim();
    [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
    ]
    .iter()
    .any(|engine| engine.decode(value).is_ok_and(|raw| raw.contains(&b':')))
}

pub(super) fn credential_field(name: &str, value: &str) -> bool {
    let name = name.trim_matches(['"', '\'', '\\']);
    (matches!(name, "auth" | "_auth") && encoded_basic(value))
        || (name == "auth"
            && value.contains(':')
            && !["https://", "http://"]
                .iter()
                .any(|prefix| value.starts_with(prefix)))
        || (matches!(compact(name).as_str(), "basicauth" | "proxybasicauth") && value.contains(':'))
}

pub(super) fn header_name(name: &str) -> bool {
    matches!(
        compact(name).as_str(),
        "authorization" | "proxyauthorization" | "cookie" | "setcookie" | "dpop" | "xwsse"
    ) || (name.to_ascii_lowercase().starts_with("x-") && sensitive_name(name))
}

pub(super) fn challenge_header(name: &str) -> bool {
    matches!(
        compact(name).as_str(),
        "wwwauthenticate" | "proxyauthenticate"
    )
}

pub(super) fn credential_values<'a>(name: &str, value: &'a mut Value) -> Vec<&'a mut Value> {
    if !credential_container_property(name) {
        return Vec::new();
    }
    match value {
        Value::Array(pair)
            if basic_auth_property(name) && pair.len() == 2 && pair[0].is_string() =>
        {
            pair.get_mut(1).into_iter().collect()
        }
        Value::Object(fields) => {
            let basic = fields.contains_key("user") || fields.contains_key("username");
            fields
                .iter_mut()
                .filter(|(name, child)| {
                    (credential_reference_name(name)
                        || (basic && (name.as_str() == "pass" || sensitive_name(name))))
                        && !public_field(name, child)
                })
                .map(|(_, child)| child)
                .collect()
        }
        _ => Vec::new(),
    }
}

pub(super) fn basic_auth_property(name: &str) -> bool {
    matches!(compact(name).as_str(), "auth" | "basicauth" | "proxyauth")
}

pub(super) fn authentication_property(name: &str) -> bool {
    basic_auth_property(name) || compact(name) == "authentication"
}

pub(super) fn credential_container_property(name: &str) -> bool {
    authentication_property(name)
        || matches!(
            compact(name).as_str(),
            "credentials" | "encryption" | "decryption" | "signing" | "crypto"
        )
}

pub(super) fn credential_reference_name(name: &str) -> bool {
    let name = compact(name);
    name == "key"
        || super::policy::business_key_name(&name)
        || super::policy::business_token_name(&name)
}

pub(super) fn credential_reference_environment(name: &str) -> bool {
    credential_reference_name(name)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
}

/// A bare `key` is ambiguous. Preserve protection when the surrounding syntax
/// explicitly uses it for authentication, including opaque keys without a
/// provider prefix. Ordinary object literals such as `{key: 'perf'}` do not.
pub(super) fn credential_reference_assignment(name: &str, prefix: &str) -> bool {
    let name = name.trim_matches(['"', '\'', '\\']);
    if !credential_reference_name(name) {
        return false;
    }
    let generic = name.eq_ignore_ascii_case("key");
    if generic && (prefix.ends_with(['?', '&']) || prefix.ends_with("--")) {
        return true;
    }
    if credential_reference_environment(name) && !prefix.trim_end().ends_with(['{', ',']) {
        return true;
    }
    credential_object_prefix(prefix)
}

pub(super) fn credential_object_prefix(prefix: &str) -> bool {
    let start = prefix
        .char_indices()
        .rev()
        .nth(2048)
        .map_or(0, |(offset, _)| offset);
    static AUTH_OBJECT: OnceLock<Regex> = OnceLock::new();
    regex(
        &AUTH_OBJECT,
        r#"(?P<name>(?:\\?["'])?[\p{L}_][\p{L}\p{N}_.-]{0,127}(?:\\?["'])?)[ \t]*[:=]\s*[\[{]"#,
    )
    .captures_iter(&prefix[start..])
    .filter(|captures| {
        captures.name("name").is_some_and(|name| {
            credential_container_property(name.as_str().trim_matches(['"', '\'', '\\']))
                || sensitive_name(name.as_str())
        })
    })
    .map(|captures| captures.get(0).expect("credential container"))
    .any(|found| {
        let mut depth = 1usize;
        let mut quote = None;
        let mut escaped = false;
        for ch in prefix[start + found.end()..].chars() {
            if escaped {
                escaped = false;
                continue;
            }
            if let Some(delimiter) = quote {
                if ch == '\\' {
                    escaped = true;
                } else if ch == delimiter {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth > 0
    })
}

fn public_boolean_name(name: &str) -> bool {
    let name = compact(name);
    name == "accesscontrolallowcredentials"
        || [
            "has", "is", "require", "requires", "enable", "supports", "with", "use", "need",
            "needs", "allow",
        ]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                sensitive_name(suffix) || super::policy::ordinary_field_name(suffix)
            })
        })
}

pub(super) fn public_field(name: &str, value: &Value) -> bool {
    (public_boolean_name(name)
        && (value.is_boolean()
            || value.as_str().is_some_and(|text| {
                matches!(text.trim().to_ascii_lowercase().as_str(), "true" | "false")
            })))
        || empty_auth_header(name, value.as_str().unwrap_or(""))
        || value
            .as_str()
            .is_some_and(|text| super::policy::public_value(name, text))
}

fn empty_auth_header(name: &str, value: &str) -> bool {
    matches!(
        compact(name).as_str(),
        "authorization" | "proxyauthorization"
    ) && is_scheme(value.trim())
}

pub(super) fn public_boolean_schema(name: &str, schema: &Value) -> bool {
    public_boolean_name(name) && schema.get("type").and_then(Value::as_str) == Some("boolean")
}

pub(super) fn oauth_field(name: &str, fields: &Map<String, Value>) -> bool {
    matches!(
        (name, fields.get("grant_type").and_then(Value::as_str)),
        ("code", Some("authorization_code"))
            | (
                "assertion",
                Some(
                    "urn:ietf:params:oauth:grant-type:jwt-bearer"
                        | "urn:ietf:params:oauth:grant-type:saml2-bearer",
                ),
            )
    )
}

pub(super) fn public_assignment(name: &str, value: &str) -> bool {
    (public_boolean_name(name)
        && matches!(
            value
                .trim()
                .trim_matches(['"', '\''])
                .to_ascii_lowercase()
                .as_str(),
            "true" | "false"
        ))
        || empty_auth_header(name, value)
        || super::policy::public_value(name, value)
}

pub(super) fn fetch_options(fields: &Map<String, Value>) -> bool {
    matches!(
        fields.get("credentials").and_then(Value::as_str),
        Some("include" | "omit" | "same-origin")
    ) && (matches!(
        fields.get("mode").and_then(Value::as_str),
        Some("cors" | "no-cors" | "same-origin")
    ) || fields
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| {
            matches!(
                method.to_ascii_uppercase().as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            )
        }))
}

/// Form and database separators only apply when another named field follows.
/// A standalone .env password such as `a;b&c` still occupies the whole line.
pub(super) fn delimited_context(text: &str, start: usize) -> Option<ValueContext> {
    let tail = window(&text[start..], 4096).split(['\r', '\n']).next()?;
    let prefix_start = text[..start]
        .char_indices()
        .rev()
        .nth(4096)
        .map_or(0, |(offset, _)| offset);
    let prefix = text[prefix_start..start].rsplit(['\r', '\n']).next()?;
    static FORM: OnceLock<Regex> = OnceLock::new();
    static CONNECTION: OnceLock<Regex> = OnceLock::new();
    let oauth_form = prefix.contains("grant_type=") || tail.contains("&grant_type=");
    let curl_form = [" -d ", " --data", " --json", " --form"]
        .iter()
        .any(|flag| prefix.contains(flag));
    if (oauth_form || curl_form) && regex(&FORM, r"&[A-Za-z_][A-Za-z0-9_.-]*=").is_match(tail) {
        return Some(ValueContext::Query);
    }
    let connection_prefix = prefix.to_ascii_lowercase();
    if ["server=", "host=", "driver=", "data source=", "jdbc:"]
        .iter().any(|name| connection_prefix.contains(name)) && regex(
        &CONNECTION,
        r"(?i);[ \t]*(?:server|host|port|database|initial catalog|data source|user id|uid|user|username|password|pwd|sslmode|encrypt|trustservercertificate|application name|connection timeout|pooling)[ \t]*=",
    )
    .is_match(tail)
    {
        return Some(ValueContext::Connection);
    }
    None
}

fn window(text: &str, length: usize) -> &str {
    let mut end = text.len().min(length);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn oauth_assignment(text: &str, name: &str, start: usize) -> bool {
    let name = name.trim_matches(['"', '\'', '\\']);
    if !matches!(name, "code" | "assertion") {
        return false;
    }
    let prefix_start = text[..start]
        .char_indices()
        .rev()
        .nth(4096)
        .map_or(0, |(offset, _)| offset);
    let context = &text[prefix_start..start + window(&text[start..], 4096).len()];
    (name == "code" && context.contains("grant_type=authorization_code"))
        || (name == "assertion"
            && (context.contains("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer")
                || context.contains("grant_type=urn:ietf:params:oauth:grant-type:saml2-bearer")))
}

pub(super) fn scan(text: &str) -> Scan {
    let mut scan = Scan::default();
    // Keep the common large-history path cheap. These are literal syntax
    // prefixes, not guesses based on password strength or token entropy.
    if [
        "curl",
        "wget",
        "http ",
        "https ",
        "xh ",
        "redis-cli",
        "mysql",
        "mariadb",
        "psql",
        "pg_dump",
        "pg_restore",
        "sshpass",
        "openssl",
    ]
    .iter()
    .any(|name| text.contains(name))
    {
        scan_commands(text, &mut scan);
    }
    if text.contains("Auth(")
        || text.contains("auth")
        || text.contains("fetch")
        || text.contains('[')
    {
        scan_sdk(text, &mut scan);
    }
    if text.contains("machine") || text.contains("default") {
        scan_netrc(text, &mut scan);
    }
    for (expression, secret) in environment_defaults(text) {
        scan.managed.push(expression);
        scan.literal_defaults.push(secret);
    }
    // Authentication arguments/SDK string literals/netrc data do not execute
    // source-language getter expressions. A password literally spelling
    // `System.getenv("NAME")` must not acquire a reference exemption.
    for range in &scan.secrets {
        for reference in symbolic_pattern().find_iter(&text[range.clone()]) {
            if super::named_getter_reference(reference.as_str()) {
                scan.literal_defaults
                    .push(range.start + reference.start()..range.start + reference.end());
            }
        }
    }
    scan.managed
        .sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in scan.managed {
        if let Some(previous) = merged.last_mut() {
            if range.start <= previous.end {
                previous.end = previous.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    scan.managed = merged;
    scan
}

fn environment_pattern(anchored: bool) -> &'static Regex {
    static ANY: OnceLock<Regex> = OnceLock::new();
    static AT_START: OnceLock<Regex> = OnceLock::new();
    (if anchored { &AT_START } else { &ANY }).get_or_init(|| {
        let literal = r#"(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')"#;
        let name = r#"(?:"[A-Za-z_][A-Za-z0-9_.-]*"|'[A-Za-z_][A-Za-z0-9_.-]*')"#;
        let anchor = if anchored { r"\A" } else { "" };
        let getter = format!(r#"(?:(?:process|Bun|import\.meta)\.env(?:\.(?P<js>[A-Za-z_][A-Za-z0-9_]*)|\[\s*(?P<bracket>{name})\s*\])|(?:Deno\.env\.get|System\.getenv|(?:System\.)?Environment\.GetEnvironmentVariable|getenv)\s*\(\s*(?P<call>{name})\s*\))"#);
        let rust = format!(r#"(?:std::)?env::var(?:_os)?\s*\(\s*(?P<rust>{name})\s*\)(?:\s*\.ok\(\))?\s*\.unwrap_or(?:_else)?\(\s*(?:(?:\|\s*[A-Za-z_][A-Za-z0-9_]*\s*\||\|\|)\s*)?(?P<rust_default>{literal})(?:\.(?:to_owned|to_string|into)\(\))?\s*\)"#);
        Regex::new(&format!(r#"{anchor}(?:{getter}\s*(?:\|\||\?\?|\?:|\+|or\b)\s*(?P<js_default>{literal})|(?:os\.(?:getenv|environ\.get)|ENV\.fetch)\s*\(\s*(?P<py>{name})\s*,\s*(?P<py_default>{literal})\s*\)|{rust})"#)).expect("literal environment default")
    })
}

fn environment_default(captures: &regex::Captures<'_>) -> Option<Range<usize>> {
    let secret = captures
        .name("js_default")
        .or_else(|| captures.name("py_default"))
        .or_else(|| captures.name("rust_default"))?;
    Some(unquote(secret.as_str(), secret.start()))
}

fn environment_defaults(text: &str) -> Vec<(Range<usize>, Range<usize>)> {
    if !text.contains("env") && !text.contains("Env") && !text.contains("ENV") {
        return Vec::new();
    }
    environment_pattern(false)
        .captures_iter(text)
        .filter_map(|captures| {
            let name = captures
                .name("js")
                .or_else(|| captures.name("bracket"))
                .or_else(|| captures.name("py"))
                .or_else(|| captures.name("call"))
                .or_else(|| captures.name("rust"))?;
            let name = name.as_str().trim_matches(['"', '\'']);
            if !sensitive_name(name) && name != "PASS" && !credential_reference_environment(name) {
                return None;
            }
            Some((captures.get(0)?.range(), environment_default(&captures)?))
        })
        .collect()
}

/// A labelled password/token field supplies sensitivity even when the
/// environment variable itself has an ordinary name such as APP_SETTING.
pub(super) fn environment_literal_at(text: &str, start: usize) -> Option<Range<usize>> {
    let tail = text.get(start..)?;
    let captures = environment_pattern(true).captures(tail)?;
    let range = environment_default(&captures)?;
    Some(start + range.start..start + range.end)
}

pub(super) fn environment_expression(text: &str) -> Option<Range<usize>> {
    let trimmed = text.trim();
    let captures = environment_pattern(true).captures(trimmed)?;
    if captures.get(0)?.end() != trimmed.len() {
        return None;
    }
    let range = environment_default(&captures)?;
    let offset = text.len() - text.trim_start().len();
    Some(offset + range.start..offset + range.end)
}

#[derive(Clone, Debug)]
struct Word<'a> {
    raw: &'a str,
    range: Range<usize>,
    segment: usize,
}

impl Word<'_> {
    fn content(&self) -> Range<usize> {
        unquote(self.raw, self.range.start)
    }

    fn text(&self) -> &str {
        let range = self.content();
        &self.raw[range.start - self.range.start..range.end - self.range.start]
    }
}

fn unquote(text: &str, start: usize) -> Range<usize> {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'\'' | b'"') && bytes.last() == Some(&bytes[0]) {
        start + 1..start + text.len() - 1
    } else if text.len() >= 3 && text.starts_with("$'") && text.ends_with('\'') {
        start + 2..start + text.len() - 1
    } else {
        start..start + text.len()
    }
}

/// A sensitive shell assignment consumes one complete word, including quote
/// concatenations. Delimiters inside that word must not expose a trailing part.
pub(super) fn shell_value_range(text: &str, start: usize) -> Option<Range<usize>> {
    let mut words = Words::new(text.get(start..)?);
    words.strict = true;
    let word = words.next()?;
    (word.range.start == 0).then(|| {
        let range = word.content();
        start + range.start..start + range.end
    })
}

/// Streaming token spans: large harmless history does not allocate a word
/// vector proportional to its size. Quotes/expansions are never evaluated.
#[derive(Clone)]
struct Words<'a> {
    text: &'a str,
    characters: std::str::CharIndices<'a>,
    segment: usize,
    strict: bool,
    peeked: Option<Word<'a>>,
}

impl<'a> Words<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            characters: text.char_indices(),
            segment: 0,
            strict: false,
            peeked: None,
        }
    }

    fn peek(&mut self) -> Option<&Word<'a>> {
        if self.peeked.is_none() {
            self.peeked = self.read();
        }
        self.peeked.as_ref()
    }

    fn argument(&mut self, segment: usize) -> Option<Word<'a>> {
        self.peek().filter(|word| word.segment == segment)?;
        self.next()
    }

    fn read(&mut self) -> Option<Word<'a>> {
        let mut start = None;
        let mut quote = None;
        let mut escaped = false;
        while let Some((offset, ch)) = self.characters.next() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' && quote != Some('\'') {
                if start.is_none()
                    && self
                        .characters
                        .clone()
                        .next()
                        .is_some_and(|(_, next)| next == '\n')
                {
                    self.characters.next();
                    continue;
                }
                start.get_or_insert(offset);
                escaped = true;
                continue;
            }
            // Prose often precedes a command ("I'd like curl ..."). Once a
            // command starts, its arguments use literal shell quote rules.
            let contraction = !self.strict
                && ch == '\''
                && quote.is_none()
                && self.text[..offset]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_ascii_alphabetic())
                && self
                    .characters
                    .clone()
                    .next()
                    .is_some_and(|(_, ch)| ch.is_ascii_alphabetic());
            if contraction {
                start.get_or_insert(offset);
            } else if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() && matches!(ch, '\'' | '"') {
                start.get_or_insert(offset);
                quote = Some(ch);
            } else if quote.is_none() && (ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '`'))
            {
                let segment = self.segment;
                if matches!(ch, '\r' | '\n' | ';' | '|' | '&' | '`') {
                    self.segment += 1;
                    self.strict = false;
                }
                if let Some(begin) = start.take() {
                    return Some(Word {
                        raw: &self.text[begin..offset],
                        range: begin..offset,
                        segment,
                    });
                }
            } else {
                start.get_or_insert(offset);
            }
        }
        start.map(|begin| Word {
            raw: &self.text[begin..],
            range: begin..self.text.len(),
            segment: self.segment,
        })
    }
}

impl<'a> Iterator for Words<'a> {
    type Item = Word<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.peeked.take().or_else(|| self.read())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Curl,
    Wget,
    Httpie,
    Redis,
    Mysql,
    Postgres,
    Sshpass,
    Openssl,
}

fn command(word: &str) -> Option<Command> {
    let file = word.rsplit(['/', '\\']).next()?;
    let name = file.strip_suffix(".exe").unwrap_or(file);
    match name {
        "curl" => Some(Command::Curl),
        "wget" => Some(Command::Wget),
        "http" | "https" | "xh" => Some(Command::Httpie),
        "redis-cli" => Some(Command::Redis),
        "mysql" | "mariadb" => Some(Command::Mysql),
        "psql" | "pg_dump" | "pg_restore" => Some(Command::Postgres),
        "sshpass" => Some(Command::Sshpass),
        "openssl" => Some(Command::Openssl),
        _ => None,
    }
}

fn takes_other_argument(command: Command, flag: &str) -> bool {
    match command {
        Command::Curl => matches!(
            flag,
            "-A" | "-d"
                | "-F"
                | "-o"
                | "-e"
                | "-x"
                | "-X"
                | "-T"
                | "-K"
                | "-c"
                | "--data"
                | "--data-raw"
                | "--data-binary"
                | "--data-urlencode"
                | "--json"
                | "--form"
                | "--form-string"
                | "--url"
                | "--request"
                | "--user-agent"
                | "--output"
                | "--referer"
                | "--proxy"
                | "--preproxy"
                | "--resolve"
                | "--connect-to"
                | "--config"
                | "--upload-file"
                | "--cacert"
                | "--capath"
                | "--connect-timeout"
                | "--max-time"
                | "--retry"
                | "--tlsuser"
                | "--proxy-tlsuser"
        ),
        Command::Httpie => matches!(
            flag,
            "-A" | "--auth-type" | "--session" | "--session-read-only" | "--output"
        ),
        Command::Sshpass => matches!(flag, "-f" | "-d"),
        Command::Mysql => matches!(
            flag,
            "-u" | "--user"
                | "-h"
                | "--host"
                | "-P"
                | "--port"
                | "-e"
                | "--execute"
                | "-D"
                | "--database"
        ),
        Command::Wget => matches!(
            flag,
            "--user"
                | "--http-user"
                | "--ftp-user"
                | "--proxy-user"
                | "--post-data"
                | "--post-file"
                | "-O"
                | "--output-document"
        ),
        Command::Redis => matches!(flag, "--user" | "-h" | "-p" | "-n" | "-u"),
        Command::Postgres => matches!(
            flag,
            "-U" | "--username"
                | "-h"
                | "--host"
                | "-p"
                | "--port"
                | "-d"
                | "--dbname"
                | "-c"
                | "--command"
        ),
        Command::Openssl => matches!(
            flag,
            "-in" | "-out" | "-inkey" | "-key" | "-cert" | "-CAfile"
        ),
    }
}

#[derive(Clone, Copy)]
enum Argument {
    Secret,
    Pair,
    Cookie,
    Certificate,
    Header,
    Public,
    Prompt,
    PasswordSource,
}

fn long_option(command: Command, flag: &str) -> Option<Argument> {
    use Argument::*;
    match (command, flag) {
        (Command::Curl, "--user" | "--proxy-user") => Some(Pair),
        (
            Command::Curl,
            "--oauth2-bearer"
            | "--pass"
            | "--proxy-pass"
            | "--tlspassword"
            | "--proxy-tlspassword"
            | "--ftp-account",
        ) => Some(Secret),
        (Command::Curl, "--cookie") => Some(Cookie),
        (Command::Curl, "--cert" | "--proxy-cert") => Some(Certificate),
        (Command::Curl, "--key" | "--proxy-key" | "--netrc-file" | "--cookie-jar") => Some(Public),
        (Command::Curl, "--header" | "--proxy-header") => Some(Header),
        (
            Command::Wget,
            "--password" | "--http-password" | "--ftp-password" | "--proxy-password",
        ) => Some(Secret),
        (Command::Wget, "--header") => Some(Header),
        (Command::Httpie, "--auth") => Some(Pair),
        (Command::Redis, "--pass") => Some(Secret),
        (Command::Mysql, "--password" | "--password1" | "--password2" | "--password3") => {
            Some(Prompt)
        }
        (Command::Postgres, "--password" | "--no-password") => Some(Prompt),
        (Command::Openssl, "-passin" | "-passout" | "-pass") => Some(PasswordSource),
        _ => None,
    }
}

fn short_option(command: Command, flag: &str) -> Option<(Argument, usize)> {
    use Argument::*;
    let bytes = flag.as_bytes();
    if bytes.first() != Some(&b'-') || bytes.get(1) == Some(&b'-') {
        return None;
    }
    if command == Command::Curl {
        for (index, byte) in bytes.iter().enumerate().skip(1) {
            let argument = match byte {
                b'u' | b'U' => Pair,
                b'b' => Cookie,
                b'E' => Certificate,
                b'H' => Header,
                _ if b"fsSkLvIiNgq".contains(byte) => continue,
                _ => return None, // An earlier option may consume this suffix.
            };
            return Some((argument, index + 1));
        }
        return None;
    }
    let argument = match (command, bytes.get(1).copied()?) {
        (Command::Httpie, b'a') => Pair,
        (Command::Redis, b'a') | (Command::Sshpass, b'p') => Secret,
        (Command::Mysql, b'p') | (Command::Postgres, b'W' | b'w') => Prompt,
        _ => return None,
    };
    Some((argument, 2))
}

fn scan_commands(text: &str, scan: &mut Scan) {
    scan_command_words(text, scan, 0);
}

fn scan_nested_word(text: &str, word: &Word<'_>, scan: &mut Scan, depth: usize) {
    let content = word.content();
    if depth >= 8 || content == word.range || !word.text().contains(char::is_whitespace) {
        return;
    }
    let mut nested = Scan::default();
    scan_command_words(&text[content.clone()], &mut nested, depth + 1);
    scan.secrets.extend(
        nested
            .secrets
            .into_iter()
            .map(|range| content.start + range.start..content.start + range.end),
    );
    scan.managed.extend(
        nested
            .managed
            .into_iter()
            .map(|range| content.start + range.start..content.start + range.end),
    );
    scan.literals.extend(
        nested
            .literals
            .into_iter()
            .map(|((start, end), value)| ((content.start + start, content.start + end), value)),
    );
}

fn httpie_token_auth(mut words: Words<'_>, segment: usize) -> bool {
    let mut token_auth = false;
    while let Some(word) = words.argument(segment) {
        if word.raw == "--" {
            break;
        }
        if let Some(mode) = word
            .raw
            .strip_prefix("--auth-type=")
            .or_else(|| word.raw.strip_prefix("-A").filter(|mode| !mode.is_empty()))
        {
            token_auth = !matches!(mode, "basic" | "digest");
        } else if matches!(word.raw, "-A" | "--auth-type") {
            if let Some(mode) = words.argument(segment) {
                token_auth = !matches!(mode.text(), "basic" | "digest");
            }
        } else if matches!(word.raw, "-a" | "--auth")
            || takes_other_argument(Command::Httpie, word.raw)
        {
            words.argument(segment);
        }
    }
    token_auth
}

fn scan_command_words(text: &str, scan: &mut Scan, depth: usize) {
    let mut words = Words::new(text);
    while let Some(current) = words.next() {
        scan_nested_word(text, &current, scan, depth);
        let Some(cmd) = command(current.text()) else {
            continue;
        };
        words.strict = words.segment == current.segment;
        let token_auth =
            cmd == Command::Httpie && httpie_token_auth(words.clone(), current.segment);
        while words
            .peek()
            .is_some_and(|word| word.segment == current.segment)
        {
            // sshpass wraps another command; leave that command for the outer
            // scanner so e.g. ssh's -p port never becomes sshpass's password.
            if cmd == Command::Sshpass
                && words.peek().is_some_and(|word| !word.raw.starts_with('-'))
            {
                break;
            }
            let word = words.next().expect("peeked argument");
            scan_nested_word(text, &word, scan, depth);
            if word.raw == "--" {
                break;
            }
            let (option, joined) = word
                .raw
                .split_once('=')
                .map_or((word.raw, None), |(flag, value)| (flag, Some(value)));
            let matched = long_option(cmd, option)
                .map(|kind| (kind, joined.map(|value| word.range.end - value.len())))
                .or_else(|| {
                    short_option(cmd, word.raw).map(|(kind, offset)| {
                        (
                            kind,
                            (offset < word.raw.len()).then_some(word.range.start + offset),
                        )
                    })
                });
            let Some((kind, joined_start)) = matched else {
                if joined.is_none() && takes_other_argument(cmd, option) {
                    if let Some(value) = words.argument(current.segment) {
                        scan_nested_word(text, &value, scan, depth);
                    }
                }
                continue;
            };
            scan.managed.push(word.range.clone());
            let (value_word, range) = if let Some(start) = joined_start {
                let range = unquote(&text[start..word.range.end], start);
                (word, range)
            } else if matches!(kind, Argument::Prompt) {
                // Bare MySQL/Postgres password flags prompt interactively.
                continue;
            } else if let Some(next) = words.argument(current.segment) {
                scan.managed.push(next.range.clone());
                if !matches!(kind, Argument::Public) {
                    scan_nested_word(text, &next, scan, depth);
                }
                let range = next.content();
                (next, range)
            } else {
                continue;
            };
            record_argument(
                text,
                scan,
                &value_word,
                range,
                kind,
                token_auth,
                !current.text().ends_with(".exe"),
            );
        }
    }
}

fn credential_separator(value: &str) -> Option<usize> {
    let mut references = symbolic_pattern().find_iter(value).peekable();
    for (offset, _) in value.match_indices(':') {
        while references
            .peek()
            .is_some_and(|reference| reference.end() <= offset)
        {
            references.next();
        }
        if !references
            .peek()
            .is_some_and(|reference| reference.start() <= offset && offset < reference.end())
        {
            return Some(offset);
        }
    }
    None
}

fn record_argument(
    text: &str,
    scan: &mut Scan,
    word: &Word<'_>,
    range: Range<usize>,
    kind: Argument,
    token_auth: bool,
    decode_literal: bool,
) {
    let value = &text[range.clone()];
    let before = scan.secrets.len();
    match kind {
        Argument::Public => {}
        Argument::Secret | Argument::Prompt => scan.secrets.push(range),
        Argument::Pair => {
            if (value.contains(':') && value != ":") || token_auth {
                scan.secrets.push(range.clone());
                // The password can be repeated outside the command in
                // another message/result. Do not learn usernames alone.
                if let Some(separator) = credential_separator(value) {
                    scan.secrets.push(range.start + separator + 1..range.end);
                }
            }
        }
        Argument::Cookie => {
            if value.contains('=') {
                scan.secrets.push(range);
            }
        }
        Argument::Certificate => {
            if let Some((file, password)) = value.rsplit_once(':') {
                if file.len() > 1 && !password.is_empty() {
                    scan.secrets.push(range.end - password.len()..range.end);
                }
            }
        }
        Argument::PasswordSource => {
            if let Some(password) = value.strip_prefix("pass:") {
                scan.secrets.push(range.end - password.len()..range.end);
            }
        }
        Argument::Header => {
            if let Some((name, value)) = value.split_once(':') {
                if (sensitive_name(name.trim()) || name.trim().eq_ignore_ascii_case("key"))
                    && !public_assignment(name.trim(), value)
                {
                    let start = range.end - value.len();
                    let start = start + value.len() - value.trim_start().len();
                    scan.secrets.push(auth_value_range(text, start..range.end));
                } else if !matches!(
                    compact(name).as_str(),
                    "wwwauthenticate" | "proxyauthenticate"
                ) {
                    // Custom header values may themselves contain a
                    // labelled key/token. Bound this scan to the header
                    // argument so it cannot consume the URL or flags.
                    let header = &text[range.clone()];
                    for assignment in super::assignments(header) {
                        if sensitive_name(assignment.name) {
                            if let Some(inner) = value_range(
                                header,
                                assignment.value_start,
                                ValueContext::Line,
                                assignment.name,
                            ) {
                                if !public_assignment(assignment.name, &header[inner.clone()]) {
                                    scan.secrets
                                        .push(range.start + inner.start..range.start + inner.end);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if !decode_literal {
        return;
    }
    for secret in &scan.secrets[before..] {
        if word.range.start <= secret.start && secret.end <= word.range.end {
            if let Some(value) = super::shell::literal_word_fragment(
                word.raw,
                secret.start - word.range.start..secret.end - word.range.start,
            ) {
                // A successfully decoded POSIX word fragment is literal data.
                // Single quotes/ANSI quotes and `%NAME%` do not expand variables.
                scan.posix_literals.push(secret.clone());
                scan.literals.insert((secret.start, secret.end), value);
            }
        }
    }
}

pub(super) fn in_challenge(text: &str, offset: usize) -> bool {
    let start = text[..offset]
        .char_indices()
        .rev()
        .nth(256)
        .map_or(0, |(offset, _)| offset);
    let prefix = &text[start..offset];
    let line = prefix.rsplit(['\r', '\n']).next().unwrap_or(prefix);
    // Challenge headers contain public realm/nonce/algorithm parameters. They
    // are not outgoing Authorization credentials (notably `Bearer realm=...`).
    line.rsplit_once(':').is_some_and(|(name, _)| {
        let name = name
            .trim_end()
            .trim_end_matches(['"', '\'', '\\'])
            .to_ascii_lowercase();
        name.ends_with("www-authenticate") || name.ends_with("proxy-authenticate")
    })
}

pub(super) fn is_scheme(scheme: &str) -> bool {
    [
        "basic",
        "bearer",
        "digest",
        "negotiate",
        "ntlm",
        "oauth",
        "hawk",
        "signature",
        "aws4-hmac-sha256",
        "sharedkey",
        "sharedaccesssignature",
        "apikey",
        "token",
        "dpop",
    ]
    .contains(&scheme.to_ascii_lowercase().as_str())
}

fn auth_value_range(text: &str, range: Range<usize>) -> Range<usize> {
    let value = &text[range.clone()];
    if let Some((scheme, rest)) = value.split_once(char::is_whitespace) {
        if is_scheme(scheme) {
            return range.end - rest.trim_start().len()..range.end;
        }
    }
    range
}

fn scan_sdk(text: &str, scan: &mut Scan) {
    static SDK: OnceLock<Regex> = OnceLock::new();
    static ENCODED: OnceLock<Regex> = OnceLock::new();
    static FETCH: OnceLock<Regex> = OnceLock::new();
    static HEADER_PAIR: OnceLock<Regex> = OnceLock::new();
    static NODE_AUTH: OnceLock<Regex> = OnceLock::new();
    // Only literal arguments in documented authentication constructors/tuples.
    // Keep constructors, commas and quote delimiters intact for copied code.
    let literal = r#"(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')"#;
    let user = format!(
        r#"(?:{literal}|[A-Za-z_][A-Za-z0-9_.]*(?:\[\s*{literal}\s*\]|\(\s*{literal}\s*\))?)"#
    );
    let pattern = SDK.get_or_init(|| Regex::new(&format!(
        r"(?:\b(?:HTTPBasicAuth|HTTPDigestAuth|HttpNtlmAuth|BasicAuth|SetBasicAuth|setBasicAuth|basic_auth)\s*\(|\bauth\s*=\s*\()\s*(?:{user})\s*,\s*(?P<password>{literal})"
    )).expect("literal authentication constructor"));
    for captures in pattern.captures_iter(text) {
        let password = captures.name("password").expect("password literal");
        let range = unquote(password.as_str(), password.start());
        scan.literal_defaults.push(range.clone());
        scan.secrets.push(range);
    }
    let node = NODE_AUTH.get_or_init(|| Regex::new(&format!(r#"\b(?:auth|proxyAuth)\s*:\s*\{{[^{{}}\r\n]{{0,1024}}?\bpass\s*:\s*(?P<password>{literal})"#)).expect("literal Node auth password"));
    for captures in node.captures_iter(text) {
        let password = captures.name("password").expect("Node password literal");
        let range = unquote(password.as_str(), password.start());
        scan.literal_defaults.push(range.clone());
        scan.secrets.push(range);
    }
    if text.contains('[') {
        let headers = HEADER_PAIR.get_or_init(|| Regex::new(&format!(
            r#"(?i)\[\s*["'](?:authorization|proxy-authorization|cookie|set-cookie|x-api-key|x-auth-token|dpop|x-wsse)["']\s*,\s*(?P<value>{literal})"#
        )).expect("literal header pair"));
        for captures in headers.captures_iter(text) {
            let value = captures.name("value").expect("header value literal");
            let range = auth_value_range(text, unquote(value.as_str(), value.start()));
            scan.literal_defaults.push(range.clone());
            scan.secrets.push(range);
        }
    }
    for captures in regex(
        &ENCODED,
        r#"(?:^|[^\w.-])(?:["']?_auth["']?|["']auth["'])[ \t]*[:=][ \t]*["']?(?P<secret>[A-Za-z0-9+/]+={0,2})"#,
    )
    .captures_iter(text)
    {
        let secret = captures.name("secret").expect("encoded auth");
        if encoded_basic(secret.as_str()) {
            scan.secrets.push(secret.range());
        }
    }
    if text.contains("fetch") {
        for captures in regex(&FETCH, r#"\bfetch\s*\([^{}\r\n]{0,1024},\s*\{[^{}\r\n]{0,1024}?(?P<field>\bcredentials\s*:\s*["'](?:include|omit|same-origin)["'])"#).captures_iter(text) {
            scan.managed.push(captures.name("field").expect("fetch credentials mode").range());
        }
    }
}

fn scan_netrc(text: &str, scan: &mut Scan) {
    static ENTRY: OnceLock<Regex> = OnceLock::new();
    for entry in regex(
        &ENTRY,
        r"(?m)^[ \t]*(?:machine[ \t]+[^\s]+|default)[ \t\r\n]+",
    )
    .find_iter(text)
    {
        let mut offset = entry.end();
        // A netrc stanza consists only of these named fields. Stop before
        // macros or unrelated prose/commands; never treat `password reset` as
        // an unlabelled credential outside a stanza.
        for _ in 0..8 {
            let tail = &text[offset..];
            let Some(end) = tail.find(char::is_whitespace) else {
                break;
            };
            let name = &tail[..end];
            if !matches!(name, "login" | "password" | "account") {
                break;
            }
            offset += end;
            offset += text[offset..].len() - text[offset..].trim_start().len();
            let Some(range) = value_range(text, offset, ValueContext::Shell, name) else {
                break;
            };
            if matches!(name, "password" | "account") {
                scan.literal_defaults.push(range.clone());
                scan.secrets.push(range.clone());
            }
            offset = range.end;
            if text
                .as_bytes()
                .get(offset)
                .is_some_and(|ch| matches!(ch, b'\'' | b'"'))
            {
                offset += 1;
            }
            offset += text[offset..].len() - text[offset..].trim_start().len();
        }
    }
}

/// References plus separators contain no literal credential data. Do not turn
/// `"$USER:$PASS"` or `Bearer ${TOKEN}` into a marker for just the punctuation.
pub(super) fn reference_composition(text: &str) -> bool {
    let mut end = 0;
    let mut found = false;
    for reference in symbolic_pattern().find_iter(text) {
        if !text[end..reference.start()]
            .chars()
            .all(|ch| ch.is_whitespace() || matches!(ch, ':' | '"' | '\''))
        {
            return false;
        }
        found = true;
        end = reference.end();
    }
    found
        && text[end..]
            .chars()
            .all(|ch| ch.is_whitespace() || matches!(ch, ':' | '"' | '\''))
}

pub(super) fn only_references(text: &str) -> bool {
    reference_composition(text)
        && ![":-", ":+", ":?", ":="]
            .iter()
            .any(|operator| text.contains(operator))
}

#[cfg(test)]
mod tests {
    use crate::redaction::{redact_text, RedactionSession, REDACTED};
    use serde_json::json;

    fn round_trip(source: &str, secrets: &[&str], kept: &[&str]) {
        let session = RedactionSession::new();
        let safe = session.redact_text(source, &[]);
        for secret in secrets {
            assert!(
                !safe.contains(secret),
                "secret {secret:?} survived in {safe:?}"
            );
        }
        for public in kept {
            assert!(safe.contains(public), "lost {public:?} in {safe:?}");
        }
        assert_eq!(session.restore_text(&safe), source);
        let mut input = json!({"command":safe,"timeout":30000,"description":"auth fixture"});
        session.restore_tool_input_for_tool("Bash", &mut input);
        assert_eq!(
            input,
            json!({"command":source,"timeout":30000,"description":"auth fixture"})
        );
    }

    #[test]
    fn common_command_authentication_forms_are_reversible() {
        for source in [
            "看下 curl -u admin:admin https://example.invalid/",
            "I'd like you to run curl -u admin:admin https://example.invalid/",
            "curl\nI'd like curl -u admin:admin https://example.invalid/",
            "curl -uadmin:admin https://example.invalid/",
            "curl --user admin:admin https://example.invalid/",
            "curl --user='admin:admin' https://example.invalid/",
            "curl -fsSu \"admin:admin\" https://example.invalid/",
            "/usr/bin/curl --digest -u admin:admin https://example.invalid/",
            "curl --ntlm -U admin:admin -x http://proxy.invalid https://example.invalid/",
            "curl --aws-sigv4 aws:amz:us-east-1:s3 -u admin:admin https://example.invalid/",
            "http -a admin:admin https://example.invalid/",
            "https --auth=admin:admin -A digest https://example.invalid/",
            "xh -A basic -a admin:admin https://example.invalid/",
            "echo curl -u admin:admin https://example.invalid/",
            "echo 'curl -u admin:admin https://example.invalid/'",
            "curl \\\n -u admin:admin https://example.invalid/",
            "curl \\\n-u admin:admin https://example.invalid/",
        ] {
            round_trip(source, &["admin:admin"], &["https://example.invalid/"]);
        }
        for source in [
            "curl --oauth2-bearer opaque-auth-fixture https://example.invalid/",
            "curl --pass opaque-auth-fixture https://example.invalid/",
            "curl --proxy-pass=opaque-auth-fixture https://example.invalid/",
            "curl --tlspassword opaque-auth-fixture https://example.invalid/",
            "wget --http-password=opaque-auth-fixture https://example.invalid/",
            "wget --proxy-password opaque-auth-fixture https://example.invalid/",
            "http --auth-type=bearer -a opaque-auth-fixture https://example.invalid/",
            "http -Abearer -a opaque-auth-fixture https://example.invalid/",
            "http -A jwt -a opaque-auth-fixture https://example.invalid/",
            "redis-cli -a opaque-auth-fixture PING",
            "redis-cli --pass=opaque-auth-fixture PING",
            "redis-cli -a -opaque-auth-fixture PING",
            "mysql -popaque-auth-fixture appdb",
            "mysql --password=opaque-auth-fixture appdb",
            "mariadb --password2=opaque-auth-fixture appdb",
            "sshpass -p opaque-auth-fixture ssh -p 2222 user@example.invalid",
            "openssl pkey -in client.pem -passin pass:opaque-auth-fixture -out result.pem",
        ] {
            round_trip(source, &["opaque-auth-fixture"], &[]);
        }
    }

    #[test]
    fn copied_auth_markers_represent_literal_arguments_instead_of_shell_spelling() {
        for (source, expected) in [
            (
                r#"curl -u "user:pa\\ss\"word" https://example.invalid/"#,
                "user:pa\\ss\"word",
            ),
            (
                r#"curl -u 'user':'opaque-fixture' https://example.invalid/"#,
                "user:opaque-fixture",
            ),
            (
                r#"curl -u user:pa\\ss https://example.invalid/"#,
                "user:pa\\ss",
            ),
            (
                r#"curl -u $'user:opaque\nfixture' https://example.invalid/"#,
                "user:opaque\nfixture",
            ),
        ] {
            let session = RedactionSession::new();
            let safe = session.redact_text(source, &[]);
            assert_ne!(source, safe);
            assert_eq!(session.restore_text(&safe), source);
            let marker = crate::redaction::marker_pattern()
                .find(&safe)
                .unwrap()
                .as_str();
            let mut http = json!({"basic_auth":marker});
            session.restore_tool_input_for_tool("http_request", &mut http);
            assert_eq!(http["basic_auth"], expected, "{source}");
            let mut bash = json!({"command":format!("printf '%s' \"{marker}\"")});
            session.restore_tool_input_for_tool("Bash", &mut bash);
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(bash["command"].as_str().unwrap())
                .env_clear()
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert_eq!(output.stdout, expected.as_bytes());
            let raw_scan = super::scan(source);
            let raw = &source[raw_scan.secrets[0].clone()];
            assert!(!session
                .redact_text(&format!("Earlier literal {raw}"), &[])
                .contains(raw));
        }
    }

    #[test]
    fn credential_headers_preserve_the_following_url_and_options() {
        for scheme in [
            "Basic",
            "Bearer",
            "Digest",
            "NTLM",
            "Negotiate",
            "OAuth",
            "Hawk",
            "Signature",
            "AWS4-HMAC-SHA256",
            "SharedKey",
            "SharedAccessSignature",
            "ApiKey",
            "Token",
            "DPoP",
        ] {
            let credential = if scheme == "Basic" {
                "dXNlcjpvcGFxdWUtYXV0aC1maXh0dXJl"
            } else {
                "opaque-auth-fixture"
            };
            for name in ["Authorization", "Proxy-Authorization"] {
                let source = format!(
                    "curl -H '{name}: {scheme} {credential}' --retry 2 https://example.invalid/"
                );
                round_trip(
                    &source,
                    &[credential],
                    &[scheme, "--retry 2 https://example.invalid/"],
                );
                round_trip(
                    &format!("{name}: {scheme} {credential}\nContent-Type: application/json"),
                    &[credential],
                    &["Content-Type: application/json"],
                );
            }
        }
        round_trip(
            "curl -H 'X-Api-Key: opaque-auth-fixture' https://example.invalid/",
            &["opaque-auth-fixture"],
            &["https://example.invalid/"],
        );
        round_trip(
            "curl -H 'X-Custom: password=opaque-auth-fixture' https://example.invalid/",
            &["opaque-auth-fixture"],
            &["https://example.invalid/"],
        );
        round_trip(
            "curl -b 'session=opaque-cookie-fixture; mode=dark' https://example.invalid/",
            &["opaque-cookie-fixture"],
            &["https://example.invalid/"],
        );
        round_trip(
            "curl -E client.p12:opaque-auth-fixture https://example.invalid/",
            &["opaque-auth-fixture"],
            &["client.p12:", "https://example.invalid/"],
        );
    }

    #[test]
    fn sdk_netrc_encoded_auth_and_connection_strings_are_reversible() {
        for source in [
            "auth = ('public-user', 'opaque-auth-fixture')",
            "requests.get(url, auth=HTTPBasicAuth('public-user', 'opaque-auth-fixture'))",
            "requests.get(url, auth=HTTPDigestAuth(\"public-user\", \"opaque-auth-fixture\"))",
            "aiohttp.BasicAuth('public-user', 'opaque-auth-fixture')",
            "requests_ntlm.HttpNtlmAuth('public-user', 'opaque-auth-fixture')",
            "auth=(\n 'public-user',\n 'opaque-auth-fixture'\n)",
            "HTTPBasicAuth(public_user, 'opaque-auth-fixture') # public-user",
            "auth=(os.environ['USER'], 'opaque-auth-fixture') # public-user",
            "request(url, {auth: {user: 'public-user', pass: 'opaque-auth-fixture'}})",
            "req.SetBasicAuth(user, \"opaque-auth-fixture\") // public-user",
            "request.basic_auth('public-user', 'opaque-auth-fixture')",
            "machine example.invalid login public-user password opaque-auth-fixture\n",
            "machine example.invalid\nlogin public-user\npassword opaque-auth-fixture\n",
            "default login public-user password 'opaque-auth-fixture'\n",
        ] {
            round_trip(source, &["opaque-auth-fixture"], &["public-user"]);
        }
        for source in [
            "_auth=dXNlcjpvcGFxdWUtYXV0aC1maXh0dXJl",
            "{\"auths\":{\"example.invalid\":{\"auth\":\"dXNlcjpvcGFxdWUtYXV0aC1maXh0dXJl\"}},\"credsStore\":\"desktop\"}",
        ] {
            round_trip(source, &["dXNlcjpvcGFxdWUtYXV0aC1maXh0dXJl"], &[]);
        }
        for source in [
            "Server=localhost;Password=opaque-auth-fixture;Database=appdb;Encrypt=true",
            "jdbc:sqlserver://localhost;password=opaque-auth-fixture;database=appdb;encrypt=true",
            "Host=localhost;Password='opaque-auth-fixture';Database=appdb",
            "Driver=sql;Pwd={opaque-auth-fixture};Database=appdb",
            "Driver=sql;Pwd={opaque-auth-fixture}}suffix};Database=appdb",
            "postgres://user:opaque-auth-fixture@localhost/appdb",
            "redis://:opaque-auth-fixture@localhost/0",
            "grant_type=client_credentials&client_secret=opaque-auth-fixture&scope=read",
            "client_secret=opaque-auth-fixture&grant_type=client_credentials",
            "grant_type=authorization_code&code=opaque-auth-fixture&redirect_uri=https://example.invalid/callback",
            "basic_auth=public-user:opaque-auth-fixture",
            "auth=aws:opaque-auth-fixture",
            "auth=public-user:https://opaque-auth-fixture",
            "node_options = { auth: 'public-user:opaque-auth-fixture' }",
            "fetch(url, { headers: [['Authorization', 'Negotiate opaque-auth-fixture']] })",
            r#"{"headers":[["Authorization","Negotiate opaque-auth-fixture"]]}"#,
            r#"{"auth":{"user":"public-user","pass":"opaque-auth-fixture"}}"#,
            r#"{"auth":["public-user","opaque-auth-fixture"]}"#,
        ] {
            round_trip(source, &["opaque-auth-fixture"], &[]);
        }
        assert_eq!(
            redact_text(
                "Server=localhost;Password=fixture;Database=appdb;Encrypt=true",
                &[]
            ),
            "Server=localhost;Password=[REDACTED];Database=appdb;Encrypt=true"
        );
        round_trip(
            "password: 'opaque-fixture''suffix'",
            &["opaque-fixture", "suffix"],
            &[],
        );
    }

    #[test]
    fn public_controls_paths_prompt_options_and_environment_references_survive() {
        for source in [
            "access-control-allow-credentials: true\naccess-control-allow-origin: *",
            "Access-Control-Allow-Credentials: false",
            "has_password=true\nisToken=false\nrequire_credentials: true",
            "axios.get(url, {withCredentials: true, timeout: 1000})",
            "WWW-Authenticate: Basic realm=\"Fake Realm\"",
            "Authorization: Negotiate\nProxy-Authorization: NTLM",
            "WWW-Authenticate: Bearer realm=\"api\", error=\"invalid_token\"",
            "Proxy-Authenticate: Digest realm=\"api\", nonce=\"public-nonce\", algorithm=MD5",
            "curl -H 'Access-Control-Allow-Credentials: true' https://example.invalid/",
            "curl -H 'WWW-Authenticate: Bearer realm=\"api\"' https://example.invalid/",
            "curl --key /run/secrets/client.pem --proxy-key /run/secrets/proxy.pem https://example.invalid/",
            "curl --cookie cookies.txt --cookie-jar result.txt --netrc-file ~/.netrc https://example.invalid/",
            "curl --cert client.p12 https://example.invalid/",
            "curl --negotiate -u : https://example.invalid/",
            "curl --user public-user https://example.invalid/",
            "curl --user \"$USER:$PASSWORD\" https://example.invalid/",
            "curl --user \"${USER}:${PASSWORD}\" https://example.invalid/",
            "curl --user \"$Env:USER:$ENV:PASSWORD\" https://example.invalid/",
            "token=${Env:API_TOKEN}",
            "token=$ENV:API_TOKEN",
            "https://$USER:$PASSWORD@example.invalid/path",
            "redis-cli --pass \"${REDIS_PASSWORD}\" PING",
            "http -a public-user https://example.invalid/",
            "http -A basic --auth public-user https://example.invalid/",
            "mysql -u root -p appdb",
            "mysql --user=root --password appdb",
            "psql --password appdb",
            "pg_dump --no-password appdb",
            "openssl pkey -passin env:PRIVATE_KEY_PASSWORD -in client.pem",
            "openssl pkey -passin file:/run/secrets/passphrase -in client.pem",
            "export GOOGLE_APPLICATION_CREDENTIALS='/run/secrets/adc.json'",
            "credential_file = os.getenv('GOOGLE_APPLICATION_CREDENTIALS', '/run/secrets/adc.json')",
            "sort -u input.txt",
            "curl --url https://example.invalid/ --data '-ufoo:bar'",
            "curl -A '-ufoo:bar' https://example.invalid/",
            "password reset instructions",
            "auth=basic\n_auth=false\ntoken_type=Bearer\ngrant_type=client_credentials",
            "auth=https://login.example.invalid/authorize",
            "code=200\nstate=California\nclient_id=public-client-id",
            "fetch(url, {credentials: 'include', method: 'GET'})",
            "not_auth=dXNlcjpvcGFxdWUtYXV0aC1maXh0dXJl",
            "Use Bearer authentication or Basic authentication; keep the Bearer token in an environment variable.",
        ] {
            assert_eq!(redact_text(source, &[]), source, "false positive in {source:?}");
        }
        for source in [
            r#"{"headers":{"Access-Control-Allow-Credentials":"true"},"hasPassword":true,"require_credentials":false,"auth":"basic","max_tokens":100,"client_id":"public-app"}"#,
            r#"{"credentials":"include","method":"GET","mode":"cors"}"#,
            r#"{"Authorization":"Negotiate $TICKET","proxy_basic_auth":"$USER:$PASSWORD"}"#,
            r#"{"headers":{"WWW-Authenticate":"Bearer realm=\"api\", error=\"invalid_token\""}}"#,
            r#"{"headers":[["WWW-Authenticate","Bearer realm=\"api\""]]}"#,
            r#"{"Authorization":"Negotiate","headers":[["Proxy-Authorization","NTLM"]]}"#,
            r#"{"credentials":{"has_password":false,"withCredentials":true}}"#,
        ] {
            assert_eq!(redact_text(source, &[]), source);
        }
        // Do not exempt a real password merely because its value is weak.
        assert_eq!(
            redact_text("password=true", &[]),
            format!("password={REDACTED}")
        );
        assert_eq!(
            redact_text("credentials=include", &[]),
            format!("credentials={REDACTED}")
        );
        assert_eq!(
            redact_text("password=a&b=c;d=e", &[]),
            format!("password={REDACTED}")
        );
        let source = r#"{"credentials":{"has_password":true,"password":"opaque-nested-secret"}}"#;
        let safe: serde_json::Value = serde_json::from_str(&redact_text(source, &[])).unwrap();
        assert_eq!(safe["credentials"]["has_password"], true);
        assert_eq!(safe["credentials"]["password"], REDACTED);
        round_trip(
            "curl -u \"$USER:opaque-auth-fixture\" https://example.invalid/",
            &["opaque-auth-fixture"],
            &["$USER"],
        );
        round_trip(
            "curl -u \"$USER:${PASSWORD:-opaque-auth-fixture}\" https://example.invalid/",
            &["opaque-auth-fixture"],
            &["$USER", "${PASSWORD:-"],
        );
    }

    #[test]
    fn malformed_and_unicode_auth_examples_never_panic_and_round_trip() {
        for fragment in [
            "",
            "'",
            "\"",
            "$'",
            "$''",
            "💡",
            "中文:口令",
            "user:'pass",
            "\\",
            "$(not-executed)",
            "user:${PASSWORD}",
            "user:\"pa'ss\"",
        ] {
            for prefix in [
                "curl -u ",
                "curl --user=",
                "curl -H ",
                "mysql -p",
                "sshpass -f\ncurl -u ",
            ] {
                let source = format!("{prefix}{fragment}");
                let session = RedactionSession::new();
                let safe = session.redact_text(&source, &[]);
                assert_eq!(session.restore_text(&safe), source);
            }
        }
        round_trip(
            "sshpass -f\ncurl -u user:opaque-fixture https://example.invalid/",
            &["opaque-fixture"],
            &["https://example.invalid/"],
        );
    }

    #[test]
    fn large_harmless_command_examples_keep_their_contents() {
        let source = format!("curl --help\n{}", "normal word ".repeat(100_000));
        assert_eq!(redact_text(&source, &[]), source);
    }

    #[test]
    fn environment_default_literals_are_private_but_reference_syntax_survives() {
        for expression in [
            "process.env.API_TOKEN || 'opaque-default-secret'",
            "process.env['API_TOKEN'] ?? \"opaque-default-secret\"",
            "os.getenv('API_TOKEN', 'opaque-default-secret')",
            "os.environ.get('PASS', 'opaque-default-secret')",
            "process.env.APP_SETTING || 'opaque-default-secret'",
            "os.getenv('APP_SETTING', 'opaque-default-secret')",
        ] {
            round_trip(
                &format!("token = {expression}"),
                &["opaque-default-secret"],
                &[expression.split(['|', '?', ',']).next().unwrap()],
            );
            let source = json!({"token":expression}).to_string();
            round_trip(&source, &["opaque-default-secret"], &[]);
        }
        for source in [
            "token=$Env:API_TOKEN",
            "token=${ENV:API_TOKEN}",
            "token=os.getenv('API_TOKEN')",
            "port=os.getenv('PORT', '8080')",
        ] {
            assert_eq!(redact_text(source, &[]), source);
        }
        for expression in [
            "process.env.API_TOKEN || 'opaque$PART'",
            "os.getenv('API_TOKEN', '$TOKEN')",
        ] {
            for source in [
                format!("token={expression}"),
                json!({"token":expression}).to_string(),
            ] {
                let session = RedactionSession::new();
                let safe = session.redact_text(&source, &[]);
                assert_ne!(safe, source);
                assert!(!safe.contains("$PART"));
                assert!(!safe.contains("$TOKEN"));
                assert_eq!(session.redact_text(&safe, &[]), safe);
                assert_eq!(session.restore_text(&safe), source);
            }
        }
    }

    #[test]
    fn explicit_non_posix_commands_keep_source_spelling() {
        let command = r#"curl -u "user:pa\\ss" https://example.invalid/"#;
        let mut payload: crate::KiroPayload = serde_json::from_value(json!({
            "conversationState":{
                "chatTriggerType":"MANUAL","conversationId":"non-posix-auth",
                "currentMessage":{"userInputMessage":{"content":"continue","modelId":"model","origin":"AI_EDITOR"}},
                "history":[{"assistantResponseMessage":{"content":"","toolUses":[{"toolUseId":"auth","name":"exec_command","input":{"shell":"pwsh","command":command}}]}}]
            }
        })).unwrap();
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        let input = &payload.conversation_state.history[0]
            .assistant_response_message
            .as_ref()
            .unwrap()
            .tool_uses[0]
            .input;
        let marker = crate::redaction::marker_pattern()
            .find(input["command"].as_str().unwrap())
            .unwrap()
            .as_str();
        let mut literal = json!({"auth":marker});
        session.restore_tool_input(&mut literal);
        assert_eq!(literal["auth"], r"user:pa\\ss");
        let mut replay = input.clone();
        session.restore_tool_input_for_tool("exec_command", &mut replay);
        assert_eq!(replay, json!({"shell":"pwsh","command":command}));
    }

    #[test]
    fn auth_schema_examples_are_filtered_without_changing_public_modes_or_controls() {
        let request: crate::ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4","max_tokens":128,
            "tools":[{"name":"request","input_schema":{"type":"object","properties":{
                "auth":{"type":"string","enum":["basic","digest","dXNlcjpvcGFxdWUtc2NoZW1hLWZpeHR1cmU="]},
                "basic_auth":{"type":"array","default":["public-user","opaque-schema-password"],"prefixItems":[{"type":"string","default":"public-user"},{"type":"string","default":"opaque-tuple-default"}]},
                "proxyAuth":{"type":"object","properties":{"user":{"type":"string","default":"public-user"},"pass":{"type":"string","default":"opaque-proxy-default"}},"examples":[{"user":"public-user","pass":"opaque-proxy-password"}]},
                "withCredentials":{"type":"boolean","default":true,"enum":[true,false]},
                "has_password":{"type":"boolean","default":false},
                "WWW-Authenticate":{"type":"string","default":"Bearer realm=\"api\""}
            }}}],
            "messages":[{"role":"user","content":"inspect example credentials"}]
        })).unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
        );
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        let wire = serde_json::to_value(&payload).unwrap();
        let schema = &wire["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["tools"][0]["toolSpecification"]["inputSchema"]["json"];
        let encoded = schema.to_string();
        for secret in [
            "dXNlcjpvcGFxdWUtc2NoZW1hLWZpeHR1cmU=",
            "opaque-schema-password",
            "opaque-proxy-password",
            "opaque-proxy-default",
            "opaque-tuple-default",
        ] {
            assert!(!encoded.contains(secret));
        }
        assert!(encoded.contains("public-user"));
        let properties = &schema["properties"];
        assert_eq!(
            properties["withCredentials"],
            json!({"type":"boolean","default":true,"enum":[true,false]})
        );
        assert_eq!(
            properties["has_password"],
            json!({"type":"boolean","default":false})
        );
        assert_eq!(
            properties["WWW-Authenticate"],
            json!({"type":"string","default":"Bearer realm=\"api\""})
        );
        assert!(encoded.contains("basic"));
        assert!(encoded.contains("digest"));
        let mut auth = json!({"auth":properties["basic_auth"]["default"]});
        session.restore_tool_input(&mut auth);
        assert_eq!(
            auth["auth"],
            json!(["public-user", "opaque-schema-password"])
        );
    }
}
