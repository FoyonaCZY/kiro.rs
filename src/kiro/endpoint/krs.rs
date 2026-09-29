//! Kiro Runtime Service 端点。
//!
//! Amazon Q 的 `q.{region}.amazonaws.com/generateAssistantResponse` 会在历史
//! 大约 200 条时返回 `CONTENT_LENGTH_EXCEEDS_THRESHOLD`，同一份正文发到
//! `runtime.us-east-1.kiro.dev` 可以被接受。KRS 不接受 `ksk_` API Key。
//!
//! MCP 仍走原来的 Q 地址。这次只验证了对话接口。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

/// KRS 端点名称。
pub const KRS_ENDPOINT_NAME: &str = "krs";

const KRS_HOST: &str = "runtime.us-east-1.kiro.dev";

/// Kiro Runtime Service 端点。
pub struct KrsEndpoint;

impl KrsEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!("KiroIDE {} {}", ctx.config.kiro_version, ctx.machine_id)
    }

    fn amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/{} KiroIDE-{}-{}",
            crate::kiro::identity::SDK_VERSION,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }
}

impl Default for KrsEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for KrsEndpoint {
    fn name(&self) -> &'static str {
        KRS_ENDPOINT_NAME
    }

    fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
        format!("https://{KRS_HOST}/generateAssistantResponse")
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://q.{}.amazonaws.com/mcp",
            ctx.credentials.effective_api_region(ctx.config)
        )
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("Accept", "*/*")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", &ctx.config.agent_mode)
            .header("x-amz-user-agent", self.amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", KRS_HOST)
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));
        if let Some(arn) = ctx.credentials.profile_arn.as_deref() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        apply_token_type(req, ctx)
    }

    fn api_log_headers(&self, ctx: &RequestContext<'_>) -> Vec<(String, String)> {
        let mut headers = vec![
            ("content-type".into(), "application/json".into()),
            ("Accept".into(), "*/*".into()),
            ("Connection".into(), "close".into()),
            ("x-amzn-codewhisperer-optout".into(), "true".into()),
            (
                "x-amzn-kiro-agent-mode".into(),
                ctx.config.agent_mode.clone(),
            ),
            ("x-amz-user-agent".into(), self.amz_user_agent(ctx)),
            ("user-agent".into(), self.user_agent(ctx)),
            ("host".into(), KRS_HOST.into()),
            ("amz-sdk-request".into(), "attempt=1; max=3".into()),
            ("Authorization".into(), "Bearer ***".into()),
        ];
        if let Some(arn) = ctx.credentials.profile_arn.as_deref() {
            headers.push(("x-amzn-kiro-profile-arn".into(), arn.to_string()));
        }
        if let Some(token_type) = ctx.credentials.upstream_token_type() {
            headers.push(("TokenType".into(), token_type.to_string()));
        }
        headers
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let region = ctx.credentials.effective_api_region(ctx.config);
        let mut req = req
            .header("x-amz-user-agent", self.amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", format!("q.{region}.amazonaws.com"))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));
        if let Some(arn) = ctx.credentials.profile_arn.as_deref() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        apply_token_type(req, ctx)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        inject_profile_arn(body, &ctx.credentials.profile_arn)
    }
}

fn apply_token_type(req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
    if let Some(token_type) = ctx.credentials.upstream_token_type() {
        req.header("TokenType", token_type)
    } else {
        req
    }
}

fn inject_profile_arn(request_body: &str, profile_arn: &Option<String>) -> String {
    if let Some(arn) = profile_arn {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
            json["profileArn"] = serde_json::Value::String(arn.clone());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    fn context<'a>(
        credentials: &'a KiroCredentials,
        config: &'a Config,
        token: &'a str,
    ) -> RequestContext<'a> {
        RequestContext {
            credentials,
            token,
            machine_id: "abc123",
            config,
        }
    }

    #[test]
    fn krs_api_url_is_fixed_runtime_host() {
        let endpoint = KrsEndpoint::new();
        let credentials = KiroCredentials::default();
        let config = Config::default();
        let ctx = context(&credentials, &config, "token");
        assert_eq!(
            endpoint.api_url(&ctx),
            "https://runtime.us-east-1.kiro.dev/generateAssistantResponse"
        );
        assert!(endpoint.mcp_url(&ctx).contains("amazonaws.com/mcp"));
    }

    #[test]
    fn krs_log_headers_use_short_ide_identity() {
        let endpoint = KrsEndpoint::new();
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".into());
        let config = Config::default();
        let ctx = context(&credentials, &config, "secret-token");
        let headers = endpoint.api_log_headers(&ctx);
        let user_agent = headers
            .iter()
            .find(|(name, _)| name == "user-agent")
            .map(|(_, value)| value.as_str());
        assert_eq!(user_agent, Some("KiroIDE 1.1.70 abc123"));
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "host" && value == "runtime.us-east-1.kiro.dev")
        );
        assert!(headers.iter().any(|(name, value)| {
            name == "x-amzn-kiro-profile-arn" && value.ends_with("profile/ABC")
        }));
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer ***")
        );
        assert!(
            !headers
                .iter()
                .any(|(_, value)| value.contains("secret-token"))
        );
    }
}
