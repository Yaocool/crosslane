//! Normalize only supported Identity Store references, never arbitrary ARN
//! resource tails. Kiro assignments always receive the canonical bare UserId.

pub(super) fn user_id(value: &str) -> Option<String> {
    resource_id(value, "user")
}

pub(super) fn group_reference(value: &str) -> Option<String> {
    resource_id(value, "group").or_else(|| {
        // Preserve opaque legacy group identifiers, but do not reinterpret a
        // foreign ARN or a user ARN as a group in the selected directory.
        (!value.is_empty() && !value.starts_with("arn:")).then(|| value.to_owned())
    })
}

fn resource_id(value: &str, kind: &str) -> Option<String> {
    let value = if value.starts_with("arn:") {
        value.strip_prefix(&format!("arn:aws:identitystore:::{kind}/"))?
    } else {
        value
    };
    if let Some(id) = uuid_id(value) {
        return Some(id);
    }
    if value.len() == 47
        && value.as_bytes()[10] == b'-'
        && value.as_bytes()[..10].iter().all(u8::is_ascii_hexdigit)
    {
        return Some(format!(
            "{}-{}",
            value[..10].to_ascii_lowercase(),
            uuid_id(&value[11..])?
        ));
    }
    None
}

pub(super) fn store_id(value: &str) -> Option<String> {
    let value = if value.starts_with("arn:") {
        let rest = value.strip_prefix("arn:aws:identitystore::")?;
        let (account, resource) = rest.split_once(':')?;
        if account.len() != 12 || !account.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        resource.strip_prefix("identitystore/")?
    } else {
        value
    };
    if value.len() == 12
        && value.starts_with("d-")
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
    {
        return Some(value.to_ascii_lowercase());
    }
    uuid_id(value)
}

pub(super) fn store_account(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("arn:aws:identitystore::")?;
    let (account, _) = rest.split_once(':')?;
    (account.len() == 12 && account.bytes().all(|byte| byte.is_ascii_digit())).then_some(account)
}

fn uuid_id(value: &str) -> Option<String> {
    if value.len() != 36
        || ![8, 13, 18, 23]
            .iter()
            .all(|index| value.as_bytes()[*index] == b'-')
    {
        return None;
    }
    Some(uuid::Uuid::parse_str(value).ok()?.hyphenated().to_string())
}
