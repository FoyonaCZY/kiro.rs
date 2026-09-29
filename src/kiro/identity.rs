//! 出站客户端标识。
//!
//! 对齐 Kiro IDE 1.1.70 的 HTTP User-Agent：`os/` 只带平台和版本，不带 CPU 架构。
//! 对话、MCP、额度和 IdC 刷新走 AWS SDK 3 的拼接，末尾是空格分隔的 `KiroIDE <version> <machineId>`。
//! Social 刷新仍是官方 auth client 的连字符形式。

use crate::model::config::Config;

/// 安装包里 `@aws-sdk/nested-clients` 的版本。
pub const SDK_VERSION: &str = "3.997.37";

pub fn sdk_user_agent(config: &Config, machine_id: &str, api_name: &str) -> String {
    format!(
        "aws-sdk-js/{sdk} ua/2.1 os/{os} lang/js md/nodejs#{node} api/{api}#{sdk} m/E KiroIDE {ver} {mid}",
        sdk = SDK_VERSION,
        os = config.system_version,
        node = config.node_version,
        api = api_name,
        ver = config.kiro_version,
        mid = machine_id,
    )
}

pub fn social_refresh_user_agent(config: &Config, machine_id: &str) -> String {
    format!("KiroIDE-{}-{}", config.kiro_version, machine_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_user_agent_has_os_but_not_cpu_arch() {
        let config = Config::default();
        let ua = sdk_user_agent(&config, &"ab".repeat(32), "codewhispererstreaming");
        assert!(ua.contains("os/win32#10.0.26200"));
        assert!(ua.contains("lang/js"));
        assert!(ua.contains("md/nodejs#24.15.0"));
        assert!(ua.contains("api/codewhispererstreaming#3.997.37"));
        assert!(ua.contains("KiroIDE 1.1.70 "));
        assert!(!ua.contains("KiroIDE-1.1.70-"));
        assert!(!ua.contains("x64"));
        assert!(!ua.contains("arm64"));
        assert!(!ua.contains("aarch64"));
    }

    #[test]
    fn social_refresh_user_agent_is_hyphenated_and_has_no_os() {
        let config = Config::default();
        let ua = social_refresh_user_agent(&config, "abc");
        assert_eq!(ua, "KiroIDE-1.1.70-abc");
        assert!(!ua.contains("os/"));
    }
}
