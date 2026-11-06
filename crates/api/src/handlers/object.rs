use std::collections::BTreeMap;

use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use loony_auth::ChunkSigner;
use loony_core::{BucketName, ObjectKey, ObjectManifest, UploadId};
use loony_object::{ByteRange, Ls3Error};

use super::request_id;
use crate::auth::AuthenticatedOwner;
use crate::error::ApiError;
use crate::http_date::http_date;
use crate::payload;
use crate::state::AppState;
use crate::xml;

/// Keeps shared caches such as a CDN from storing objects. One would otherwise go on serving
/// a presigned link after it expires, or a file after it's deleted. A person's own browser
/// may still cache what it was allowed to fetch, which is what makes video seeking cheap.
const OBJECT_CACHE_CONTROL: &str = "private";

/// Body-size cap for a `CompleteMultipartUpload` request: the part list is the only
/// thing in it, and even the upstream protocol's maximum of 10,000 parts comfortably fits in a
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
            Ls3Error::InvalidBucketName(e.reason.to_string()),
            rid.to_string(),
            Some(resource.clone()),
        )
    })?;
    let object_key = ObjectKey::parse(key).map_err(|e| {
        ApiError::new(
            Ls3Error::InvalidArgument(e.reason.to_string()),
            rid.to_string(),
            Some(resource),
        )
    })?;
    Ok((bucket_name, object_key))
}

/// A malformed `uploadId` can never match a real upload, so it's treated exactly like
/// one that doesn't exist (matching the upstream protocol's own behavior) rather than surfaced as a
/// separate `InvalidArgument`.
fn parse_upload_id(raw: &str, rid: &str, resource: &str) -> Result<UploadId, ApiError> {
    raw.parse::<UploadId>().map_err(|_| {
        ApiError::new(
            Ls3Error::NoSuchUpload,
            rid.to_string(),
            Some(resource.to_string()),
        )
    })
}

/// Collects `x-amz-meta-*` request headers into the object's user metadata
/// (prompt §60). Header names arriving here are already lowercase (the `http` crate
/// normalizes them), matching LS3's case-insensitive-but-conventionally-lowercase
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

/// Parses a `Range` header. Anything but a single well-formed byte range gives `None`,
/// which means serving the whole object: HTTP lets a server ignore a Range it can't honor,
/// and the upstream protocol does the same for multiple ranges.
fn parse_range(value: &str) -> Option<ByteRange> {
    let spec = value.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if start.is_empty() {
        return digits(end).then(|| end.parse().ok().map(ByteRange::Suffix))?;
    }
    if !digits(start) {
        return None;
    }
    let start = start.parse().ok()?;
    if end.is_empty() {
        return Some(ByteRange::From { start, end: None });
    }
    if !digits(end) {
        return None;
    }
    let end = end.parse().ok()?;
    (start <= end).then_some(ByteRange::From {
        start,
        end: Some(end),
    })
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `If-Match`: refuses the read if the object has changed. Clients downloading in pieces
/// rely on this so a file replaced partway through can't end up mixed together.
fn check_if_match(headers: &HeaderMap, manifest: &ObjectManifest) -> Result<(), Ls3Error> {
    let Some(value) = header_text(headers, header::IF_MATCH) else {
        return Ok(());
    };
    let current = xml::quoted_etag(&manifest.etag);
    let matches = value
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate == current);
    if matches {
        Ok(())
    } else {
        Err(Ls3Error::PreconditionFailed)
    }
}

/// `If-Range` says to apply `Range` only if the object is unchanged, and otherwise to send
/// the whole new object. The value is either the ETag or the Last-Modified date.
fn if_range_holds(headers: &HeaderMap, manifest: &ObjectManifest) -> bool {
    header_text(headers, header::IF_RANGE).is_none_or(|value| {
        let value = value.trim();
        value == xml::quoted_etag(&manifest.etag) || value == http_date(manifest.created_at)
    })
}

/// The inclusive byte range to serve, or `None` for the whole object.
fn requested_range(
    headers: &HeaderMap,
    manifest: &ObjectManifest,
) -> Result<Option<(u64, u64)>, Ls3Error> {
    if !if_range_holds(headers, manifest) {
        return Ok(None);
    }
    header_text(headers, header::RANGE)
        .and_then(parse_range)
        .map(|range| range.resolve(manifest.size))
        .transpose()
}

