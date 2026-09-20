//! SigV4 canonicalization primitives (prompt §52). Kept free of any HTTP-framework
//! types so it's testable in complete isolation from `s3-api`/axum.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// SigV4's URI-encode: unreserved characters (`A-Za-z0-9-._~`) pass through, everything
/// else becomes `%XX` (uppercase hex); `/` passes through unless `encode_slash`.
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else if c == '/' && !encode_slash {
            out.push('/');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Canonicalize a request path: percent-decode once (normalizing whatever encoding the
/// client used) then re-encode per SigV4 rules. This is S3's own documented quirk vs.
/// most other AWS services: paths are single-, not double-, encoded.
pub fn canonical_uri(raw_path: &str) -> String {
    let decoded = percent_decode(raw_path);
    let encoded = uri_encode(&decoded, false);
    if encoded.is_empty() {
        "/".to_string()
    } else {
        encoded
    }
}

pub fn parse_query_string(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let k = percent_decode(parts.next().unwrap_or(""));
            let v = percent_decode(parts.next().unwrap_or(""));
            (k, v)
        })
        .collect()
}

pub fn canonical_query_string(pairs: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    encoded.sort();
    encoded
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn normalize_header_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Only the headers named in `signed` are included, one `name:value\n` line each,
/// sorted by (lowercased) header name — independent of the order `signed` lists them in.
pub fn canonical_headers(headers: &[(String, String)], signed: &[String]) -> String {
    let mut lines: Vec<(String, String)> = signed
        .iter()
        .filter_map(|name| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| (name.to_lowercase(), normalize_header_value(v)))
        })
        .collect();
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    lines
        .into_iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect()
}

pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_leaves_unreserved_chars_alone() {
        assert_eq!(uri_encode("abcXYZ019-._~", false), "abcXYZ019-._~");
    }

    #[test]
    fn uri_encode_escapes_everything_else() {
        assert_eq!(uri_encode("a b", true), "a%20b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("a/b", false), "a/b");
    }

    #[test]
    fn percent_decode_roundtrips() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("no-escapes"), "no-escapes");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }

    #[test]
    fn canonical_query_string_sorts_by_key() {
        let pairs = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ];
        assert_eq!(canonical_query_string(&pairs), "a=1&b=2");
    }

    #[test]
    fn canonical_headers_are_sorted_and_only_include_signed_ones() {
        let headers = vec![
            ("Host".to_string(), "example.com".to_string()),
            ("X-Amz-Date".to_string(), "20250101T000000Z".to_string()),
            ("X-Ignored".to_string(), "nope".to_string()),
        ];
        let signed = vec!["x-amz-date".to_string(), "host".to_string()];
        assert_eq!(
            canonical_headers(&headers, &signed),
            "host:example.com\nx-amz-date:20250101T000000Z\n"
        );
    }

    #[test]
    fn constant_time_eq_matches_string_eq_semantics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
