//! Social（GitHub / Google）登录。
//!
//! 对齐 Kiro IDE 1.1.70：生成 `https://app.kiro.dev/signin` 链接，浏览器回调到
//! `http://localhost:3128`。用户把地址栏里的回调 URL 贴回来，服务端用同一条 SOCKS5
//! 向 `prod.us-east-1.auth.desktop.kiro.dev/oauth/token` 换 refresh token。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::identity::social_refresh_user_agent;
use crate::kiro::machine_id;
use crate::model::config::Config;

pub const AUTH_PORTAL: &str = "https://app.kiro.dev";
pub const TOKEN_ENDPOINT: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token";
pub const REDIRECT_ORIGIN: &str = "http://localhost:3128";
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct PendingSocialLogin {
    pub code_verifier: String,
    pub proxy_url: String,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
    pub machine_id: String,
    created_at: Instant,
}

pub struct SocialLoginStore {
    pending: Mutex<HashMap<String, PendingSocialLogin>>,
}

impl Default for SocialLoginStore {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl SocialLoginStore {
    pub fn start(
        &self,
        proxy_url: &str,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<StartedLogin, String> {
        validate_socks_proxy(proxy_url)?;
        let state = uuid::Uuid::new_v4().to_string();
        let code_verifier = base64url(&random_bytes(32));
        let code_challenge = base64url(&Sha256::digest(code_verifier.as_bytes()));
        let machine_id = machine_id::random_machine_id();
        self.pending.lock().insert(
            state.clone(),
            PendingSocialLogin {
                code_verifier,
                proxy_url: proxy_url.to_string(),
                proxy_username: username,
                proxy_password: password,
                machine_id,
                created_at: Instant::now(),
            },
        );
        self.purge_expired();
        let authorization_url = portal_url(&state, &code_challenge);
        Ok(StartedLogin {
            state,
            authorization_url,
        })
    }

    pub fn take(&self, state: &str) -> Result<PendingSocialLogin, String> {
        self.purge_expired();
        self.pending
            .lock()
            .remove(state)
            .ok_or_else(|| "登录会话不存在或已过期，请重新生成授权链接".to_string())
    }

    pub fn restore(&self, state: String, pending: PendingSocialLogin) {
        self.pending.lock().insert(state, pending);
    }

    fn purge_expired(&self) {
        self.pending
            .lock()
            .retain(|_, pending| pending.created_at.elapsed() < LOGIN_TTL);
    }
}

pub struct StartedLogin {
    pub state: String,
    pub authorization_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCallback {
    pub code: String,
    pub state: String,
    pub login_option: String,
    pub path: String,
    pub port: u16,
}

pub fn portal_url(state: &str, code_challenge: &str) -> String {
    format!(
        "{AUTH_PORTAL}/signin?state={}&code_challenge={}&code_challenge_method=S256&redirect_uri={}&redirect_from=KiroIDE",
        encode_query(state),
        encode_query(code_challenge),
        encode_query(REDIRECT_ORIGIN),
    )
}

fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn parse_callback(raw: &str) -> Result<ParsedCallback, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| "回调不是合法 URL".to_string())?;
    if url.scheme() != "http" || url.host_str() != Some("localhost") {
        return Err("回调必须是 http://localhost 开头".to_string());
    }
    let port = url.port().unwrap_or(80);
    if port != 3128 {
        return Err("回调端口必须是 3128".to_string());
    }
    let path = url.path().to_string();
    if path != "/oauth/callback" && path != "/signin/callback" {
        return Err("回调路径必须是 /oauth/callback 或 /signin/callback".to_string());
    }
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if let Some(error) = query.get("error") {
        let detail = query
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        return Err(format!("授权被拒绝: {detail}"));
    }
    let code = query
        .get("code")
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "回调里没有 code".to_string())?;
    let state = query
        .get("state")
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "回调里没有 state".to_string())?;
    let login_option = query
        .get("login_option")
        .cloned()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if login_option != "github" && login_option != "google" {
        return Err("只支持 GitHub 或 Google 登录回调".to_string());
    }
    Ok(ParsedCallback {
        code,
        state,
        login_option,
        path,
        port,
    })
}

pub fn exchange_redirect_uri(callback: &ParsedCallback) -> String {
    format!(
        "http://localhost:{}{}?login_option={}",
        callback.port, callback.path, callback.login_option
    )
}

#[derive(Debug, Deserialize)]
pub struct SocialTokenResponse {
    #[serde(rename = "accessToken", alias = "access_token")]
    pub access_token: String,
    #[serde(rename = "refreshToken", alias = "refresh_token")]
    pub refresh_token: String,
    #[serde(rename = "profileArn", alias = "profile_arn", default)]
    pub profile_arn: Option<String>,
    #[serde(rename = "expiresIn", alias = "expires_in")]
    pub expires_in: i64,
}

pub async fn exchange_social_code(
    config: &Config,
    pending: &PendingSocialLogin,
    callback: &ParsedCallback,
) -> anyhow::Result<SocialTokenResponse> {
    let mut proxy = ProxyConfig::new(&pending.proxy_url);
    if let (Some(username), Some(password)) = (&pending.proxy_username, &pending.proxy_password) {
        proxy = proxy.with_auth(username, password);
    }
    let client = build_client(Some(&proxy), 60, config.tls_backend)?;
    let user_agent = social_refresh_user_agent(config, &pending.machine_id);
    let response = client
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent)
        .json(&serde_json::json!({
            "code": callback.code,
            "code_verifier": pending.code_verifier,
            "redirect_uri": exchange_redirect_uri(callback),
        }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("换取 token 失败: HTTP {status} {}", redact_body(&body));
    }
    Ok(response.json().await?)
}

pub fn validate_socks_proxy(url: &str) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("socks5://") || lower.starts_with("socks5h://")) {
        return Err("代理必须是 socks5:// 或 socks5h://".to_string());
    }
    ProxyConfig::new(url.trim());
    if reqwest::Proxy::all(url.trim()).is_err() {
        return Err("代理 URL 无法解析".to_string());
    }
    Ok(())
}

fn random_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|_| fastrand::u8(..)).collect()
}

fn base64url(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    if i < data.len() {
        let mut n = (data[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        if i + 1 < data.len() {
            n |= (data[i + 1] as u32) << 8;
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
        }
    }
    out
}

fn redact_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > 180 {
        format!("{}…", &trimmed[..180])
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_url_uses_s256_and_localhost_redirect() {
        let url = portal_url("state-1", "challenge");
        assert!(url.starts_with("https://app.kiro.dev/signin?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A3128"));
        assert!(url.contains("redirect_from=KiroIDE"));
        assert!(url.contains("state=state-1"));
    }

    #[test]
    fn parse_github_callback_and_build_exchange_redirect() {
        let parsed = parse_callback(
            "http://localhost:3128/oauth/callback?code=abc&state=state-1&login_option=github",
        )
        .unwrap();
        assert_eq!(parsed.code, "abc");
        assert_eq!(parsed.state, "state-1");
        assert_eq!(parsed.login_option, "github");
        assert_eq!(
            exchange_redirect_uri(&parsed),
            "http://localhost:3128/oauth/callback?login_option=github"
        );
    }

    #[test]
    fn reject_non_localhost_callback() {
        let error =
            parse_callback("https://example.com/oauth/callback?code=a&state=b&login_option=github")
                .unwrap_err();
        assert!(error.contains("localhost"));
    }
}
