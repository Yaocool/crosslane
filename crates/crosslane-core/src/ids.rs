//! 账号 ID 与设备标识生成。

use rand::RngCore;

/// 生成账号 ID，格式 `acc_<8位小写hex>`。
pub fn new_account_id() -> String {
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("acc_{}", hex_encode(&bytes))
}

/// 生成设备标识，64 位小写 hex。
///
/// 仅用于构造上游 User-Agent，不读写本机任何文件。
pub fn new_machine_id() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_has_prefix_and_eight_hex_chars() {
        let id = new_account_id();
        assert!(id.starts_with("acc_"), "got {id}");
        let hex = &id[4..];
        assert_eq!(hex.len(), 8, "got {id}");
        assert!(hex
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_uppercase()));
    }

    #[test]
    fn machine_id_is_sixty_four_lowercase_hex_chars() {
        let id = new_machine_id();
        assert_eq!(id.len(), 64);
        assert!(id
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_uppercase()));
    }

    #[test]
    fn generated_ids_are_unique_across_calls() {
        let accounts: std::collections::HashSet<_> = (0..100).map(|_| new_account_id()).collect();
        assert_eq!(accounts.len(), 100);
        let machines: std::collections::HashSet<_> = (0..100).map(|_| new_machine_id()).collect();
        assert_eq!(machines.len(), 100);
    }
}
