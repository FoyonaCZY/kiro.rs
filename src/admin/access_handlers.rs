//! 接入 Key 和调度分组的管理接口。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;

use crate::access::{AccessGroup, AccessKey};

use super::middleware::AdminState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewKey {
    name: String,
    #[serde(default)]
    secret: String,
    #[serde(default)]
    group_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PatchKey {
    name: Option<String>,
    group_id: Option<String>,
    disabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewGroup {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameGroup {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Members {
    credential_ids: Vec<u64>,
}

pub async fn list_keys(State(state): State<AdminState>) -> Json<Vec<AccessKey>> {
    Json(state.service.access.list_keys())
}

pub async fn create_key(State(state): State<AdminState>, Json(body): Json<NewKey>) -> Response {
    match state
        .service
        .access
        .create_key(&body.name, &body.secret, &body.group_id)
    {
        Ok(key) => (StatusCode::OK, Json(key)).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn update_key(
    State(state): State<AdminState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchKey>,
) -> Response {
    match state.service.access.update_key(
        id,
        body.name.as_deref(),
        body.group_id.as_deref(),
        body.disabled,
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn delete_key(State(state): State<AdminState>, Path(id): Path<i64>) -> Response {
    match state.service.access.delete_key(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn list_groups(State(state): State<AdminState>) -> Json<Vec<AccessGroup>> {
    let mut groups = state.service.access.list_groups();
    let known = state.service.credential_ids();
    if let Some(default_group) = groups.iter_mut().find(|group| group.is_default) {
        let explicit: std::collections::HashSet<u64> = default_group.members.iter().copied().collect();
        let assigned = state.service.access.assigned_credentials();
        for id in known {
            if !assigned.contains(&id) && !explicit.contains(&id) {
                default_group.members.push(id);
            }
        }
        default_group.members.sort_unstable();
        default_group.members.dedup();
    }
    Json(groups)
}

pub async fn create_group(
    State(state): State<AdminState>,
    Json(body): Json<NewGroup>,
) -> Response {
    match state.service.access.create_group(&body.name) {
        Ok(group) => (StatusCode::OK, Json(group)).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn rename_group(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<RenameGroup>,
) -> Response {
    match state.service.access.rename_group(&id, &body.name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn delete_group(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    match state.service.access.delete_group(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

pub async fn set_members(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<Members>,
) -> Response {
    match state.service.access.set_members(
        &id,
        &body.credential_ids,
        &state.service.credential_ids(),
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

type Response = axum::response::Response;
