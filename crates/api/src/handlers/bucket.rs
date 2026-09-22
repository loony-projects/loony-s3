use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use loony_core::BucketName;
use loony_object::S3Error;

use super::request_id;
use crate::auth::AuthenticatedOwner;
use crate::error::ApiError;
use crate::state::AppState;
use crate::xml;

fn parse_bucket_name(raw: &str, rid: &str) -> Result<BucketName, ApiError> {
    BucketName::parse(raw).map_err(|e| {
        ApiError::new(
            S3Error::InvalidBucketName(e.reason.to_string()),
            rid.to_string(),
            Some(format!("/{raw}")),
        )
    })
}

pub async fn list_buckets(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let buckets = state
        .buckets
        .list_buckets(owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), None))?;
    let body = xml::list_all_my_buckets(&buckets, owner.0);
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        body,
    )
        .into_response())
}

pub async fn create_bucket(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path(bucket): Path<String>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let name = parse_bucket_name(&bucket, &rid)?;
    state
        .buckets
        .create_bucket(name, owner.0, state.region.clone())
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}"))))?;
    Ok((StatusCode::OK, [(header::LOCATION, format!("/{bucket}"))]).into_response())
}

pub async fn delete_bucket(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path(bucket): Path<String>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let name = parse_bucket_name(&bucket, &rid)?;
    state
        .buckets
        .delete_bucket(&name, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}"))))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn head_bucket(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path(bucket): Path<String>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let name = parse_bucket_name(&bucket, &rid)?;
    state
        .buckets
        .head_bucket(&name, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}"))))?;
    Ok(StatusCode::OK.into_response())
}

#[derive(Debug, Deserialize)]
pub struct ListObjectsV2Params {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub delimiter: Option<String>,
    #[serde(rename = "start-after", default)]
    pub start_after: Option<String>,
    #[serde(rename = "continuation-token", default)]
    pub continuation_token: Option<String>,
    #[serde(rename = "max-keys", default)]
    pub max_keys: Option<u32>,
}

pub async fn list_objects_v2(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path(bucket): Path<String>,
    Query(params): Query<ListObjectsV2Params>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let name = parse_bucket_name(&bucket, &rid)?;
    let max_keys = params.max_keys.unwrap_or(1000).clamp(1, 1000);

    let page = state
        .objects
        .list_objects(
            &name,
            loony_object::ListObjectsParams {
                prefix: params.prefix.clone(),
                delimiter: params.delimiter.clone(),
                start_after: params.start_after,
                continuation_token: params.continuation_token,
                max_keys,
            },
            owner.0,
        )
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}"))))?;

    let body = xml::list_bucket_result(
        &bucket,
        params.prefix.as_deref(),
        params.delimiter.as_deref(),
        max_keys,
        &page,
    );
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        body,
    )
        .into_response())
}
