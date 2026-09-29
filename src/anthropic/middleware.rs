//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::common::auth;
use crate::kiro::provider::KiroProvider;
use crate::usage_log::UsageLog;

use super::types::ErrorResponse;

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥
    pub api_key: String,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 请求和价格记录。未配置时不记录。
    pub usage: Option<Arc<UsageLog>>,
    /// 接入 Key 和调度分组。未配置时仍使用单一 apiKey。
    pub access: Option<Arc<crate::access::AccessStore>>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(api_key: impl Into<String>, extract_thinking: bool) -> Self {
        Self {
            api_key: api_key.into(),
            kiro_provider: None,
            extract_thinking,
            usage: None,
            access: None,
        }
    }

    pub fn with_access(mut self, access: Arc<crate::access::AccessStore>) -> Self {
        self.access = Some(access);
        self
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }

    pub fn with_usage(mut self, usage: Arc<UsageLog>) -> Self {
        self.usage = Some(usage);
        self
    }
}

/// API Key 认证中间件
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(presented) = auth::extract_api_key(&request) else {
        let error = ErrorResponse::authentication_error();
        return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
    };
    if let Some(access) = &state.access {
        let Some(found) = access.authenticate(&presented) else {
            let error = ErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        };
        let ids = state
            .kiro_provider
            .as_ref()
            .map(|provider| provider.credential_ids())
            .unwrap_or_default();
        let allowed = access.allowed_credentials(&found.group_id, &ids);
        return crate::access::run_with_credentials(allowed, next.run(request)).await;
    }
    if auth::constant_time_eq(&presented, &state.api_key) {
        next.run(request).await
    } else {
        let error = ErrorResponse::authentication_error();
        (StatusCode::UNAUTHORIZED, Json(error)).into_response()
    }
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}
