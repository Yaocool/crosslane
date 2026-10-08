//! Shared Kiro client identities for upstream requests and token refresh.
//!
//! Verified against official stable releases on 2026-09-14. Keep application,
//! SDK and generated service versions together when updating this baseline.
//! See docs/protocol-compatibility.md for sources and the 2026-11-09 cutoff.
//! These are the existing desktop/CLI platform profiles, not the proxy host OS.

pub const IDE_VERSION: &str = "1.0.437";
pub const CLI_VERSION: &str = "2.21.4";

const IDE_SDK_VERSION: &str = "1.0.39";
const IDE_NODE_VERSION: &str = "22.21.1";
const CLI_SDK_VERSION: &str = "1.3.15";
const CLI_API_VERSION: &str = "0.1.17975";
const CLI_RUST_VERSION: &str = "1.92.0";

/// The V2 CLI uses different generated clients for generation and management.
#[derive(Clone, Copy)]
pub enum CliService {
    Streaming,
    Management,
}

impl CliService {
    fn api_name(self) -> &'static str {
        match self {
            Self::Streaming => "codewhispererstreaming",
            Self::Management => "codewhispererruntime",
        }
    }
}

pub fn ide_auth_user_agent(machine_id: &str) -> String {
    format!("KiroIDE-{IDE_VERSION}-{machine_id}")
}

pub fn ide_user_agent(machine_id: &str) -> String {
    let app = ide_auth_user_agent(machine_id);
    format!(
        "aws-sdk-js/{IDE_SDK_VERSION} ua/2.1 os/win32#10.0.19044 lang/js md/nodejs#{IDE_NODE_VERSION} api/codewhispererstreaming#{IDE_SDK_VERSION} m/E {app}"
    )
}

pub fn ide_amz_user_agent(machine_id: &str) -> String {
    let app = ide_auth_user_agent(machine_id);
    format!("aws-sdk-js/{IDE_SDK_VERSION} {app}")
}

pub fn cli_user_agent(service: CliService) -> String {
    let api = service.api_name();
    format!(
        "aws-sdk-rust/{CLI_SDK_VERSION} ua/2.1 api/{api}/{CLI_API_VERSION} os/macos lang/rust/{CLI_RUST_VERSION} md/appVersion-{CLI_VERSION} app/AmazonQ-For-CLI"
    )
}

pub fn cli_amz_user_agent(service: CliService) -> String {
    let api = service.api_name();
    format!(
        "aws-sdk-rust/{CLI_SDK_VERSION} ua/2.1 api/{api}/{CLI_API_VERSION} os/macos lang/rust/{CLI_RUST_VERSION} m/F app/AmazonQ-For-CLI"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_parts(version: &str) -> [u32; 3] {
        version
            .split('.')
            .map(|part| part.parse::<u32>().expect("numeric version component"))
            .collect::<Vec<_>>()
            .try_into()
            .expect("three version components")
    }

    #[test]
    fn client_versions_meet_the_2026_11_09_support_minimums() {
        // AWS Health notice: earlier versions cannot connect after this date.
        // Compare numeric components: e.g. CLI 2.10.0 is newer than 1.28.2.
        assert!(version_parts(IDE_VERSION) >= version_parts("0.11.133"));
        assert!(version_parts(CLI_VERSION) >= version_parts("1.28.2"));
    }
}
