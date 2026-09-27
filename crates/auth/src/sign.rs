//! SigV4 request *signing* — the client-side counterpart to [`crate::verify`]. Used to
//! generate presigned URLs (prompt §53: "Implement SigV4 presigned: GET, PUT") and, in
//! tests, to construct realistically-signed requests without depending on an external
//! HTTP client library.

use time::OffsetDateTime;

use crate::canonical::{
    SCOPE_TERMINATOR, SERVICE, SIGNING_ALGORITHM, canonical_headers, canonical_query_string,
    canonical_uri, compute_signature, sha256_hex,
};

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
    let signed_headers_line = signed_headers.join(";");
    format!(
        "{method}\n{canonical_uri}\n{canonical_qs}\n{canonical_hdrs}\n{signed_headers_line}\n{payload_hash}"
    )
}

/// The `x-amz-date` value for "now", in the `YYYYMMDDTHHMMSSZ` format SigV4 requires.
pub fn amz_date_now() -> String {
    amz_date(OffsetDateTime::now_utc())
}

pub fn amz_date(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Signs a request and returns the value for its `Authorization` header.
/// `headers` must already include every header that will be signed (typically at
/// least `host`, `x-amz-date`, and `x-amz-content-sha256`), and `signed_headers` names
/// which of them to include in the signature — both, verbatim, as they'll be sent.
#[allow(clippy::too_many_arguments)]
pub fn sign_header_auth(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    signed_headers: &[String],
    payload_hash: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    amz_date: &str,
) -> String {
    let date = &amz_date[0..8];
    let query_pairs = crate::canonical::parse_query_string(query);
    let canonical_request = build_canonical_request(
        method,
        path,
        &query_pairs,
        headers,
        signed_headers,
        payload_hash,
    );
    let string_to_sign = format!(
        "{SIGNING_ALGORITHM}\n{amz_date}\n{date}/{region}/{SERVICE}/{SCOPE_TERMINATOR}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = compute_signature(secret_key, date, region, SERVICE, &string_to_sign);

    format!(
        "{SIGNING_ALGORITHM} Credential={access_key}/{date}/{region}/{SERVICE}/{SCOPE_TERMINATOR}, SignedHeaders={}, Signature={signature}",
        signed_headers.join(";")
    )
}

/// Builds a presigned URL's query string (everything after `?`), including the final
/// `X-Amz-Signature` parameter.
#[allow(clippy::too_many_arguments)]
pub fn sign_presigned_query(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    signed_headers: &[String],
    access_key: &str,
    secret_key: &str,
    region: &str,
    amz_date: &str,
    expires_secs: i64,
) -> String {
    let date = &amz_date[0..8];
    let credential = format!("{access_key}/{date}/{region}/{SERVICE}/{SCOPE_TERMINATOR}");
    let mut query_pairs = vec![
        ("X-Amz-Algorithm".to_string(), SIGNING_ALGORITHM.to_string()),
        ("X-Amz-Credential".to_string(), credential),
        ("X-Amz-Date".to_string(), amz_date.to_string()),
        ("X-Amz-Expires".to_string(), expires_secs.to_string()),
        ("X-Amz-SignedHeaders".to_string(), signed_headers.join(";")),
    ];
    query_pairs.sort();

    let canonical_request = build_canonical_request(
        method,
        path,
        &query_pairs,
        headers,
        signed_headers,
        "UNSIGNED-PAYLOAD",
    );
    let string_to_sign = format!(
        "{SIGNING_ALGORITHM}\n{amz_date}\n{date}/{region}/{SERVICE}/{SCOPE_TERMINATOR}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = compute_signature(secret_key, date, region, SERVICE, &string_to_sign);

    query_pairs.push(("X-Amz-Signature".to_string(), signature));
    query_pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amz_date_formats_correctly() {
        use time::macros::datetime;
        assert_eq!(
            amz_date(datetime!(2025-03-04 05:06:07 UTC)),
            "20250304T050607Z"
        );
    }
}
