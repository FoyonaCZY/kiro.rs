//! 请求记录和价格管理接口

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};

use crate::usage_log::{AccountCostReport, ModelPrice, RequestView, UsageSummary};

use super::middleware::AdminState;

pub async fn list_requests(State(state): State<AdminState>) -> Json<Vec<RequestView>> {
    Json(state.service.usage.list(200))
}

pub async fn get_request(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> Response {
    match state.service.usage.get(id) {
        Some(detail) => (StatusCode::OK, Json(detail)).into_response(),
        None => (StatusCode::NOT_FOUND, "请求不存在").into_response(),
    }
}

pub async fn usage_summary(State(state): State<AdminState>) -> Json<UsageSummary> {
    Json(state.service.usage.summary())
}

pub async fn list_prices(State(state): State<AdminState>) -> Json<Vec<ModelPrice>> {
    Json(state.service.usage.prices())
}

pub async fn upsert_price(
    State(state): State<AdminState>,
    Json(price): Json<ModelPrice>,
) -> Response {
    match state.service.usage.upsert_price(price) {
        Ok(price) => (StatusCode::OK, Json(price)).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn delete_price(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    match state.service.usage.delete_price(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::NOT_FOUND, err).into_response(),
    }
}

/// 账号人民币成本、按当前单价重算的累计消耗和成本倍率
pub async fn account_costs(State(state): State<AdminState>) -> Json<AccountCostReport> {
    Json(state.service.account_costs())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCostBody {
    /// 人民币成本。null 表示清空（未知，不参与倍率），0 表示免费账号
    pub cost_cny: Option<f64>,
}

pub async fn set_account_cost(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(body): Json<AccountCostBody>,
) -> Response {
    match state.service.set_account_cost(id, body.cost_cny) {
        Ok(()) => (StatusCode::OK, Json(state.service.account_costs())).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

type Response = axum::response::Response;
