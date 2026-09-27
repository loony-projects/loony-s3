//! Opaque `ListObjectsV2` continuation tokens. A token is the hex encoding of the
//! bucket-relative key to resume from -- never the internal row key, which embeds the
//! bucket id and a NUL separator. NUL can't appear anywhere in an XML 1.0 document, so
//! handing the row key back verbatim made every truncated listing unparseable.

pub(crate) fn encode(logical_key: &str) -> String {
    logical_key.bytes().map(|b| format!("{b:02x}")).collect()
}

/// `None` for anything that isn't a token `encode` could have produced.
pub(crate) fn decode(token: &str) -> Option<String> {
    if !token.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..token.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(token.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_xml_safe() {
        let key = "photos/2025/ünïcode & <brackets>.jpg";
        let token = encode(key);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(decode(&token).as_deref(), Some(key));
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert_eq!(decode("abc"), None); // odd length
        assert_eq!(decode("zz"), None); // not hex
        assert_eq!(decode("ff"), None); // not UTF-8
        assert_eq!(decode("éé"), None); // not ASCII
    }
}
