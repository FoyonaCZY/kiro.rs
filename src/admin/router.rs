//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

use super::{
    access_handlers::{
        create_group, create_key, delete_group, delete_key, list_groups, list_keys, rename_group,
        set_members, update_key,
    },
    handlers::{
        add_credential, complete_social_login, delete_credential, force_refresh_token,
        get_all_credentials, get_credential_balance, get_load_balancing_mode, reset_failure_count,
        set_credential_disabled, set_credential_priority, set_load_balancing_mode,
        start_social_login, update_credential,
    },
    middleware::{AdminState, admin_auth_middleware},
    usage_handlers::{
        account_costs, delete_price, get_request, list_prices, list_requests, set_account_cost,
        upsert_price, usage_summary,
    },
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /config/load-balancing` - 获取负载均衡模式
/// - `PUT /config/load-balancing` - 设置负载均衡模式
///
/// # 认证
/// 需要 Admin API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    Router::new()
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route(
            "/credentials/{id}",
            put(update_credential).delete(delete_credential),
        )
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route("/social-login/start", post(start_social_login))
        .route("/social-login/complete", post(complete_social_login))
        .route("/usage/requests", get(list_requests))
        .route("/usage/requests/{id}", get(get_request))
        .route("/usage/summary", get(usage_summary))
        .route("/usage/prices", get(list_prices).post(upsert_price))
        .route("/usage/prices/{id}", delete(delete_price))
        .route("/usage/accounts", get(account_costs))
        .route("/usage/accounts/{id}/cost", put(set_account_cost))
        .route("/access/keys", get(list_keys).post(create_key))
        .route("/access/keys/{id}", put(update_key).delete(delete_key))
        .route("/access/groups", get(list_groups).post(create_group))
        .route(
            "/access/groups/{id}",
            put(rename_group).delete(delete_group),
        )
        .route("/access/groups/{id}/members", put(set_members))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}
