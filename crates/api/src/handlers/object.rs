use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use s3_core::{BucketName, ObjectKey};
use s3_object::S3Error;
use s3_storage::ShardBytesIn;

use super::request_id;
use crate::auth::AuthenticatedOwner;
use crate::error::ApiError;
use crate::http_date::http_date;
use crate::state::AppState;
use crate::xml;

fn parse_bucket_and_key(
    bucket: &str,
    key: &str,
    rid: &str,
) -> Result<(BucketName, ObjectKey), ApiError> {
    let resource = format!("/{bucket}/{key}");
    let bucket_name = BucketName::parse(bucket).map_err(|e| {
        ApiError::new(
            S3Error::InvalidBucketName(e.reason.to_string()),
            rid.to_string(),
            Some(resource.clone()),
        )
    })?;
    let object_key = ObjectKey::parse(key).map_err(|e| {
        ApiError::new(
            S3Error::InvalidArgument(e.reason.to_string()),
            rid.to_string(),
            Some(resource),
        )
    })?;
    Ok((bucket_name, object_key))
}

/// Collects `x-amz-meta-*` request headers into the object's user metadata
/// (prompt §60). Header names arriving here are already lowercase (the `http` crate
/// normalizes them), matching S3's case-insensitive-but-conventionally-lowercase
/// treatment of these headers.
fn user_metadata(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (name, value) in headers {
        if let Some(suffix) = name.as_str().strip_prefix("x-amz-meta-")
            && let Ok(v) = value.to_str()
        {
            map.insert(suffix.to_string(), v.to_string());
        }
    }
    map
}

pub async fn put_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
    body: Body,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let metadata = user_metadata(&headers);

    let stream = body
        .into_data_stream()
        .map(|item| item.map_err(std::io::Error::other));
    let boxed: ShardBytesIn = Box::pin(stream);

    let manifest = state
        .objects
        .put_object(
            &bucket_name,
            object_key,
            content_type,
            metadata,
            boxed,
            owner.0,
        )
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}/{key}"))))?;

    Ok((
        StatusCode::OK,
        [(header::ETAG, xml::quoted_etag(&manifest.etag))],
    )
        .into_response())
}

pub async fn get_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let (manifest, stream) = state
        .objects
        .get_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}/{key}"))))?;

    let body = Body::from_stream(stream);
    // Safe: every header value here comes from data already validated as a HeaderValue
    // once before (content_type was accepted via `.to_str()` on the way in) or is a
    // value we constructed ourselves (ETag, Content-Length, Last-Modified) that is
    // always ASCII.
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, manifest.content_type)
        .header(header::CONTENT_LENGTH, manifest.size)
        .header(header::ETAG, xml::quoted_etag(&manifest.etag))
        .header(header::LAST_MODIFIED, http_date(manifest.created_at))
        .body(body)
        .expect("response built entirely from previously-validated header values");
    Ok(response.into_response())
}

pub async fn head_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let manifest = state
        .objects
        .head_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}/{key}"))))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, manifest.content_type),
            (header::CONTENT_LENGTH, manifest.size.to_string()),
            (header::ETAG, xml::quoted_etag(&manifest.etag)),
            (header::LAST_MODIFIED, http_date(manifest.created_at)),
        ],
    )
        .into_response())
}

pub async fn delete_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    state
        .objects
        .delete_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(format!("/{bucket}/{key}"))))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
