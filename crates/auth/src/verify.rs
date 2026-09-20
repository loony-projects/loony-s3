//! SigV4 request verification (prompt §52-53): header-based `Authorization` and
//! presigned-URL query parameters, sharing the same canonical-request machinery.
//! Constant-time signature comparison throughout (prompt §52, §83: "signature timing
//! attacks").

use time::{Duration, OffsetDateTime};

use crate::canonical::{
    canonical_headers, canonical_query_string, canonical_uri, constant_time_eq, hmac_sha256,
    parse_query_string, sha256_hex,
};
use crate::credential::CredentialProvider;
use s3_core::OwnerId;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SERVICE: &str = "s3";

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing Authorization header or presigned signature")]
    MissingSignature,
    #[error("malformed signature parameters")]
    Malformed,
    #[error("missing required header: {0}")]
    MissingHeader(String),
    #[error("missing required query parameter: {0}")]
    MissingQueryParam(String),
    #[error("unsupported signing algorithm")]
    UnsupportedAlgorithm,
    #[error("request timestamp is outside the acceptable window")]
    RequestExpired,
    #[error("the access key does not exist")]
    UnknownAccessKey,
    #[error("credential is disabled")]
    CredentialDisabled,
    #[error("the request signature does not match")]
    SignatureMismatch,
    #[error("credential scope does not match the expected region/service")]
    InvalidCredentialScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRequest {
    pub access_key: String,
    pub owner_id: OwnerId,
}

/// The pieces of an HTTP request SigV4 verification needs. Framework-agnostic on
/// purpose: `s3-api` is the only crate that knows how to pull these out of an axum
/// `Request`.
pub struct RequestParts<'a> {
    pub method: &'a str,
    /// Raw request-target path, still percent-encoded exactly as received on the wire.
    pub path: &'a str,
    /// Raw query string (no leading `?`).
    pub query: &'a str,
    /// Every header on the request, name exactly as received (case-insensitive lookups
    /// are done internally).
    pub headers: &'a [(String, String)],
}

pub async fn verify(
    parts: &RequestParts<'_>,
    region: &str,
    credentials: &dyn CredentialProvider,
    now: OffsetDateTime,
    max_skew: Duration,
) -> Result<VerifiedRequest, AuthError> {
    let query_pairs = parse_query_string(parts.query);
    if query_pairs.iter().any(|(k, _)| k == "X-Amz-Signature") {
        verify_presigned(parts, &query_pairs, region, credentials, now).await
    } else {
        verify_header(parts, &query_pairs, region, credentials, now, max_skew).await
    }
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}

struct ParsedAuth {
    access_key: String,
    date: String,
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization_header(value: &str) -> Result<ParsedAuth, AuthError> {
    let rest = value
        .trim()
        .strip_prefix(ALGORITHM)
        .ok_or(AuthError::UnsupportedAlgorithm)?;

    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for part in rest.trim().split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(v.split(';').map(|s| s.to_string()).collect::<Vec<_>>());
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = Some(v.to_string());
        }
    }

    let credential = credential.ok_or(AuthError::Malformed)?;
    let (access_key, date, region, service) = parse_credential_scope(&credential)?;

    Ok(ParsedAuth {
        access_key,
        date,
        region,
        service,
        signed_headers: signed_headers.ok_or(AuthError::Malformed)?,
        signature: signature.ok_or(AuthError::Malformed)?,
    })
}

/// `{access_key}/{date}/{region}/{service}/aws4_request`
fn parse_credential_scope(credential: &str) -> Result<(String, String, String, String), AuthError> {
    let mut scope = credential.splitn(5, '/');
    let access_key = scope.next().ok_or(AuthError::Malformed)?.to_string();
    let date = scope.next().ok_or(AuthError::Malformed)?.to_string();
    let region = scope.next().ok_or(AuthError::Malformed)?.to_string();
    let service = scope.next().ok_or(AuthError::Malformed)?.to_string();
    let terminator = scope.next().ok_or(AuthError::Malformed)?;
    if terminator != "aws4_request" {
        return Err(AuthError::Malformed);
    }
    Ok((access_key, date, region, service))
}

fn parse_amz_date(s: &str) -> Result<OffsetDateTime, AuthError> {
    if s.len() != 16 || !s.ends_with('Z') || s.as_bytes().get(8) != Some(&b'T') {
        return Err(AuthError::Malformed);
    }
    let field = |range: std::ops::Range<usize>| -> Result<u8, AuthError> {
        s.get(range)
            .and_then(|s| s.parse().ok())
            .ok_or(AuthError::Malformed)
    };
    let year: i32 = s
        .get(0..4)
        .and_then(|s| s.parse().ok())
        .ok_or(AuthError::Malformed)?;
    let month = time::Month::try_from(field(4..6)?).map_err(|_| AuthError::Malformed)?;
    let day = field(6..8)?;
    let hour = field(9..11)?;
    let minute = field(11..13)?;
    let second = field(13..15)?;

    let date =
        time::Date::from_calendar_date(year, month, day).map_err(|_| AuthError::Malformed)?;
    let time = time::Time::from_hms(hour, minute, second).map_err(|_| AuthError::Malformed)?;
    Ok(date.with_time(time).assume_utc())
}

