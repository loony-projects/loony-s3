//! `ErasureCodec` trait wrapping `reed-solomon-simd` for stripe encode/reconstruct
//! (architecture.md §8). Pure computation, no I/O — `loony-object`'s durability layer
//! drives this and does the actual shard reads/writes.
//!
//! **Why `reed-solomon-simd` over `reed-solomon-erasure`:** pure Rust with runtime
//! SIMD dispatch (AVX2/SSSE3/NEON with scalar fallback), O(n log n), and benchmarks
//! ahead of `reed-solomon-erasure` (which shells out to C for its SIMD paths) in most
//! cases — see docs/architecture.md §8.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErasureScheme {
    pub data: usize,
    pub parity: usize,
}

impl ErasureScheme {
    pub fn total(&self) -> usize {
        self.data + self.parity
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ErasureError {
    #[error("reed-solomon backend error: {0}")]
    Backend(String),
    #[error("not enough shards to reconstruct: need {needed}, have {have}")]
    InsufficientShards { needed: usize, have: usize },
}

/// The result of encoding one stripe: `total()` equal-length (zero-padded) shards,
/// indices `0..data` are the original data split into equal pieces, `data..total` are
/// parity. `shard_len` is how long each one is (needed to reconstruct correctly, since
/// the original data length may not divide evenly and padding is not self-describing).
pub struct EncodedStripe {
    pub shards: Vec<Vec<u8>>,
    pub shard_len: usize,
}

pub trait ErasureCodec: Send + Sync {
    fn scheme(&self) -> ErasureScheme;
    fn encode(&self, data: &[u8]) -> Result<EncodedStripe, ErasureError>;
    /// `shards[i]` is `Some` for every shard that's available (indices `0..data` are
    /// original, `data..total` are parity), `None` for missing/corrupt ones. At least
    /// `scheme().data` must be `Some`. `original_len` trims the reconstructed output
    /// back down from the zero-padded `shard_len * data` to the real object size.
    fn reconstruct(
        &self,
        shards: &[Option<Vec<u8>>],
        shard_len: usize,
        original_len: usize,
    ) -> Result<Vec<u8>, ErasureError>;
}

pub struct RsErasureCodec {
    scheme: ErasureScheme,
}

impl RsErasureCodec {
    pub fn new(data: usize, parity: usize) -> Self {
        Self {
            scheme: ErasureScheme { data, parity },
        }
    }
}

impl ErasureCodec for RsErasureCodec {
    fn scheme(&self) -> ErasureScheme {
        self.scheme
    }

    fn encode(&self, data: &[u8]) -> Result<EncodedStripe, ErasureError> {
        let n = self.scheme.data;
        // reed-solomon-simd requires an even shard length; round up to satisfy that as
        // well as give every data shard equal size (zero-padding the tail).
        let mut shard_len = data.len().div_ceil(n).max(2);
        if !shard_len.is_multiple_of(2) {
            shard_len += 1;
        }

        let mut data_shards: Vec<Vec<u8>> = Vec::with_capacity(n);
        for i in 0..n {
            let start = i * shard_len;
            let mut shard = vec![0u8; shard_len];
            if start < data.len() {
                let end = (start + shard_len).min(data.len());
                shard[..end - start].copy_from_slice(&data[start..end]);
            }
            data_shards.push(shard);
        }

        if self.scheme.parity == 0 {
            return Ok(EncodedStripe {
                shards: data_shards,
                shard_len,
            });
        }

        let recovery = reed_solomon_simd::encode(
            n,
            self.scheme.parity,
            data_shards.iter().map(|s| s.as_slice()),
        )
        .map_err(|e| ErasureError::Backend(e.to_string()))?;

        let mut shards = data_shards;
        shards.extend(recovery);
        Ok(EncodedStripe { shards, shard_len })
    }

    fn reconstruct(
        &self,
        shards: &[Option<Vec<u8>>],
        shard_len: usize,
        original_len: usize,
    ) -> Result<Vec<u8>, ErasureError> {
        let n = self.scheme.data;
        let m = self.scheme.parity;
        let have = shards.iter().filter(|s| s.is_some()).count();
        if have < n {
            return Err(ErasureError::InsufficientShards { needed: n, have });
        }

        // Fast path: every original data shard is present, no reconstruction math
        // needed at all (architecture.md §8/§22).
        if shards[..n].iter().all(|s| s.is_some()) {
            let mut out = Vec::with_capacity(n * shard_len);
            for s in &shards[..n] {
                out.extend_from_slice(s.as_ref().expect("checked Some above"));
            }
            out.truncate(original_len);
            return Ok(out);
        }

        let restored: BTreeMap<usize, Vec<u8>> = if m == 0 {
            return Err(ErasureError::InsufficientShards { needed: n, have });
        } else {
            let original_present: Vec<(usize, &[u8])> = shards[..n]
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_deref().map(|d| (i, d)))
                .collect();
            let recovery_present: Vec<(usize, &[u8])> = shards[n..n + m]
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_deref().map(|d| (i, d)))
                .collect();
            reed_solomon_simd::decode(n, m, original_present, recovery_present)
                .map_err(|e| ErasureError::Backend(e.to_string()))?
        };

        let mut out = vec![0u8; n * shard_len];
        for (i, slot) in out.chunks_mut(shard_len).enumerate() {
            if let Some(s) = &shards[i] {
                slot.copy_from_slice(s);
            } else if let Some(r) = restored.get(&i) {
                slot.copy_from_slice(r);
            } else {
                return Err(ErasureError::Backend(format!(
                    "shard {i} missing after reconstruction"
                )));
            }
        }
        out.truncate(original_len);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    #[test]
    fn encode_then_reconstruct_with_no_losses() {
        let codec = RsErasureCodec::new(4, 2);
        let data = b"the quick brown fox jumps over the lazy dog".repeat(50);
        let encoded = codec.encode(&data).unwrap();
        assert_eq!(encoded.shards.len(), 6);

        let shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        let reconstructed = codec
            .reconstruct(&shards, encoded.shard_len, data.len())
            .unwrap();
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn reconstructs_after_losing_up_to_parity_count_shards() {
        let codec = RsErasureCodec::new(4, 2);
        let data = b"0123456789".repeat(777);
        let encoded = codec.encode(&data).unwrap();

        // Lose 2 shards (the parity count): one data shard and one parity shard.
        let mut shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        shards[1] = None;
        shards[5] = None;

        let reconstructed = codec
            .reconstruct(&shards, encoded.shard_len, data.len())
            .unwrap();
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn fails_clearly_when_too_many_shards_are_missing() {
        let codec = RsErasureCodec::new(4, 2);
        let data = b"some data".repeat(100);
        let encoded = codec.encode(&data).unwrap();

        let mut shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        shards[0] = None;
        shards[1] = None;
        shards[2] = None; // 3 losses > 2 parity shards: unrecoverable

        let err = codec
            .reconstruct(&shards, encoded.shard_len, data.len())
            .unwrap_err();
        assert!(matches!(err, ErasureError::InsufficientShards { .. }));
    }

    #[test]
    fn property_random_data_survives_up_to_m_losses_for_various_schemes() {
        let mut rng = rand::thread_rng();
        for &(n, m) in &[(2usize, 1usize), (4, 2), (8, 4)] {
            let codec = RsErasureCodec::new(n, m);
            for trial in 0..20 {
                let len = 1 + (rng.next_u32() as usize % 5000);
                let mut data = vec![0u8; len];
                rng.fill_bytes(&mut data);

                let encoded = codec.encode(&data).unwrap();
                let mut shards: Vec<Option<Vec<u8>>> =
                    encoded.shards.iter().cloned().map(Some).collect();

                // Drop a random selection of up to m shards (any mix of data/parity).
                let mut indices: Vec<usize> = (0..encoded.shards.len()).collect();
                for i in (1..indices.len()).rev() {
                    let j = (rng.next_u32() as usize) % (i + 1);
                    indices.swap(i, j);
                }
                for &idx in indices.iter().take(m) {
                    shards[idx] = None;
                }

                let reconstructed = codec
                    .reconstruct(&shards, encoded.shard_len, data.len())
                    .unwrap_or_else(|e| panic!("n={n} m={m} trial={trial} len={len} failed: {e}"));
                assert_eq!(reconstructed, data, "n={n} m={m} trial={trial} len={len}");
            }
        }
    }
}
