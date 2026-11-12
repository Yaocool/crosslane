//! Shared Kiro client identities for upstream requests and token refresh.
//!
//! Application versions come from upstream configuration. SDK, service, and
//! platform metadata describe the desktop and CLI protocols used by Crosslane.

pub use crosslane_core::config::{
    DEFAULT_KIRO_CLI_VERSION as CLI_VERSION, DEFAULT_KIRO_IDE_VERSION as IDE_VERSION,
};

pub const KAS_VERSION: &str = "0.66.32";

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

pub fn ide_auth_user_agent(machine_id: &str, ide_version: &str) -> String {
    format!("KiroIDE-{ide_version}-{machine_id}")
}

/// Agent RPC clients and extension REST clients have distinct identities.
/// The hyphenated desktop identity belongs only to social authentication.
#[derive(Clone, Copy)]
pub enum IdeService {
    Runtime,
    ControlPlane,
    Management,
    LegacyStreaming,
}

impl IdeService {
    fn api_name(self) -> &'static str {
        match self {
            Self::Runtime => "kiro-runtime",
            Self::ControlPlane => "kiro-controlplane-bearer",
            Self::Management => "codewhisperer",
            Self::LegacyStreaming => "codewhispererstreaming",
        }
    }

    fn custom_user_agent(self, machine_id: &str, ide_version: &str) -> String {
        let app = format!("KiroIDE {ide_version} {machine_id}");
        if matches!(self, Self::Runtime | Self::ControlPlane) {
            format!("{app} KAS/{KAS_VERSION}")
        } else {
            app
        }
    }
}

pub fn ide_service_user_agent(service: IdeService, machine_id: &str, ide_version: &str) -> String {
    let app = service.custom_user_agent(machine_id, ide_version);
    let api = service.api_name();
    format!(
        "aws-sdk-js/{IDE_SDK_VERSION} ua/2.1 os/win32#10.0.19044 lang/js md/nodejs#{IDE_NODE_VERSION} api/{api}#{IDE_SDK_VERSION} m/E {app}"
    )
}

pub fn ide_service_amz_user_agent(
    service: IdeService,
    machine_id: &str,
    ide_version: &str,
) -> String {
    let app = service.custom_user_agent(machine_id, ide_version);
    format!("aws-sdk-js/{IDE_SDK_VERSION} {app}")
}

pub fn cli_user_agent(service: CliService, cli_version: &str) -> String {
    let api = service.api_name();
    format!(
        "aws-sdk-rust/{CLI_SDK_VERSION} ua/2.1 api/{api}/{CLI_API_VERSION} os/macos lang/rust/{CLI_RUST_VERSION} md/appVersion-{cli_version} app/AmazonQ-For-CLI"
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