/// Query parameters distinguishing a multipart operation from its plain-object
/// counterpart on the same route (prompt §51: "plus multipart query variants") --
/// `PUT`/`GET`/`DELETE`/`POST` on `/{bucket}/{key}` are shared between whole-object and
/// multipart operations in the upstream protocol too, disambiguated the same way here.
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
    chunk_signer: Option<Extension<ChunkSigner>>,
    body: Body,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let resource = format!("/{bucket}/{key}");

    let (boxed, failure) = payload::decode_body(&headers, chunk_signer.map(|s| s.0), body)
        .map_err(|e| ApiError::new(e.into(), rid.clone(), Some(resource.clone())))?;

    if let (Some(upload_id_raw), Some(part_number)) = (&query.upload_id, query.part_number) {
        let upload_id = parse_upload_id(upload_id_raw, &rid, &resource)?;
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
            .map_err(|e| ApiError::new(failure.explain(e), rid.clone(), Some(resource)))?;

        return Ok((StatusCode::OK, [(header::ETAG, xml::quoted_etag(&etag))]).into_response());
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let metadata = user_metadata(&headers);

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
        .map_err(|e| ApiError::new(failure.explain(e), rid.clone(), Some(resource)))?;

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

    let manifest = state
        .objects
        .head_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource.clone())))?;
    check_if_match(&headers, &manifest)
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource.clone())))?;
    let range = requested_range(&headers, &manifest)
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

    let (start, end) = range.unwrap_or((0, manifest.size.saturating_sub(1)));
    // An empty object has no bytes to read, and `end` can't express that.
    let body = if manifest.size == 0 {
        Body::empty()
    } else {
        Body::from_stream(state.objects.read_range(&manifest, start, end))
    };

    // Safe: every header value here comes from data already validated as a HeaderValue
    // once before (content_type was accepted via `.to_str()` on the way in) or is a
    // value we constructed ourselves (ETag, lengths, Last-Modified) that is always ASCII.
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, &manifest.content_type)
        .header(header::CACHE_CONTROL, OBJECT_CACHE_CONTROL)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, xml::quoted_etag(&manifest.etag))
        .header(header::LAST_MODIFIED, http_date(manifest.created_at));
    response = match range {
        Some((start, end)) => response
            .header(header::CONTENT_LENGTH, end - start + 1)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", manifest.size),
            ),
        None => response.header(header::CONTENT_LENGTH, manifest.size),
    };
    Ok(response
        .body(body)
        .expect("response built entirely from previously-validated header values")
        .into_response())
}

pub async fn head_object(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthenticatedOwner>,
    headers: HeaderMap,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let rid = request_id(&headers);
    let (bucket_name, object_key) = parse_bucket_and_key(&bucket, &key, &rid)?;
    let resource = format!("/{bucket}/{key}");
    let manifest = state
        .objects
        .head_object(&bucket_name, &object_key, owner.0)
        .await
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource.clone())))?;
    check_if_match(&headers, &manifest)
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource.clone())))?;
    let range = requested_range(&headers, &manifest)
        .map_err(|e| ApiError::new(e, rid.clone(), Some(resource)))?;

    let common = [
        (header::CONTENT_TYPE, manifest.content_type.clone()),
        (header::CACHE_CONTROL, OBJECT_CACHE_CONTROL.to_string()),
        (header::ACCEPT_RANGES, "bytes".to_string()),
        (header::ETAG, xml::quoted_etag(&manifest.etag)),
        (header::LAST_MODIFIED, http_date(manifest.created_at)),
    ];
    Ok(match range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            common,
            [
                (header::CONTENT_LENGTH, (end - start + 1).to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", manifest.size),
                ),
            ],
        )
            .into_response(),
        None => (
            StatusCode::OK,
            common,
            [(header::CONTENT_LENGTH, manifest.size.to_string())],
        )
            .into_response(),
    })
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
/// listing parts) completes one. Nothing else in the LS3 API this server implements uses
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
                    Ls3Error::InvalidArgument(format!("failed to read request body: {e}")),
                    rid.clone(),
                    Some(resource.clone()),
                )
            })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ApiError::new(
                Ls3Error::InvalidArgument("request body is not valid UTF-8".into()),
                rid.clone(),
                Some(resource.clone()),
            )
        })?;
        let parsed: CompleteMultipartUploadRequest =
            quick_xml::de::from_str(text).map_err(|e| {
                ApiError::new(
                    Ls3Error::InvalidArgument(format!(
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
        Ls3Error::InvalidArgument("unsupported operation".into()),
        rid,
        Some(resource),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_byte_ranges() {
        let from = |start, end| Some(ByteRange::From { start, end });
        assert_eq!(parse_range("bytes=0-99"), from(0, Some(99)));
        assert_eq!(parse_range("bytes=5-5"), from(5, Some(5)));
        assert_eq!(parse_range("bytes=100-"), from(100, None));
        assert_eq!(parse_range("bytes=-20"), Some(ByteRange::Suffix(20)));
        assert_eq!(parse_range("bytes=-0"), Some(ByteRange::Suffix(0)));
    }

    #[test]
    fn anything_else_means_the_whole_object() {
        for value in [
            "",
            "0-99",
            "items=0-5",
            "bytes=",
            "bytes=-",
            "bytes=a-b",
            "bytes=5-2",
            "bytes=0-1,5-6",
            "bytes=+1-2",
            "bytes=1-+2",
            "bytes=-+2",
            "bytes=99999999999999999999999-",
            "bytes=-99999999999999999999999",
        ] {
            assert_eq!(parse_range(value), None, "{value:?}");
        }
    }
}
