//! Splits an incoming byte stream into fixed-size chunks ("stripes") without ever
//! buffering more than one stripe at a time (architecture.md §8/§81: memory must not
//! scale with object size). A chunk from the underlying stream rarely lines up exactly
//! on a stripe boundary, so leftover bytes carry over between calls.

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};

pub struct StripeReader<S> {
    inner: S,
    leftover: BytesMut,
    exhausted: bool,
}

impl<S: Stream<Item = std::io::Result<Bytes>> + Unpin> StripeReader<S> {
    /// Seeds the reader with bytes already read from `inner` by a caller that peeked
    /// ahead (e.g. to decide small-object vs. erasure-coded durability) before handing
    /// the stream off here.
    pub fn with_leftover(inner: S, leftover: Vec<u8>) -> Self {
        Self {
            inner,
            leftover: BytesMut::from(leftover.as_slice()),
            exhausted: false,
        }
    }

    /// Returns up to `max_bytes` of the next stripe, or `None` once there's truly
    /// nothing left. The final stripe of an object is often shorter than `max_bytes`.
    pub async fn next_stripe(&mut self, max_bytes: usize) -> std::io::Result<Option<Vec<u8>>> {
        while self.leftover.len() < max_bytes && !self.exhausted {
            match self.inner.next().await {
                Some(Ok(chunk)) => self.leftover.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(e),
                None => self.exhausted = true,
            }
        }
        if self.leftover.is_empty() {
            return Ok(None);
        }
        let take = self.leftover.len().min(max_bytes);
        Ok(Some(self.leftover.split_to(take).to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn src(chunks: Vec<&'static [u8]>) -> impl Stream<Item = std::io::Result<Bytes>> + Unpin {
        stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    #[tokio::test]
    async fn splits_chunks_that_straddle_stripe_boundaries() {
        let mut reader =
            StripeReader::with_leftover(src(vec![b"abc", b"defgh", b"ij"]), Vec::new());
        assert_eq!(reader.next_stripe(4).await.unwrap(), Some(b"abcd".to_vec()));
        assert_eq!(reader.next_stripe(4).await.unwrap(), Some(b"efgh".to_vec()));
        assert_eq!(reader.next_stripe(4).await.unwrap(), Some(b"ij".to_vec()));
        assert_eq!(reader.next_stripe(4).await.unwrap(), None);
    }

    #[tokio::test]
    async fn seeded_leftover_is_consumed_first() {
        let mut reader = StripeReader::with_leftover(src(vec![b"world"]), b"hello ".to_vec());
        assert_eq!(
            reader.next_stripe(100).await.unwrap(),
            Some(b"hello world".to_vec())
        );
        assert_eq!(reader.next_stripe(100).await.unwrap(), None);
    }

    #[tokio::test]
    async fn empty_stream_yields_nothing() {
        let mut reader = StripeReader::with_leftover(src(vec![]), Vec::new());
        assert_eq!(reader.next_stripe(4).await.unwrap(), None);
    }
}
