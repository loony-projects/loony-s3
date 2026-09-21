//! Placement engine (architecture.md §10): rendezvous hashing (HRW — Highest Random
//! Weight) over the cluster's healthy `(node_id, volume_id)` candidates, used to choose
//! where each shard of a stripe should be written. Pure computation, no I/O — callers
//! (`s3-object`) gather the candidate list from the (already-replicated) node registry
//! and hand it in.
//!
//! **Why HRW over consistent hashing with vnodes**: no persisted ring state. The same
//! deterministic function run against the current healthy-member list reproduces the
//! same target set every time, and a membership change only remaps the shards whose
//! top-N scores actually changed — exactly the minimal-disruption property rebalancing
//! (a later phase) depends on.
//!
//! **Failure-domain spread, simplified for this phase**: the full spec (§10) rejects a
//! candidate set with two shards of the same stripe in the same failure domain,
//! failing fast with `InsufficientFailureDomains` when the cluster is too small to
//! satisfy that. Applied strictly, "same node" as the initial domain would make
//! erasure coding *impossible* on a single node with multiple local volumes — exactly
//! standalone mode's normal case, and exactly what pre-Phase-9 code did on purpose.
//! So this implementation *prefers* distinct nodes (ranking nodes by their best
//! candidate's score, taking one volume from each of the highest-scoring nodes first)
//! and only falls back to a second volume on an already-used node when there aren't
//! enough distinct nodes to fill every shard slot. It never hard-fails the way the full
//! spec does — documented tradeoff, not an oversight.

use std::collections::HashMap;

use s3_core::{NodeId, ObjectId, VersionId, VolumeId};
use sha2::{Digest, Sha256};

/// One writable `(node, volume)` the placement engine may choose — only `HEALTHY`
/// nodes' `ACTIVE` volumes should ever be passed in; filtering happens before scoring,
/// not after (architecture.md §10).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub node_id: NodeId,
    pub volume_id: VolumeId,
    /// Relative selection weight — higher means more likely to be picked. `1.0` for
    /// every candidate (uniform) until real capacity/utilization tracking exists to
    /// derive it from.
    pub weight: f64,
}

impl Candidate {
    pub fn new(node_id: NodeId, volume_id: VolumeId) -> Self {
        Self {
            node_id,
            volume_id,
            weight: 1.0,
        }
    }
}

/// One shard's chosen destination, in `shard_index` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedShard {
    pub shard_index: u16,
    pub node_id: NodeId,
    pub volume_id: VolumeId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("need {needed} shard targets but only {available} candidate volumes are healthy")]
    InsufficientCandidates { needed: usize, available: usize },
}

/// Deterministic HRW score for one candidate against `stripe_key` — the same inputs
/// always produce the same score, which is the entire property placement (and its
/// disruption-minimizing behavior under membership changes) relies on.
fn score(stripe_key: &[u8; 32], node_id: NodeId, volume_id: VolumeId, weight: f64) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(stripe_key);
    hasher.update(node_id.as_uuid().as_bytes());
    hasher.update(volume_id.as_uuid().as_bytes());
    let digest = hasher.finalize();
    let bytes: [u8; 8] = digest[..8].try_into().unwrap();
    // Normalize to [0, 1) so `weight` scales it meaningfully regardless of magnitude.
    let unit = (u64::from_be_bytes(bytes) as f64) / (u64::MAX as f64);
    unit * weight
}

fn stripe_key(object_id: ObjectId, version_id: VersionId, stripe_index: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(object_id.as_uuid().as_bytes());
    hasher.update(version_id.as_uuid().as_bytes());
    hasher.update(stripe_index.to_be_bytes());
    hasher.finalize().into()
}

