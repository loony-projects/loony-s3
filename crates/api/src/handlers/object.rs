use std::collections::BTreeMap;

use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;

use loony_core::{BucketName, ObjectKey, UploadId};
use loony_object::S3Error;
use loony_storage::ShardBytesIn;

use super::request_id;
use crate::auth::AuthenticatedOwner;
use crate::error::ApiError;
use crate::http_date::http_date;
use crate::state::AppState;
use crate::xml;

/// Body-size cap for a `CompleteMultipartUpload` request: the part list is the only
/// thing in it, and even the real S3 maximum of 10,000 parts comfortably fits in a
/// fraction of this (each `<Part>` element is well under 100 bytes).
const MAX_COMPLETE_MULTIPART_BODY: usize = 4 * 1024 * 1024;

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

/// A malformed `uploadId` can never match a real upload, so it's treated exactly like
/// one that doesn't exist (matching real S3's own behavior) rather than surfaced as a
/// separate `InvalidArgument`.
fn parse_upload_id(raw: &str, rid: &str, resource: &str) -> Result<UploadId, ApiError> {
    raw.parse::<UploadId>().map_err(|_| {
        ApiError::new(
            S3Error::NoSuchUpload,
            rid.to_string(),
            Some(resource.to_string()),
        )
    })
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

/// Query parameters distinguishing a multipart operation from its plain-object
/// counterpart on the same route (prompt §51: "plus multipart query variants") --
/// `PUT`/`GET`/`DELETE`/`POST` on `/{bucket}/{key}` are shared between whole-object and
/// multipart operations in the real S3 API too, disambiguated the same way here.
#[derive(Debug, Deserialize, Default)]
pub struct MultipartQuery {
    #[serde(default)]
    pub uploads: Option<String>,
    #[serde(rename = "uploadId", default)]
    pub upload_id: Option<String>,
    #[serde(rename = "partNumber", default)]
    pub part_number: Option<u32>,
}

pub async fn put_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<MultipartQuery>,
    body: Body,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let resource = format!("/{bucket}/{key}");

    if let (Some(upload_id_raw), Some(part_number)) = (&query.upload_id, query.part_number) {
        let upload_id = parse_upload_id(upload_id_raw, &rid, &resource)?;
        let stream = body
            .into_data_stream()
            .map(|item| item.map_err(std::io::Error::other));
        let boxed: ShardBytesIn = Box::pin(stream);

        let etag = state
            .objects
            .upload_part(
                &bucket_name,
                &object_key,
                upload_id,
                part_number,
                boxed,
                owner.0,
            )
            .await
            .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

        return Ok((StatusCode::OK, [(header::ETAG, xml::quoted_etag(&etag))]).into_response());
    }

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
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

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
    Query(query): Query<MultipartQuery>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let resource = format!("/{bucket}/{key}");

    if let Some(upload_id_raw) = &query.upload_id {
        let upload_id = parse_upload_id(upload_id_raw, &rid, &resource)?;
        let parts = state
            .objects
            .list_parts(&bucket_name, &object_key, upload_id, owner.0)
            .await
            .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

        let body = xml::list_parts_result(&bucket, &key, upload_id, &parts);
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response());
    }

    let (manifest, stream) = state
        .objects
        .get_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

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
    Query(query): Query<MultipartQuery>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let resource = format!("/{bucket}/{key}");

    if let Some(upload_id_raw) = &query.upload_id {
        let upload_id = parse_upload_id(upload_id_raw, &rid, &resource)?;
        state
            .objects
            .abort_multipart_upload(&bucket_name, &object_key, upload_id, owner.0)
            .await
            .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    state
        .objects
        .delete_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct CompleteMultipartUploadRequest {
    #[serde(rename = "Part", default)]
    part: Vec<CompletedPartXml>,
}

#[derive(Debug, Deserialize)]
struct CompletedPartXml {
    #[serde(rename = "PartNumber")]
    part_number: u32,
    #[serde(rename = "ETag")]
    etag: String,
}

/// `POST /{bucket}/{key}` is exclusively a multipart entry point (prompt §51's "plus
/// multipart query variants") -- `?uploads` starts one, `?uploadId=X` (with a body
/// listing parts) completes one. Nothing else in the S3 API this server implements uses
/// a bare POST on this route.
pub async fn post_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<MultipartQuery>,
    body: Body,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let resource = format!("/{bucket}/{key}");

    if query.uploads.is_some() {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let metadata = user_metadata(&headers);

        let upload_id = state
            .objects
            .create_multipart_upload(&bucket_name, object_key, content_type, metadata, owner.0)
            .await
            .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

        let body = xml::initiate_multipart_upload_result(&bucket, &key, upload_id);
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response());
    }

    if let Some(upload_id_raw) = &query.upload_id {
        let upload_id = parse_upload_id(upload_id_raw, &rid, &resource)?;

        let bytes = to_bytes(body, MAX_COMPLETE_MULTIPART_BODY)
            .await
            .map_err(|e| {
                ApiError::new(
                    S3Error::InvalidArgument(format!("failed to read request body: {e}")),
                    rid.clone(),
                    Some(resource.clone()),
                )
            })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ApiError::new(
                S3Error::InvalidArgument("request body is not valid UTF-8".into()),
                rid.clone(),
                Some(resource.clone()),
            )
        })?;
        let parsed: CompleteMultipartUploadRequest =
            quick_xml::de::from_str(text).map_err(|e| {
                ApiError::new(
                    S3Error::InvalidArgument(format!(
                        "malformed CompleteMultipartUpload body: {e}"
                    )),
                    rid.clone(),
                    Some(resource.clone()),
                )
            })?;
        let requested_parts: Vec<(u32, String)> = parsed
            .part
            .into_iter()
            .map(|p| (p.part_number, p.etag.trim_matches('"').to_string()))
            .collect();

        let manifest = state
            .objects
            .complete_multipart_upload(
                &bucket_name,
                &object_key,
                upload_id,
                requested_parts,
                owner.0,
            )
            .await
            .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

        let body = xml::complete_multipart_upload_result(&bucket, &key, &manifest.etag);
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response());
    }

    Err(ApiError::new(
        S3Error::InvalidArgument("unsupported operation".into()),
        rid,
        Some(resource),
    ))
}
