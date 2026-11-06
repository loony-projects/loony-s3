//! Turns an upload's request body into the object's actual bytes, checking whatever the
//! client declared about them along the way.
//!
//! `x-amz-content-sha256` says how the body is encoded. Newer SDKs frame it as
//! `aws-chunked` by default over HTTPS, with a checksum trailer at the end; those framing
//! bytes must never reach storage. A mode this server doesn't know is refused rather than
//! stored as-is, since storing it would silently save something other than the object.
//!
//! A failed check ends the stream with an error, which stops the upload before anything
//! is committed. The stream's error type can only carry an `io::Error`, so the real
//! reason is left in [`PayloadFailure`] for the handler to report.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::{Bytes, BytesMut};
use crc::{CRC_32_ISCSI, CRC_32_ISO_HDLC, CRC_64_NVME, Crc, Digest as CrcDigest, Table};
use futures::{Stream, StreamExt};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use loony_auth::ChunkSigner;
use loony_object::Ls3Error;
use loony_storage::ShardBytesIn;

const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const UNSIGNED_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
const SIGNED_CHUNKS: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
const SIGNED_CHUNKS_TRAILER: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER";

const TRAILER_SIGNATURE: &str = "x-amz-trailer-signature";

/// Well above any real chunk header or trailer line; bounds what a client can make the
/// decoder hold while it looks for a line ending.
const MAX_LINE: usize = 1024;

static CRC32: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISO_HDLC);
static CRC32C: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);
static CRC64NVME: Crc<u64, Table<16>> = Crc::<u64, Table<16>>::new(&CRC_64_NVME);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadError {
    Unsupported(String),
    Incomplete(String),
    BadDigest(String),
    BadSignature,
}

impl From<PayloadError> for Ls3Error {
    fn from(error: PayloadError) -> Self {
        match error {
            PayloadError::Unsupported(reason) => Ls3Error::NotImplemented(reason),
            PayloadError::Incomplete(reason) => Ls3Error::IncompleteBody(reason),
            PayloadError::BadDigest(reason) => Ls3Error::BadDigest(reason),
            PayloadError::BadSignature => Ls3Error::SignatureDoesNotMatch,
        }
    }
}

/// Where the stream leaves the reason it failed.
#[derive(Clone, Default)]
pub struct PayloadFailure(Arc<Mutex<Option<PayloadError>>>);

impl PayloadFailure {
    fn record(&self, error: PayloadError) {
        *self.0.lock().expect("payload failure lock poisoned") = Some(error);
    }

    /// The error to report for a failed upload: the body's own problem if it had one,
    /// otherwise whatever the upload failed with.
    pub fn explain(&self, error: Ls3Error) -> Ls3Error {
        self.0
            .lock()
            .expect("payload failure lock poisoned")
            .take()
            .map_or(error, Into::into)
    }
}