/// Chooses `total_shards` distinct `(node, volume)` targets for one stripe, preferring
/// to spread across distinct nodes before ever placing two shards on the same node.
/// Returns targets in descending-score order, assigned `shard_index` 0..`total_shards`.
pub fn plan_write(
    object_id: ObjectId,
    version_id: VersionId,
    stripe_index: u32,
    total_shards: usize,
    candidates: &[Candidate],
) -> Result<Vec<PlacedShard>, PlacementError> {
    if candidates.len() < total_shards {
        return Err(PlacementError::InsufficientCandidates {
            needed: total_shards,
            available: candidates.len(),
        });
    }
    if total_shards == 0 {
        return Ok(Vec::new());
    }

    let key = stripe_key(object_id, version_id, stripe_index);
    let mut scored: Vec<(f64, Candidate)> = candidates
        .iter()
        .map(|c| (score(&key, c.node_id, c.volume_id, c.weight), *c))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    // Pass 1: one candidate per distinct node, best-scoring node first.
    let mut by_node_best: HashMap<NodeId, (f64, Candidate)> = HashMap::new();
    for &(s, c) in &scored {
        by_node_best.entry(c.node_id).or_insert((s, c));
    }
    let mut node_ranked: Vec<(f64, Candidate)> = by_node_best.into_values().collect();
    node_ranked.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut selected: Vec<(f64, Candidate)> = node_ranked.into_iter().take(total_shards).collect();

    // Pass 2: not enough distinct nodes -- fill remaining slots from the overall
    // ranking, skipping anything already selected (by node+volume identity).
    if selected.len() < total_shards {
        for &(s, c) in &scored {
            if selected.len() >= total_shards {
                break;
            }
            let already = selected
                .iter()
                .any(|(_, sc)| sc.node_id == c.node_id && sc.volume_id == c.volume_id);
            if !already {
                selected.push((s, c));
            }
        }
    }

    selected.sort_by(|a, b| b.0.total_cmp(&a.0));

    Ok(selected
        .into_iter()
        .take(total_shards)
        .enumerate()
        .map(|(i, (_, c))| PlacedShard {
            shard_index: i as u16,
            node_id: c.node_id,
            volume_id: c.volume_id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(nodes: usize, volumes_per_node: usize) -> (Vec<NodeId>, Vec<Candidate>) {
        let node_ids: Vec<NodeId> = (0..nodes).map(|_| NodeId::new()).collect();
        let mut candidates = Vec::new();
        for &node_id in &node_ids {
            for _ in 0..volumes_per_node {
                candidates.push(Candidate::new(node_id, VolumeId::new()));
            }
        }
        (node_ids, candidates)
    }

    #[test]
    fn same_inputs_always_produce_the_same_plan() {
        let (_, candidates) = candidates(6, 1);
        let object_id = ObjectId::new();
        let version_id = VersionId::new();

        let plan1 = plan_write(object_id, version_id, 0, 4, &candidates).unwrap();
        let plan2 = plan_write(object_id, version_id, 0, 4, &candidates).unwrap();
        assert_eq!(plan1, plan2);
    }

    #[test]
    fn different_stripe_indices_can_choose_different_targets() {
        let (_, candidates) = candidates(10, 1);
        let object_id = ObjectId::new();
        let version_id = VersionId::new();

        let plan0 = plan_write(object_id, version_id, 0, 4, &candidates).unwrap();
        let plan1 = plan_write(object_id, version_id, 1, 4, &candidates).unwrap();
        // Not a hard guarantee for any specific pair of stripe indices, but with 10
        // candidates and only 4 chosen, two independent stripe keys landing on the
        // exact same *set* is astronomically unlikely -- a real behavioral check, not
        // a tautology.
        let set0: std::collections::HashSet<_> =
            plan0.iter().map(|p| (p.node_id, p.volume_id)).collect();
        let set1: std::collections::HashSet<_> =
            plan1.iter().map(|p| (p.node_id, p.volume_id)).collect();
        assert_ne!(set0, set1);
    }

    #[test]
    fn prefers_distinct_nodes_when_enough_exist() {
        // 6 nodes, 2 volumes each -- plenty of candidates, but only 4 shards needed, so
        // every shard should land on a *different* node rather than doubling up on any
        // one node while others sit idle.
        let (_, candidates) = candidates(6, 2);
        let plan = plan_write(ObjectId::new(), VersionId::new(), 0, 4, &candidates).unwrap();
        let distinct_nodes: std::collections::HashSet<_> = plan.iter().map(|p| p.node_id).collect();
        assert_eq!(
            distinct_nodes.len(),
            4,
            "expected 4 distinct nodes, got plan {plan:?}"
        );
    }

    #[test]
    fn falls_back_to_co_location_when_too_few_nodes() {
        // Only 2 nodes but 6 local volumes each (the standalone-with-6-volumes case
        // pre-Phase-9 code already supported) -- must still produce 6 targets, not fail.
        let (node_ids, candidates) = candidates(2, 6);
        let plan = plan_write(ObjectId::new(), VersionId::new(), 0, 6, &candidates).unwrap();
        assert_eq!(plan.len(), 6);
        let distinct_nodes: std::collections::HashSet<_> = plan.iter().map(|p| p.node_id).collect();
        assert!(distinct_nodes.len() <= node_ids.len());
        // Every chosen (node, volume) pair must be distinct even though nodes repeat.
        let pairs: std::collections::HashSet<_> =
            plan.iter().map(|p| (p.node_id, p.volume_id)).collect();
        assert_eq!(pairs.len(), 6);
    }

    #[test]
    fn a_single_node_single_volume_still_works() {
        // The degenerate standalone case: one candidate, one shard.
        let node_id = NodeId::new();
        let volume_id = VolumeId::new();
        let candidates = vec![Candidate::new(node_id, volume_id)];
        let plan = plan_write(ObjectId::new(), VersionId::new(), 0, 1, &candidates).unwrap();
        assert_eq!(
            plan,
            vec![PlacedShard {
                shard_index: 0,
                node_id,
                volume_id
            }]
        );
    }

    #[test]
    fn rejects_when_not_enough_candidates() {
        let (_, candidates) = candidates(1, 2);
        let err = plan_write(ObjectId::new(), VersionId::new(), 0, 3, &candidates).unwrap_err();
        assert_eq!(
            err,
            PlacementError::InsufficientCandidates {
                needed: 3,
                available: 2
            }
        );
    }

    #[test]
    fn zero_shards_is_a_trivial_empty_plan() {
        let (_, candidates) = candidates(3, 1);
        let plan = plan_write(ObjectId::new(), VersionId::new(), 0, 0, &candidates).unwrap();
        assert!(plan.is_empty());
    }
}
