//! Hand-rolled S3 response XML. `quick-xml`'s serde integration doesn't cleanly express
//! S3's repeated-element-without-a-wrapper-name shape (`<Contents>...</Contents>`
//! repeated directly inside `<ListBucketResult>`) without fighting its Vec handling, so
//! these are built directly — simpler to get right and to keep matching AWS's exact
//! shape than fighting a serde adapter meant for more regular documents.

use s3_core::{Bucket, ETag, OwnerId};
use s3_metadata::{ListObjectsPage, ObjectSummary};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn error_xml(code: &str, message: &str, request_id: &str, resource: Option<&str>) -> String {
    let mut body = format!(
        "{XML_HEADER}<Error><Code>{}</Code><Message>{}</Message>",
        escape(code),
        escape(message)
    );
    if let Some(resource) = resource {
        body.push_str(&format!("<Resource>{}</Resource>", escape(resource)));
    }
    body.push_str(&format!(
        "<RequestId>{}</RequestId></Error>",
        escape(request_id)
    ));
    body
}

pub fn list_all_my_buckets(buckets: &[Bucket], owner: OwnerId) -> String {
    let mut body = format!(
        "{XML_HEADER}<ListAllMyBucketsResult xmlns=\"{XMLNS}\">\
         <Owner><ID>{}</ID><DisplayName>{}</DisplayName></Owner><Buckets>",
        escape(&owner.to_string()),
        escape(&owner.to_string()),
    );
    for bucket in buckets {
        body.push_str(&format!(
            "<Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>",
            escape(bucket.name.as_str()),
            rfc3339(bucket.created_at),
        ));
    }
    body.push_str("</Buckets></ListAllMyBucketsResult>");
    body
}

pub fn list_bucket_result(
    bucket_name: &str,
    prefix: Option<&str>,
    delimiter: Option<&str>,
    max_keys: u32,
    page: &ListObjectsPage,
) -> String {
    let mut body = format!(
        "{XML_HEADER}<ListBucketResult xmlns=\"{XMLNS}\">\
         <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{}</MaxKeys>",
        escape(bucket_name),
        escape(prefix.unwrap_or("")),
        page.objects.len() + page.common_prefixes.len(),
        max_keys,
    );
    if let Some(delimiter) = delimiter {
        body.push_str(&format!("<Delimiter>{}</Delimiter>", escape(delimiter)));
    }
    body.push_str(&format!("<IsTruncated>{}</IsTruncated>", page.is_truncated));
    if let Some(token) = &page.next_continuation_token {
        body.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            escape(token)
        ));
    }
    for object in &page.objects {
        body.push_str(&object_summary_xml(object));
    }
    for common_prefix in &page.common_prefixes {
        body.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            escape(common_prefix)
        ));
    }
    body.push_str("</ListBucketResult>");
    body
}

fn object_summary_xml(object: &ObjectSummary) -> String {
    format!(
        "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag>\
         <Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
        escape(object.key.as_str()),
        rfc3339(object.last_modified),
        escape(object.etag.as_str()),
        object.size,
    )
}

/// The quoted-ETag convention every S3 client expects on both headers and XML bodies.
pub fn quoted_etag(etag: &ETag) -> String {
    format!("\"{}\"", etag.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_reserved_characters() {
        assert_eq!(escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    #[test]
    fn error_xml_contains_required_elements() {
        let xml = error_xml(
            "NoSuchKey",
            "The specified key does not exist.",
            "req-123",
            Some("/bucket/key"),
        );
        assert!(xml.contains("<Code>NoSuchKey</Code>"));
        assert!(xml.contains("<Message>The specified key does not exist.</Message>"));
        assert!(xml.contains("<RequestId>req-123</RequestId>"));
        assert!(xml.contains("<Resource>/bucket/key</Resource>"));
    }
}