fn build_canonical_request(
    method: &str,
    path: &str,
    query_pairs: &[(String, String)],
    headers: &[(String, String)],
    signed_headers: &[String],
    payload_hash: &str,
) -> String {
    let canonical_uri = canonical_uri(path);
    let canonical_qs = canonical_query_string(query_pairs);
    let canonical_hdrs = canonical_headers(headers, signed_headers);
    // The SignedHeaders line must be exactly what the client put in the credential --
    // NOT independently re-sorted here -- since it's part of what was literally signed.
    let signed_headers_line = signed_headers.join(";");
    format!(
        "{method}\n{canonical_uri}\n{canonical_qs}\n{canonical_hdrs}\n{signed_headers_line}\n{payload_hash}"
    )
}

fn build_string_to_sign(
    amz_date: &str,
    date: &str,
    region: &str,
    service: &str,
    canonical_request: &str,
) -> String {
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let hashed = sha256_hex(canonical_request.as_bytes());
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{hashed}")
}

fn compute_signature(
    secret_key: &str,
    date: &str,
    region: &str,
    service: &str,
    string_to_sign: &str,
) -> String {
    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    crate::canonical::hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()))
}

async fn verify_header(
    parts: &RequestParts<'_>,
    query_pairs: &[(String, String)],
    region: &str,
    credentials: &dyn CredentialProvider,
    now: OffsetDateTime,
    max_skew: Duration,
) -> Result<VerifiedRequest, AuthError> {
    let auth_header =
        header_value(parts.headers, "authorization").ok_or(AuthError::MissingSignature)?;
    let parsed = parse_authorization_header(&auth_header)?;

    let amz_date = header_value(parts.headers, "x-amz-date")
        .ok_or_else(|| AuthError::MissingHeader("x-amz-date".into()))?;
    let request_time = parse_amz_date(&amz_date)?;
    if (now - request_time).abs() > max_skew {
        return Err(AuthError::RequestExpired);
    }
    if parsed.date != amz_date[0..8] || parsed.region != region || parsed.service != SERVICE {
        return Err(AuthError::InvalidCredentialScope);
    }

    let cred = credentials
        .get_secret(&parsed.access_key)
        .await
        .ok_or(AuthError::UnknownAccessKey)?;
    if !cred.enabled {
        return Err(AuthError::CredentialDisabled);
    }

    let payload_hash = header_value(parts.headers, "x-amz-content-sha256")
        .unwrap_or_else(|| "UNSIGNED-PAYLOAD".to_string());

    let canonical_request = build_canonical_request(
        parts.method,
        parts.path,
        query_pairs,
        parts.headers,
        &parsed.signed_headers,
        &payload_hash,
    );
    let string_to_sign = build_string_to_sign(
        &amz_date,
        &parsed.date,
        &parsed.region,
        &parsed.service,
        &canonical_request,
    );
    let expected = compute_signature(
        &cred.secret_key,
        &parsed.date,
        &parsed.region,
        &parsed.service,
        &string_to_sign,
    );

    if !constant_time_eq(&expected, &parsed.signature) {
        return Err(AuthError::SignatureMismatch);
    }

    Ok(VerifiedRequest {
        access_key: parsed.access_key,
        owner_id: cred.owner_id,
    })
}