enum Checksum {
    Crc32(CrcDigest<'static, u32, Table<16>>),
    Crc32c(CrcDigest<'static, u32, Table<16>>),
    Crc64Nvme(CrcDigest<'static, u64, Table<16>>),
    Sha1(Sha1),
    Sha256(Sha256),
}

impl Checksum {
    fn for_header(name: &str) -> Option<Self> {
        Some(match name {
            "x-amz-checksum-crc32" => Checksum::Crc32(CRC32.digest()),
            "x-amz-checksum-crc32c" => Checksum::Crc32c(CRC32C.digest()),
            "x-amz-checksum-crc64nvme" => Checksum::Crc64Nvme(CRC64NVME.digest()),
            "x-amz-checksum-sha1" => Checksum::Sha1(Sha1::new()),
            "x-amz-checksum-sha256" => Checksum::Sha256(Sha256::new()),
            _ => return None,
        })
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Checksum::Crc32(digest) | Checksum::Crc32c(digest) => digest.update(data),
            Checksum::Crc64Nvme(digest) => digest.update(data),
            Checksum::Sha1(digest) => digest.update(data),
            Checksum::Sha256(digest) => digest.update(data),
        }
    }

    fn finish_base64(self) -> String {
        match self {
            Checksum::Crc32(digest) | Checksum::Crc32c(digest) => {
                BASE64.encode(digest.finalize().to_be_bytes())
            }
            Checksum::Crc64Nvme(digest) => BASE64.encode(digest.finalize().to_be_bytes()),
            Checksum::Sha1(digest) => BASE64.encode(digest.finalize()),
            Checksum::Sha256(digest) => BASE64.encode(digest.finalize()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Checks made over the object's decoded bytes.
#[derive(Default)]
struct BodyChecks {
    sha256: Option<(Sha256, String)>,
    checksum: Option<(Checksum, &'static str, Option<String>)>,
}

impl BodyChecks {
    fn update(&mut self, data: &[u8]) {
        if let Some((digest, _)) = &mut self.sha256 {
            digest.update(data);
        }
        if let Some((digest, _, _)) = &mut self.checksum {
            digest.update(data);
        }
    }

    fn finish(&mut self, trailer: Option<String>) -> Result<(), PayloadError> {
        if let Some((digest, expected)) = self.sha256.take()
            && !hex(&digest.finalize()).eq_ignore_ascii_case(&expected)
        {
            return Err(PayloadError::BadDigest(
                "The body does not match x-amz-content-sha256".into(),
            ));
        }
        if let Some((digest, name, from_header)) = self.checksum.take() {
            let Some(expected) = from_header.or(trailer) else {
                return Err(PayloadError::Incomplete(format!(
                    "The body ended without the declared {name} trailer"
                )));
            };
            if digest.finish_base64() != expected {
                return Err(PayloadError::BadDigest(format!(
                    "The body does not match {name}"
                )));
            }
        }
        Ok(())
    }
}

enum DecodeState {
    ChunkHeader,
    ChunkData { remaining: u64 },
    ChunkEnd,
    Trailer,
    FinalLineEnd,
    Done,
}

/// Parses `aws-chunked` framing. Bytes arrive however the network split them, so every
/// state stops and waits when it hasn't got enough yet.
struct ChunkDecoder {
    signer: Option<ChunkSigner>,
    /// Whether this mode ends in trailers rather than just a closing line end.
    has_trailer: bool,
    declared_trailer: Option<&'static str>,
    state: DecodeState,
    buf: BytesMut,
    chunk_hash: Sha256,
    chunk_signature: String,
    decoded_len: u64,
    declared_len: Option<u64>,
    trailer_value: Option<String>,
    trailer_signed: bool,
}

fn malformed(detail: &str) -> PayloadError {
    PayloadError::Incomplete(format!("Malformed aws-chunked body: {detail}"))
}

impl ChunkDecoder {
    fn new(
        signer: Option<ChunkSigner>,
        has_trailer: bool,
        declared_trailer: Option<&'static str>,
        declared_len: Option<u64>,
    ) -> Self {
        Self {
            signer,
            has_trailer,
            declared_trailer,
            state: DecodeState::ChunkHeader,
            buf: BytesMut::new(),
            chunk_hash: Sha256::new(),
            chunk_signature: String::new(),
            decoded_len: 0,
            declared_len,
            trailer_value: None,
            trailer_signed: false,
        }
    }

    /// Takes one line off the front of the buffer, without its line ending.
    fn take_line(&mut self) -> Result<Option<String>, PayloadError> {
        let Some(end) = self.buf.windows(2).position(|pair| pair == b"\r\n") else {
            if self.buf.len() > MAX_LINE {
                return Err(malformed("line too long"));
            }
            return Ok(None);
        };
        if end > MAX_LINE {
            return Err(malformed("line too long"));
        }
        let line = self.buf.split_to(end);
        let _ = self.buf.split_to(2);
        String::from_utf8(line.to_vec())
            .map(Some)
            .map_err(|_| malformed("line is not valid text"))
    }

    fn start_chunk(&mut self, line: &str) -> Result<(), PayloadError> {
        let (size, extensions) = line.split_once(';').unwrap_or((line, ""));
        let size = u64::from_str_radix(size, 16).map_err(|_| malformed("bad chunk size"))?;

        if self.signer.is_some() {
            self.chunk_signature = extensions
                .split(';')
                .find_map(|ext| ext.strip_prefix("chunk-signature="))
                .ok_or_else(|| malformed("chunk has no signature"))?
                .to_string();
        }

        let total = self
            .decoded_len
            .checked_add(size)
            .ok_or_else(|| malformed("bad chunk size"))?;
        if self.declared_len.is_some_and(|declared| total > declared) {
            return Err(malformed("more data than x-amz-decoded-content-length"));
        }
        self.decoded_len = total;

        self.state = if size == 0 {
            self.verify_chunk_signature()?;
            if self.has_trailer {
                DecodeState::Trailer
            } else {
                DecodeState::FinalLineEnd
            }
        } else {
            DecodeState::ChunkData { remaining: size }
        };
        Ok(())
    }

    fn verify_chunk_signature(&mut self) -> Result<(), PayloadError> {
        let hash = hex(&std::mem::take(&mut self.chunk_hash).finalize());
        if let Some(signer) = &mut self.signer
            && !signer.verify_chunk(&hash, &self.chunk_signature)
        {
            return Err(PayloadError::BadSignature);
        }
        Ok(())
    }

    fn read_trailer(&mut self, line: &str) -> Result<(), PayloadError> {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed("bad trailer"))?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();

        if name == TRAILER_SIGNATURE {
            let (Some(signer), Some(declared), Some(checksum)) =
                (&mut self.signer, self.declared_trailer, &self.trailer_value)
            else {
                return Err(malformed("unexpected trailer signature"));
            };
            let canonical = format!("{declared}:{checksum}\n");
            let hash = hex(&Sha256::digest(canonical.as_bytes()));
            if !signer.verify_trailer(&hash, value) {
                return Err(PayloadError::BadSignature);
            }
            self.trailer_signed = true;
            return Ok(());
        }

        if self.declared_trailer != Some(name.as_str()) || self.trailer_value.is_some() {
            return Err(malformed("trailer was not declared in x-amz-trailer"));
        }
        self.trailer_value = Some(value.to_string());
        Ok(())
    }

    /// Consumes `input`, pushing any object bytes it completes onto `out`.
    fn push(&mut self, input: &[u8], out: &mut VecDeque<Bytes>) -> Result<(), PayloadError> {
        self.buf.extend_from_slice(input);
        loop {
            match self.state {
                DecodeState::ChunkHeader => match self.take_line()? {
                    Some(line) => self.start_chunk(&line)?,
                    None => return Ok(()),
                },
                DecodeState::ChunkData { remaining } => {
                    if self.buf.is_empty() {
                        return Ok(());
                    }
                    let take = remaining.min(self.buf.len() as u64);
                    let data = self.buf.split_to(take as usize).freeze();
                    if self.signer.is_some() {
                        self.chunk_hash.update(&data);
                    }
                    out.push_back(data);
                    self.state = if take == remaining {
                        self.verify_chunk_signature()?;
                        DecodeState::ChunkEnd
                    } else {
                        DecodeState::ChunkData {
                            remaining: remaining - take,
                        }
                    };
                }
                DecodeState::ChunkEnd => {
                    if self.buf.len() < 2 {
                        return Ok(());
                    }
                    if &self.buf[..2] != b"\r\n" {
                        return Err(malformed("chunk is longer than its declared size"));
                    }
                    let _ = self.buf.split_to(2);
                    self.state = DecodeState::ChunkHeader;
                }
                DecodeState::Trailer => match self.take_line()? {
                    Some(line) if line.is_empty() => self.state = DecodeState::Done,
                    Some(line) => self.read_trailer(&line)?,
                    None => return Ok(()),
                },
                DecodeState::FinalLineEnd => {
                    if self.buf.len() < 2 {
                        return Ok(());
                    }
                    if &self.buf[..2] != b"\r\n" {
                        return Err(malformed("data after the final chunk"));
                    }
                    let _ = self.buf.split_to(2);
                    self.state = DecodeState::Done;
                }
                DecodeState::Done => {
                    return if self.buf.is_empty() {
                        Ok(())
                    } else {
                        Err(malformed("data after the end of the body"))
                    };
                }
            }
        }
    }

    /// Called at the end of the body. Returns the trailer's checksum, if it carried one.
    fn finish(&mut self) -> Result<Option<String>, PayloadError> {
        // Some clients stop right after the last chunk or trailer, without the closing
        // blank line. Nothing is lost by that, so only a body that stopped earlier fails.
        let complete = match self.state {
            DecodeState::Done | DecodeState::FinalLineEnd => true,
            DecodeState::Trailer => self.buf.is_empty(),
            _ => false,
        };
        if !complete || (matches!(self.state, DecodeState::FinalLineEnd) && !self.buf.is_empty()) {
            return Err(PayloadError::Incomplete(
                "The body ended before its final chunk".into(),
            ));
        }
        if self.signer.is_some() && self.declared_trailer.is_some() && !self.trailer_signed {
            return Err(PayloadError::BadSignature);
        }
        if self
            .declared_len
            .is_some_and(|declared| declared != self.decoded_len)
        {
            return Err(PayloadError::Incomplete(
                "The body does not match x-amz-decoded-content-length".into(),
            ));
        }
        Ok(self.trailer_value.take())
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

const CHECKSUM_HEADERS: [&str; 5] = [
    "x-amz-checksum-crc32",
    "x-amz-checksum-crc32c",
    "x-amz-checksum-crc64nvme",
    "x-amz-checksum-sha1",
    "x-amz-checksum-sha256",
];

/// Works out from the headers how to read the body and what to check it against.
fn plan(
    headers: &HeaderMap,
    signer: Option<ChunkSigner>,
) -> Result<(Option<ChunkDecoder>, BodyChecks), PayloadError> {
    let content_sha256 = header(headers, "x-amz-content-sha256").map(str::trim);
    let aws_chunked = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|encoding| encoding.trim().eq_ignore_ascii_case("aws-chunked"));
    let trailer = header(headers, "x-amz-trailer").map(str::trim);

    let mut checks = BodyChecks::default();

    let (chunked, has_trailer) = match content_sha256 {
        Some(UNSIGNED_TRAILER) | Some(SIGNED_CHUNKS_TRAILER) => (true, true),
        Some(SIGNED_CHUNKS) => (true, false),
        None | Some(UNSIGNED_PAYLOAD) => (false, false),
        Some(value) if is_sha256_hex(value) => {
            checks.sha256 = Some((Sha256::new(), value.to_string()));
            (false, false)
        }
        Some(value) => {
            return Err(PayloadError::Unsupported(format!(
                "Unsupported x-amz-content-sha256 value: {value}"
            )));
        }
    };

    if !chunked && aws_chunked {
        return Err(PayloadError::Unsupported(
            "Content-Encoding: aws-chunked needs a STREAMING x-amz-content-sha256 value".into(),
        ));
    }
    if trailer.is_some() && !has_trailer {
        return Err(PayloadError::Unsupported(
            "x-amz-trailer needs a -TRAILER x-amz-content-sha256 value".into(),
        ));
    }

    let mut header_checksums = CHECKSUM_HEADERS
        .iter()
        .filter_map(|name| Some((*name, header(headers, name)?)));
    let header_checksum = header_checksums.next();
    if header_checksums.next().is_some() {
        return Err(PayloadError::Unsupported(
            "Only one x-amz-checksum header is allowed".into(),
        ));
    }

    let declared_trailer = match trailer {
        None => None,
        Some(name) => {
            let name = name.to_ascii_lowercase();
            let known = CHECKSUM_HEADERS.iter().find(|known| **known == name);
            match known {
                Some(known) => Some(*known),
                None => {
                    return Err(PayloadError::Unsupported(format!(
                        "Unsupported x-amz-trailer value: {name}"
                    )));
                }
            }
        }
    };
    if declared_trailer.is_some() && header_checksum.is_some() {
        return Err(PayloadError::Unsupported(
            "A checksum can be sent as a header or a trailer, not both".into(),
        ));
    }

    if let Some((name, value)) = header_checksum {
        let digest = Checksum::for_header(name).expect("every name in the list has a digest");
        checks.checksum = Some((digest, name, Some(value.trim().to_string())));
    } else if let Some(name) = declared_trailer {
        let digest = Checksum::for_header(name).expect("every name in the list has a digest");
        checks.checksum = Some((digest, name, None));
    }

    if !chunked {
        return Ok((None, checks));
    }

    let signed = content_sha256 != Some(UNSIGNED_TRAILER);
    let signer = match (signed, signer) {
        (true, Some(signer)) => Some(signer),
        (true, None) => {
            return Err(PayloadError::Unsupported(
                "Signed chunks need an Authorization header".into(),
            ));
        }
        (false, _) => None,
    };
    let declared_len = match header(headers, "x-amz-decoded-content-length") {
        Some(value) => Some(value.trim().parse::<u64>().map_err(|_| {
            PayloadError::Unsupported("Invalid x-amz-decoded-content-length".into())
        })?),
        None => None,
    };

    Ok((
        Some(ChunkDecoder::new(
            signer,
            has_trailer,
            declared_trailer,
            declared_len,
        )),
        checks,
    ))
}

struct StreamState<S> {
    inner: S,
    decoder: Option<ChunkDecoder>,
    checks: BodyChecks,
    pending: VecDeque<Bytes>,
    failure: PayloadFailure,
    finished: bool,
}

impl<S> StreamState<S> {
    fn absorb(&mut self, input: Bytes) -> Result<(), PayloadError> {
        let already_pending = self.pending.len();
        match &mut self.decoder {
            Some(decoder) => decoder.push(&input, &mut self.pending)?,
            None => self.pending.push_back(input),
        }
        for data in self.pending.iter().skip(already_pending) {
            self.checks.update(data);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), PayloadError> {
        let trailer = match &mut self.decoder {
            Some(decoder) => decoder.finish()?,
            None => None,
        };
        self.checks.finish(trailer)
    }
}

fn decode_stream<S>(state: StreamState<S>) -> impl Stream<Item = std::io::Result<Bytes>> + Send
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin + Send,
{
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(data) = state.pending.pop_front() {
                return Some((Ok(data), state));
            }
            if state.finished {
                return None;
            }
            let step = match state.inner.next().await {
                Some(Ok(input)) => state.absorb(input),
                Some(Err(error)) => {
                    state.finished = true;
                    return Some((Err(error), state));
                }
                None => {
                    state.finished = true;
                    state.finish()
                }
            };
            if let Err(error) = step {
                state.failure.record(error.clone());
                state.finished = true;
                state.pending.clear();
                return Some((Err(std::io::Error::other(format!("{error:?}"))), state));
            }
        }
    })
}

/// Wraps an upload's body so it yields the object's real bytes and fails if any check
/// fails. Errors returned here mean the headers alone already rule the upload out.
pub fn decode_body(
    headers: &HeaderMap,
    signer: Option<ChunkSigner>,
    body: Body,
) -> Result<(ShardBytesIn, PayloadFailure), PayloadError> {
    let (decoder, checks) = plan(headers, signer)?;
    let failure = PayloadFailure::default();
    let inner = body
        .into_data_stream()
        .map(|item| item.map_err(std::io::Error::other));
    let stream = decode_stream(StreamState {
        inner,
        decoder,
        checks,
        pending: VecDeque::new(),
        failure: failure.clone(),
        finished: false,
    });
    Ok((Box::pin(stream), failure))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const AMZ_DATE: &str = "20130524T000000Z";
    const SEED: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";

    fn signer() -> ChunkSigner {
        ChunkSigner::new(SECRET, AMZ_DATE, "us-east-1", SEED)
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    fn crc32_base64(data: &[u8]) -> String {
        let mut digest = Checksum::for_header("x-amz-checksum-crc32").unwrap();
        digest.update(data);
        digest.finish_base64()
    }

    /// What boto3 sent in the failure this module exists to fix.
    fn unsigned_body(data: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut body = Vec::new();
        for chunk in data.chunks(chunk_size) {
            body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            body.extend_from_slice(chunk);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(
            format!("0\r\nx-amz-checksum-crc32:{}\r\n\r\n", crc32_base64(data)).as_bytes(),
        );
        body
    }

    fn signed_body(data: &[u8], chunk_size: usize, trailer: bool) -> Vec<u8> {
        let mut signer = signer();
        let mut body = Vec::new();
        for chunk in data.chunks(chunk_size).chain(std::iter::once(&b""[..])) {
            let hash = sha256_hex(chunk);
            let signature = signer.sign_chunk(&hash);
            assert!(signer.verify_chunk(&hash, &signature));
            body.extend_from_slice(
                format!("{:x};chunk-signature={signature}\r\n", chunk.len()).as_bytes(),
            );
            if !chunk.is_empty() {
                body.extend_from_slice(chunk);
                body.extend_from_slice(b"\r\n");
            }
        }
        if trailer {
            let checksum = crc32_base64(data);
            let hash = sha256_hex(format!("x-amz-checksum-crc32:{checksum}\n").as_bytes());
            let signature = signer.sign_trailer(&hash);
            body.extend_from_slice(
                format!(
                    "x-amz-checksum-crc32:{checksum}\r\nx-amz-trailer-signature:{signature}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(b"\r\n");
        body
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    /// Feeds the body in pieces of `piece` bytes, so framing lands across every kind of
    /// boundary, and returns the decoded bytes.
    async fn run(
        headers: &HeaderMap,
        signer: Option<ChunkSigner>,
        body: Vec<u8>,
        piece: usize,
    ) -> Result<Vec<u8>, PayloadError> {
        let pieces: Vec<std::io::Result<Bytes>> = body
            .chunks(piece.max(1))
            .map(|part| Ok(Bytes::copy_from_slice(part)))
            .collect();
        let body = Body::from_stream(futures::stream::iter(pieces));
        let (mut stream, failure) = decode_body(headers, signer, body)?;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(data) => out.extend_from_slice(&data),
                Err(_) => {
                    let error = failure.0.lock().unwrap().take();
                    return Err(error.expect("a failed stream records why"));
                }
            }
        }
        Ok(out)
    }

    fn unsigned_headers() -> HeaderMap {
        headers(&[
            ("x-amz-content-sha256", UNSIGNED_TRAILER),
            ("content-encoding", "aws-chunked"),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ])
    }

    #[tokio::test]
    async fn the_exact_bytes_boto3_sent_decode_to_the_original() {
        let body = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:NhCmhg==\r\n\r\n".to_vec();
        let decoded = run(&unsigned_headers(), None, body, 4096).await.unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[tokio::test]
    async fn framing_split_at_any_point_still_decodes() {
        let data: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let body = unsigned_body(&data, 700);
        for piece in [1, 2, 3, 5, 7, 64, 701, 4096] {
            let decoded = run(&unsigned_headers(), None, body.clone(), piece)
                .await
                .unwrap();
            assert_eq!(decoded, data, "pieces of {piece} bytes");
        }
    }

    #[tokio::test]
    async fn an_empty_object_decodes_to_nothing() {
        let decoded = run(&unsigned_headers(), None, unsigned_body(b"", 10), 3)
            .await
            .unwrap();
        assert!(decoded.is_empty());
    }

    #[tokio::test]
    async fn a_wrong_trailer_checksum_is_rejected() {
        let body = b"5\r\nhello\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n".to_vec();
        let error = run(&unsigned_headers(), None, body, 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::BadDigest(_)));
    }

    #[tokio::test]
    async fn a_missing_declared_trailer_is_rejected() {
        let body = b"5\r\nhello\r\n0\r\n\r\n".to_vec();
        let error = run(&unsigned_headers(), None, body, 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Incomplete(_)));
    }

    #[tokio::test]
    async fn a_body_cut_off_at_any_point_is_rejected() {
        let body = unsigned_body(b"hello world", 4);
        // Everything short of the checksum itself: past that, only the closing line end
        // is missing, and the checksum still proves the data arrived whole.
        let checksum_at = body.windows(2).rposition(|pair| pair == b"0\r").unwrap() + 3;
        for cut in 0..checksum_at {
            let error = run(&unsigned_headers(), None, body[..cut].to_vec(), 4096).await;
            assert!(error.is_err(), "accepted a body cut at byte {cut}");
        }
    }

    #[tokio::test]
    async fn a_chunk_longer_than_its_size_is_rejected() {
        let body = b"3\r\nhello\r\n0\r\n\r\n".to_vec();
        let error = run(&unsigned_headers(), None, body, 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Incomplete(_)));
    }

    #[tokio::test]
    async fn an_endless_header_line_is_rejected() {
        let error = run(&unsigned_headers(), None, vec![b'1'; MAX_LINE + 10], 64)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Incomplete(_)));
    }

    #[tokio::test]
    async fn the_declared_decoded_length_is_enforced() {
        let mut too_short = unsigned_headers();
        too_short.insert("x-amz-decoded-content-length", "4".parse().unwrap());
        let error = run(&too_short, None, unsigned_body(b"hello", 10), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Incomplete(_)));

        let mut too_long = unsigned_headers();
        too_long.insert("x-amz-decoded-content-length", "6".parse().unwrap());
        let error = run(&too_long, None, unsigned_body(b"hello", 10), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Incomplete(_)));
    }

    #[tokio::test]
    async fn signed_chunks_decode_and_tampering_is_caught() {
        let headers = headers(&[("x-amz-content-sha256", SIGNED_CHUNKS)]);
        let data = vec![b'a'; 66560];
        let body = signed_body(&data, 65536, false);
        for piece in [1000, 4096, 100_000] {
            let decoded = run(&headers, Some(signer()), body.clone(), piece)
                .await
                .unwrap();
            assert_eq!(decoded, data);
        }

        let mut tampered = body.clone();
        let middle = tampered.len() / 2;
        tampered[middle] = b'b';
        let error = run(&headers, Some(signer()), tampered, 4096)
            .await
            .unwrap_err();
        assert_eq!(error, PayloadError::BadSignature);
    }

    #[tokio::test]
    async fn dropping_a_signed_chunk_is_caught() {
        let headers = headers(&[("x-amz-content-sha256", SIGNED_CHUNKS)]);
        let body = signed_body(b"aaaabbbbcccc", 4, false);
        // Remove the middle chunk's whole framing, leaving the others intact.
        let text = String::from_utf8(body).unwrap();
        let chunks: Vec<&str> = text.split_inclusive("\r\n").collect();
        let without_second = [&chunks[..2], &chunks[4..]].concat().concat();
        let error = run(&headers, Some(signer()), without_second.into_bytes(), 4096)
            .await
            .unwrap_err();
        assert_eq!(error, PayloadError::BadSignature);
    }

    #[tokio::test]
    async fn signed_chunks_need_a_signer() {
        let headers = headers(&[("x-amz-content-sha256", SIGNED_CHUNKS)]);
        let error = run(&headers, None, signed_body(b"x", 4, false), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Unsupported(_)));
    }

    #[tokio::test]
    async fn signed_trailers_are_verified() {
        let headers = headers(&[
            ("x-amz-content-sha256", SIGNED_CHUNKS_TRAILER),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ]);
        let body = signed_body(b"hello world", 4, true);
        let decoded = run(&headers, Some(signer()), body.clone(), 7)
            .await
            .unwrap();
        assert_eq!(decoded, b"hello world");

        let text = String::from_utf8(body).unwrap();
        let start = text.find("x-amz-trailer-signature:").unwrap() + 24;
        let mut tampered = text.into_bytes();
        tampered[start] = if tampered[start] == b'0' { b'1' } else { b'0' };
        let error = run(&headers, Some(signer()), tampered, 4096)
            .await
            .unwrap_err();
        assert_eq!(error, PayloadError::BadSignature);
    }

    #[tokio::test]
    async fn a_signed_trailer_without_its_signature_is_rejected() {
        let headers = headers(&[
            ("x-amz-content-sha256", SIGNED_CHUNKS_TRAILER),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ]);
        let body = signed_body(b"hello", 10, false);
        let mut body = body[..body.len() - 2].to_vec();
        body.extend_from_slice(
            format!("x-amz-checksum-crc32:{}\r\n\r\n", crc32_base64(b"hello")).as_bytes(),
        );
        let error = run(&headers, Some(signer()), body, 4096).await.unwrap_err();
        assert_eq!(error, PayloadError::BadSignature);
    }

    #[tokio::test]
    async fn unknown_modes_are_refused_not_stored() {
        for mode in [
            "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD",
            "STREAMING-SOMETHING-NEW",
            "not-a-hash",
        ] {
            let headers = headers(&[("x-amz-content-sha256", mode)]);
            let error = run(&headers, None, b"data".to_vec(), 4096)
                .await
                .unwrap_err();
            assert!(matches!(error, PayloadError::Unsupported(_)), "{mode}");
        }
    }

    #[tokio::test]
    async fn aws_chunked_without_a_streaming_mode_is_refused() {
        let headers = headers(&[
            ("x-amz-content-sha256", UNSIGNED_PAYLOAD),
            ("content-encoding", "aws-chunked"),
        ]);
        let error = run(&headers, None, b"5\r\nhello\r\n0\r\n\r\n".to_vec(), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Unsupported(_)));
    }

    #[tokio::test]
    async fn an_unknown_trailer_is_refused() {
        let headers = headers(&[
            ("x-amz-content-sha256", UNSIGNED_TRAILER),
            ("x-amz-trailer", "x-amz-checksum-md9"),
        ]);
        let error = run(&headers, None, unsigned_body(b"x", 4), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Unsupported(_)));
    }

