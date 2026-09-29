//! 请求记录和价格管理接口

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};

use crate::usage_log::{ModelPrice, RequestView, UsageSummary};

use super::middleware::AdminState;

pub async fn list_requests(State(state): State<AdminState>) -> Json<Vec<RequestView>> {
    Json(state.service.usage.list(200))
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

type Response = axum::response::Response;