async fn verify_presigned(
    parts: &RequestParts<'_>,
    query_pairs: &[(String, String)],
    region: &str,
    credentials: &dyn CredentialProvider,
    now: OffsetDateTime,
) -> Result<VerifiedRequest, AuthError> {
    let get = |name: &str| {
        query_pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    let algorithm = get("X-Amz-Algorithm")
        .ok_or_else(|| AuthError::MissingQueryParam("X-Amz-Algorithm".into()))?;
    if algorithm != ALGORITHM {
        return Err(AuthError::UnsupportedAlgorithm);
    }
    let credential = get("X-Amz-Credential")
        .ok_or_else(|| AuthError::MissingQueryParam("X-Amz-Credential".into()))?;
    let amz_date =
        get("X-Amz-Date").ok_or_else(|| AuthError::MissingQueryParam("X-Amz-Date".into()))?;
    let expires_secs: i64 = get("X-Amz-Expires")
        .ok_or_else(|| AuthError::MissingQueryParam("X-Amz-Expires".into()))?
        .parse()
        .map_err(|_| AuthError::Malformed)?;
    let signed_headers: Vec<String> = get("X-Amz-SignedHeaders")
        .ok_or_else(|| AuthError::MissingQueryParam("X-Amz-SignedHeaders".into()))?
        .split(';')
        .map(|s| s.to_string())
        .collect();
    let signature = get("X-Amz-Signature").ok_or(AuthError::MissingSignature)?;

    let (access_key, date, cred_region, service) = parse_credential_scope(&credential)?;
    if cred_region != region || service != SERVICE {
        return Err(AuthError::InvalidCredentialScope);
    }

    let request_time = parse_amz_date(&amz_date)?;
    if date != amz_date[0..8] {
        return Err(AuthError::InvalidCredentialScope);
    }
    if expires_secs <= 0 || expires_secs > 7 * 24 * 3600 {
        return Err(AuthError::Malformed);
    }
    if now < request_time || now > request_time + Duration::seconds(expires_secs) {
        return Err(AuthError::RequestExpired);
    }

    let cred = credentials
        .get_secret(&access_key)
        .await
        .ok_or(AuthError::UnknownAccessKey)?;
    if !cred.enabled {
        return Err(AuthError::CredentialDisabled);
    }

    // Everything in the query string is part of what's signed *except* the signature
    // itself.
    let signing_query_pairs: Vec<(String, String)> = query_pairs
        .iter()
        .filter(|(k, _)| k != "X-Amz-Signature")
        .cloned()
        .collect();

    let canonical_request = build_canonical_request(
        parts.method,
        parts.path,
        &signing_query_pairs,
        parts.headers,
        &signed_headers,
        "UNSIGNED-PAYLOAD",
    );
    let string_to_sign =
        build_string_to_sign(&amz_date, &date, &cred_region, &service, &canonical_request);
    let expected = compute_signature(
        &cred.secret_key,
        &date,
        &cred_region,
        &service,
        &string_to_sign,
    );

    if !constant_time_eq(&expected, &signature) {
        return Err(AuthError::SignatureMismatch);
    }

    Ok(VerifiedRequest {
        access_key,
        owner_id: cred.owner_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use s3_core::OwnerId;
    use std::sync::Mutex;
    use time::macros::datetime;

    struct FakeCredentials {
        access_key: &'static str,
        secret_key: &'static str,
        owner_id: OwnerId,
        enabled: bool,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CredentialProvider for FakeCredentials {
        async fn get_secret(&self, access_key: &str) -> Option<crate::SigningCredential> {
            self.calls.lock().unwrap().push(access_key.to_string());
            if access_key == self.access_key {
                Some(crate::SigningCredential {
                    secret_key: self.secret_key.to_string(),
                    owner_id: self.owner_id,
                    enabled: self.enabled,
                })
            } else {
                None
            }
        }
    }

    fn creds(enabled: bool) -> FakeCredentials {
        FakeCredentials {
            access_key: "AKIATEST",
            secret_key: "test-secret-key",
            owner_id: OwnerId::new(),
            enabled,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Mirrors what a well-behaved SigV4 client does: sign using the public
    /// [`crate::sign`] API (independent implementation path from `verify` internally,
    /// but exercised through the same public surface a real client would use) and
    /// return the full header set including the resulting `Authorization` header.
    #[allow(clippy::too_many_arguments)]
    fn sign_header_request(
        method: &str,
        path: &str,
        query: &str,
        extra_headers: &[(&str, &str)],
        access_key: &str,
        secret_key: &str,
        region: &str,
        amz_date: &str,
        payload_hash: &str,
    ) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = extra_headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain([
                ("x-amz-date".to_string(), amz_date.to_string()),
                ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
            ])
            .collect();
        let mut signed_headers: Vec<String> =
            headers.iter().map(|(k, _)| k.to_lowercase()).collect();
        signed_headers.sort();

        let auth_value = crate::sign::sign_header_auth(
            method,
            path,
            query,
            &headers,
            &signed_headers,
            payload_hash,
            access_key,
            secret_key,
            region,
            amz_date,
        );
        headers.push(("authorization".to_string(), auth_value));
        headers
    }

    #[tokio::test]
    async fn accepts_a_correctly_signed_request() {
        let creds = creds(true);
        let headers = sign_header_request(
            "GET",
            "/bucket/key",
            "",
            &[("host", "s3.example.com")],
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            "UNSIGNED-PAYLOAD",
        );
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: "",
            headers: &headers,
        };

        let result = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:00:05 UTC),
            Duration::minutes(15),
        )
        .await
        .unwrap();
        assert_eq!(result.access_key, "AKIATEST");
    }

    #[tokio::test]
    async fn rejects_a_tampered_header() {
        let creds = creds(true);
        let mut headers = sign_header_request(
            "GET",
            "/bucket/key",
            "",
            &[("host", "s3.example.com")],
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            "UNSIGNED-PAYLOAD",
        );
        // An attacker (or a buggy proxy) changes the Host header after signing.
        for (k, v) in headers.iter_mut() {
            if k == "host" {
                *v = "attacker.example.com".to_string();
            }
        }
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: "",
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:00:05 UTC),
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::SignatureMismatch);
    }

    #[tokio::test]
    async fn rejects_an_expired_request() {
        let creds = creds(true);
        let headers = sign_header_request(
            "GET",
            "/bucket/key",
            "",
            &[("host", "s3.example.com")],
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            "UNSIGNED-PAYLOAD",
        );
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: "",
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:30:00 UTC), // 30 minutes later
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::RequestExpired);
    }

    #[tokio::test]
    async fn rejects_unknown_access_key() {
        let creds = creds(true);
        let headers = sign_header_request(
            "GET",
            "/bucket/key",
            "",
            &[("host", "s3.example.com")],
            "AKIAWRONG",
            "some-other-secret",
            "us-east-1",
            "20250101T120000Z",
            "UNSIGNED-PAYLOAD",
        );
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: "",
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:00:05 UTC),
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::UnknownAccessKey);
    }

    #[tokio::test]
    async fn rejects_disabled_credential() {
        let creds = creds(false);
        let headers = sign_header_request(
            "GET",
            "/bucket/key",
            "",
            &[("host", "s3.example.com")],
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            "UNSIGNED-PAYLOAD",
        );
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: "",
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:00:05 UTC),
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::CredentialDisabled);
    }

    #[tokio::test]
    async fn rejects_wrong_secret_even_with_right_access_key() {
        let creds = creds(true);
        let headers = sign_header_request(
            "GET",
            "/bucket/key",
            "",
            &[("host", "s3.example.com")],
            "AKIATEST",
            "wrong-secret",
            "us-east-1",
            "20250101T120000Z",
            "UNSIGNED-PAYLOAD",
        );
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: "",
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:00:05 UTC),
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::SignatureMismatch);
    }

    fn sign_presigned_url(
        method: &str,
        path: &str,
        access_key: &str,
        secret_key: &str,
        region: &str,
        amz_date: &str,
        expires: i64,
    ) -> String {
        let headers = vec![("host".to_string(), "s3.example.com".to_string())];
        crate::sign::sign_presigned_query(
            method,
            path,
            &headers,
            &["host".to_string()],
            access_key,
            secret_key,
            region,
            amz_date,
            expires,
        )
    }

    #[tokio::test]
    async fn accepts_a_valid_presigned_url_within_its_window() {
        let creds = creds(true);
        let query = sign_presigned_url(
            "GET",
            "/bucket/key",
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            3600,
        );
        let headers = vec![("host".to_string(), "s3.example.com".to_string())];
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: &query,
            headers: &headers,
        };

        let result = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:30:00 UTC), // 30 minutes into a 1-hour window
            Duration::minutes(15),
        )
        .await
        .unwrap();
        assert_eq!(result.access_key, "AKIATEST");
    }

    #[tokio::test]
    async fn rejects_a_presigned_url_past_its_expiry() {
        let creds = creds(true);
        let query = sign_presigned_url(
            "GET",
            "/bucket/key",
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            3600,
        );
        let headers = vec![("host".to_string(), "s3.example.com".to_string())];
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: &query,
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 13:30:00 UTC), // 1.5 hours later, past the 1-hour window
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::RequestExpired);
    }

    #[tokio::test]
    async fn rejects_a_presigned_url_with_a_tampered_query_param() {
        let creds = creds(true);
        let query = sign_presigned_url(
            "GET",
            "/bucket/key",
            "AKIATEST",
            "test-secret-key",
            "us-east-1",
            "20250101T120000Z",
            3600,
        );
        let tampered = query.replace("X-Amz-Expires=3600", "X-Amz-Expires=360000");
        let headers = vec![("host".to_string(), "s3.example.com".to_string())];
        let parts = RequestParts {
            method: "GET",
            path: "/bucket/key",
            query: &tampered,
            headers: &headers,
        };

        let err = verify(
            &parts,
            "us-east-1",
            &creds,
            datetime!(2025-01-01 12:30:00 UTC),
            Duration::minutes(15),
        )
        .await
        .unwrap_err();
        assert_eq!(err, AuthError::SignatureMismatch);
    }
}
