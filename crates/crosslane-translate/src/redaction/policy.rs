//! Narrow public spellings, never a blanket exemption for example documents.

use std::sync::OnceLock;

use regex::Regex;

pub(super) fn ordinary_field_name(compact: &str) -> bool {
    matches!(
        compact,
        "key"
            | "cachekey"
            | "sortkey"
            | "primarykey"
            | "foreignkey"
            | "partitionkey"
            | "rowkey"
            | "objectkey"
            | "mapkey"
            | "hotkey"
            | "shortcutkey"
            | "lookupkey"
            | "displaykey"
            | "fieldkey"
            | "indexkey"
            | "itemkey"
            | "publickey"
            | "pagetoken"
            | "nextpagetoken"
            | "previouspagetoken"
            | "nexttoken"
            | "nextcontinuationtoken"
            | "continuationtoken"
    )
}

fn compact(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

pub(super) fn bearer_prose(value: &str) -> bool {
    matches!(
        value
            .trim_end_matches(['.', ',', ':', ';', '!', '?'])
            .to_ascii_lowercase()
            .as_str(),
        "token"
            | "tokens"
            | "token-based"
            | "authentication"
            | "authorization"
            | "auth"
            | "scheme"
            | "schemes"
            | "header"
            | "headers"
            | "credential"
            | "credentials"
    )
}

pub(super) fn bearer_prose_assignment(name: &str, prefix: &str) -> bool {
    matches!(
        compact(name).as_str(),
        "authentication" | "authorization" | "scheme" | "header"
    ) && prefix
        .split_whitespace()
        .next_back()
        .is_some_and(|word| word.eq_ignore_ascii_case("bearer"))
}

/// Only explicit key-file spellings denote a local reference. Opaque keys can
/// start with `/`, so a path prefix alone cannot exempt a credential value.
pub(super) fn public_value(name: &str, value: &str) -> bool {
    let name = compact(name);
    let value = value.trim();
    if matches!(
        name.as_str(),
        "key" | "privatekey" | "sshkey" | "sslkey" | "tlskey"
    ) {
        let windows = value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && matches!(value.as_bytes().get(2), Some(b'\\' | b'/'));
        let path = windows
            || ["/", "./", "../", "~/", "\\\\"]
                .iter()
                .any(|prefix| value.starts_with(prefix));
        let rest = if windows { &value[2..] } else { value };
        let basename = value.rsplit(['/', '\\']).next().unwrap_or(value);
        let lower = basename.to_ascii_lowercase();
        let key_file = matches!(basename, "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519")
            || [".key", ".pem", ".p12", ".pfx", ".pkcs8", ".der", ".jks"]
                .iter()
                .any(|extension| lower.ends_with(extension));
        if path && key_file && !value.contains(['\r', '\n']) && !rest.contains(':') {
            return true;
        }
    }
    if matches!(name.as_str(), "signature" | "sig") {
        static FUNCTION: OnceLock<Regex> = OnceLock::new();
        return super::regex(&FUNCTION, r"\A(?:(?:pub\s+)?(?:async\s+)?fn\s+[A-Za-z_][A-Za-z0-9_]*\s*\(|(?:\([^\r\n)]*\)|[A-Za-z_][A-Za-z0-9_]*)\s*=>)").is_match(value);
    }
    false
}

/// Recognize annotations only inside a declaration/parameter list. A literal
/// `password=string`, JSON password, or initializer remains credential data.
pub(super) enum TypeAnnotation {
    Declaration,
    Initializer(usize),
}

pub(super) fn type_annotation(
    text: &str,
    name_start: usize,
    value_start: usize,
) -> Option<TypeAnnotation> {
    if !text[name_start..value_start].trim_end().ends_with(':') {
        return None;
    }
    static TYPE: OnceLock<Regex> = OnceLock::new();
    let tail = &text[value_start..];
    let captures = super::regex(&TYPE, r"\A(?:&(?:'[A-Za-z_][A-Za-z0-9_]*\s+)?(?:mut\s+)?\s*)?[A-Za-z_][A-Za-z0-9_.]*(?:[<\[][A-Za-z0-9_, <>\[\]&.'|]+[>\]])?(?:\[\])?(?:[ \t]*\|[ \t]*(?:null|None|undefined))?[ \t]*(?P<end>[=,;}\r\n)]|\z)").captures(tail)?;
    let prefix = &text[..name_start];
    let start = prefix
        .char_indices()
        .rev()
        .nth(2048)
        .map_or(0, |(offset, _)| offset);
    let prefix = &prefix[start..];
    static DECLARATION: OnceLock<Regex> = OnceLock::new();
    static PARAMETERS: OnceLock<Regex> = OnceLock::new();
    let declaration = super::regex(&DECLARATION, r"(?s)\b(?:(?:interface|struct|class)\s+[A-Za-z_][A-Za-z0-9_]*[^{};]*|type\s+[A-Za-z_][A-Za-z0-9_]*(?:<[^>]*>)?\s*=\s*)\{[^{}]*\z").is_match(prefix)
        || super::regex(&PARAMETERS, r"(?s)\b(?:def|fn|function)\s+[A-Za-z_][A-Za-z0-9_]*(?:<[^>]*>)?\s*\([^(){}]*\z").is_match(prefix);
    if !declaration {
        return None;
    }
    let end = captures.name("end")?;
    if end.as_str() == "=" {
        let start = value_start + end.end();
        return Some(TypeAnnotation::Initializer(
            start + text[start..].len() - text[start..].trim_start_matches([' ', '\t']).len(),
        ));
    }
    Some(TypeAnnotation::Declaration)
}

/// Only complete, unmistakable placeholders qualify. Do not exempt arbitrary
/// values containing `example`, `test` or `xxxxx`, or weak literal passwords.
/// Explicitly known credentials are matched independently and take precedence.
pub(super) fn credential_placeholder(text: &str) -> bool {
    let mut text = text.trim();
    if text.len() > 256 {
        return false;
    }
    if let Some((scheme, value)) = text.split_once(' ') {
        if matches!(
            scheme.to_ascii_lowercase().as_str(),
            "bearer" | "basic" | "apikey" | "token"
        ) {
            text = value.trim_start();
        }
    }
    if named_placeholder(text) {
        return true;
    }
    if text
        .split_once(':')
        .is_some_and(|(user, password)| named_placeholder(user) && named_placeholder(password))
    {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    [
        "sk-proj-",
        "sk-ant-api03-",
        "sk-ant-",
        "sk-svcacct-",
        "sk-",
        "ksk_",
        "sk_live_",
        "sk_test_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "aiza",
        "akia",
        "asia",
        "akid",
        "ltai",
        "aklt",
    ]
    .iter()
    .any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.len() >= 3
                && (suffix.bytes().all(|byte| matches!(byte, b'x' | b'X'))
                    || suffix.bytes().all(|byte| byte == b'*')
                    || suffix.bytes().all(|byte| byte == b'.')
                    || suffix.chars().all(|ch| ch == '…'))
        })
    })
}

fn named_placeholder(text: &str) -> bool {
    let mut inner = text;
    let mut wrapped = false;
    for (open, close) in [("<", ">"), ("{{", "}}"), ("[", "]")] {
        if let Some(value) = text
            .strip_prefix(open)
            .and_then(|value| value.strip_suffix(close))
        {
            inner = value;
            wrapped = true;
            break;
        }
    }
    if !inner
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b' '))
    {
        return false;
    }
    let name = compact(inner);
    let mut name = name.as_str();
    let mut instructed = false;
    for prefix in ["replacewithyour", "insertyour", "putyour", "your"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest;
            instructed = true;
            break;
        }
    }
    if !(wrapped || instructed) {
        return false;
    }
    let name = name.strip_suffix("here").unwrap_or(name);
    matches!(
        name,
        "redacted"
            | "username"
            | "password"
            | "passphrase"
            | "key"
            | "token"
            | "secret"
            | "credentials"
    ) || [
        "apikey",
        "password",
        "passphrase",
        "secret",
        "accesskey",
        "secretkey",
        "privatekey",
        "accesstoken",
        "refreshtoken",
        "sessiontoken",
        "authtoken",
        "connectionstring",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}
