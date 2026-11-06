//! Chunk signatures for SigV4 streaming uploads (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD` and
//! its `-TRAILER` variant). Each chunk's signature covers the one before it, starting
//! from the signature on the request's headers, so chunks can't be dropped or reordered.

use std::fmt;

use crate::canonical::{
    SCOPE_TERMINATOR, SERVICE, constant_time_eq, hex, hmac_sha256, sha256_hex, signing_key,
};

pub(crate) const STREAMING_PREFIX: &str = "STREAMING-";

const CHUNK_ALGORITHM: &str = "AWS4-HMAC-SHA256-PAYLOAD";
const TRAILER_ALGORITHM: &str = "AWS4-HMAC-SHA256-TRAILER";

#[derive(Clone)]
pub struct ChunkSigner {
    key: Vec<u8>,
    amz_date: String,
    scope: String,
    previous_signature: String,
}

/// Hand-written so the derived signing key can't end up in a log line.
impl fmt::Debug for ChunkSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkSigner")
            .field("amz_date", &self.amz_date)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ChunkSigner {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.amz_date == other.amz_date
            && self.scope == other.scope
            && self.previous_signature == other.previous_signature
    }
}

impl Eq for ChunkSigner {}

impl ChunkSigner {
    /// `amz_date` must already be validated as `YYYYMMDDTHHMMSSZ`.
    pub fn new(
        secret_key: &str,
        amz_date: &str,
        region: &str,
        seed_signature: impl Into<String>,
    ) -> Self {
        let date = &amz_date[0..8];
        Self {
            key: signing_key(secret_key, date, region, SERVICE),
            amz_date: amz_date.to_string(),
            scope: format!("{date}/{region}/{SERVICE}/{SCOPE_TERMINATOR}"),
            previous_signature: seed_signature.into(),
        }
    }

    pub fn verify_chunk(&mut self, chunk_sha256_hex: &str, claimed: &str) -> bool {
        let string_to_sign = format!(
            "{CHUNK_ALGORITHM}\n{}\n{}\n{}\n{}\n{chunk_sha256_hex}",
            self.amz_date,
            self.scope,
            self.previous_signature,
            sha256_hex(b""),
        );
        self.check(&string_to_sign, claimed)
    }

    /// `trailer_sha256_hex` is the hash of the trailer in canonical form:
    /// `lowercase-name:value\n`.
    pub fn verify_trailer(&mut self, trailer_sha256_hex: &str, claimed: &str) -> bool {
        let string_to_sign = format!(
            "{TRAILER_ALGORITHM}\n{}\n{}\n{}\n{trailer_sha256_hex}",
            self.amz_date, self.scope, self.previous_signature,
        );
        self.check(&string_to_sign, claimed)
    }

    /// The signature `verify_chunk` would accept for this chunk. Doesn't advance the
    /// chain; `verify_chunk` does that.
    pub fn sign_chunk(&self, chunk_sha256_hex: &str) -> String {
        let string_to_sign = format!(
            "{CHUNK_ALGORITHM}\n{}\n{}\n{}\n{}\n{chunk_sha256_hex}",
            self.amz_date,
            self.scope,
            self.previous_signature,
            sha256_hex(b""),
        );
        hex(&hmac_sha256(&self.key, string_to_sign.as_bytes()))
    }

    /// The signature `verify_trailer` would accept for this trailer.
    pub fn sign_trailer(&self, trailer_sha256_hex: &str) -> String {
        let string_to_sign = format!(
            "{TRAILER_ALGORITHM}\n{}\n{}\n{}\n{trailer_sha256_hex}",
            self.amz_date, self.scope, self.previous_signature,
        );
        hex(&hmac_sha256(&self.key, string_to_sign.as_bytes()))
    }

    fn check(&mut self, string_to_sign: &str, claimed: &str) -> bool {
        let expected = hex(&hmac_sha256(&self.key, string_to_sign.as_bytes()));
        if !constant_time_eq(&expected, claimed) {
            return false;
        }
        self.previous_signature = expected;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The worked example in AWS's "Signature Calculations for the Authorization Header:
    // Transferring Payload in Multiple Chunks" documentation.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const SEED: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";

    fn signer() -> ChunkSigner {
        ChunkSigner::new(SECRET, "20130524T000000Z", "us-east-1", SEED)
    }

    #[test]
    fn matches_the_published_aws_example() {
        let mut signer = signer();
        assert!(signer.verify_chunk(
            &sha256_hex(&vec![b'a'; 65536]),
            "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648",
        ));
        assert!(signer.verify_chunk(
            &sha256_hex(&vec![b'a'; 1024]),
            "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497",
        ));
        assert!(signer.verify_chunk(
            &sha256_hex(b""),
            "b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9",
        ));
    }

    #[test]
    fn a_chunk_out_of_order_fails() {
        let mut signer = signer();
        assert!(!signer.verify_chunk(
            &sha256_hex(&vec![b'a'; 1024]),
            "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497",
        ));
    }

    #[test]
    fn debug_output_leaves_out_the_signing_key() {
        let printed = format!("{:?}", signer());
        assert!(!printed.contains("key"));
    }
}