    #[tokio::test]
    async fn plain_bodies_pass_through_and_a_real_hash_is_checked() {
        let plain = headers(&[("x-amz-content-sha256", UNSIGNED_PAYLOAD)]);
        assert_eq!(
            run(&plain, None, b"hello".to_vec(), 2).await.unwrap(),
            b"hello"
        );
        assert_eq!(
            run(&HeaderMap::new(), None, b"hello".to_vec(), 2)
                .await
                .unwrap(),
            b"hello"
        );

        let hash = sha256_hex(b"hello");
        let good = headers(&[("x-amz-content-sha256", hash.as_str())]);
        assert_eq!(
            run(&good, None, b"hello".to_vec(), 2).await.unwrap(),
            b"hello"
        );

        let error = run(&good, None, b"HELLO".to_vec(), 2).await.unwrap_err();
        assert!(matches!(error, PayloadError::BadDigest(_)));
    }

    #[tokio::test]
    async fn checksum_headers_are_checked_for_every_algorithm() {
        // Known values for "hello".
        for (name, value) in [
            ("x-amz-checksum-crc32", "NhCmhg=="),
            ("x-amz-checksum-crc32c", "mnG7TA=="),
            ("x-amz-checksum-sha1", "qvTGHdzF6KLavt4PO0gs2a6pQ00="),
            (
                "x-amz-checksum-sha256",
                "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=",
            ),
        ] {
            let good = headers(&[(name, value)]);
            assert_eq!(
                run(&good, None, b"hello".to_vec(), 4096).await.unwrap(),
                b"hello",
                "{name}"
            );
            let error = run(&good, None, b"HELLO".to_vec(), 4096).await.unwrap_err();
            assert!(matches!(error, PayloadError::BadDigest(_)), "{name}");
        }
    }

    #[tokio::test]
    async fn crc64nvme_matches_its_published_check_value() {
        // The CRC-64/NVME check value for "123456789" is 0xae8b14860a799888.
        let good = headers(&[("x-amz-checksum-crc64nvme", "rosUhgp5mIg=")]);
        assert_eq!(
            run(&good, None, b"123456789".to_vec(), 4).await.unwrap(),
            b"123456789"
        );
    }

    #[tokio::test]
    async fn two_checksums_are_refused() {
        let headers = headers(&[
            ("x-amz-checksum-crc32", "NhCmhg=="),
            ("x-amz-checksum-sha1", "qvTGHdzF6KLavt4PO0gs2a6pQ00="),
        ]);
        let error = run(&headers, None, b"hello".to_vec(), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, PayloadError::Unsupported(_)));
    }
}
