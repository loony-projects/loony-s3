//! `RedbStateMachineStore`: `openraft`'s [`RaftStateMachine`] + [`RaftSnapshotBuilder`],
//! backed by the same redb tables `applier.rs` operates on plus two Raft-internal ones
//! (`sm_meta` for the last-applied log id/membership, `snapshot` for this node's most
//! recently built/installed snapshot — architecture.md §5's "two redb tables: raft_log
//! and raft_state_machine", split further here so applied business state and Raft's own
//! bookkeeping about it never share a row).
//!
//! Snapshotting serializes every business table to one JSON blob (`snapshot.rs`) rather
//! than copying the redb file — simpler, and the blob is exactly what
//! `install_snapshot` needs to atomically replace: wipe every table, replay the blob,
//! all inside one write transaction, in the same commit as the new `sm_meta`.

use std::io::Cursor;
use std::sync::Arc;

use redb::{Database, ReadableTable, TableDefinition};

use crate::error::MetaError;
use crate::raft::applier::{self, db_err};
use crate::raft::snapshot::{restore_blob, snapshot_blob};
use crate::raft::types::TypeConfig;

const SM_META: TableDefinition<&str, &[u8]> = TableDefinition::new("sm_meta");
const SM_META_KEY: &str = "applied";
const SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("snapshot");
const SNAPSHOT_KEY: &str = "current";

type LogId = openraft::LogId<s3_core::NodeId>;
type StoredMembership = openraft::StoredMembership<s3_core::NodeId, openraft::BasicNode>;
type SnapshotMeta = openraft::SnapshotMeta<s3_core::NodeId, openraft::BasicNode>;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AppliedState {
    last_applied_log: Option<LogId>,
    last_membership: StoredMembership,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct StoredSnapshot {
    meta: SnapshotMeta,
    data: Vec<u8>,
}

/// Cheaply `Clone` (an `Arc<Database>` handle) so the same store can be handed to
/// `openraft::Raft::new` (which takes ownership) while `RaftMetadataStore` keeps a
/// second clone for direct reads (`store.rs`) — exactly the split the upstream
/// `raft-kv-memstore` example uses `Arc<StateMachineStore>` for.
#[derive(Clone)]
pub struct RedbStateMachineStore {
    pub(crate) db: Arc<Database>,
}

impl RedbStateMachineStore {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    fn read_applied_state(&self) -> Result<AppliedState, MetaError> {
        let read_txn = self.db.begin_read().map_err(db_err)?;
        let table = read_txn.open_table(SM_META).map_err(db_err)?;
        match table.get(SM_META_KEY).map_err(db_err)? {
            Some(guard) => Ok(serde_json::from_slice(guard.value())?),
            None => Ok(AppliedState {
                last_applied_log: None,
                last_membership: StoredMembership::default(),
            }),
        }
    }
}

impl openraft::storage::RaftSnapshotBuilder<TypeConfig> for RedbStateMachineStore {
    async fn build_snapshot(&mut self) -> Result<openraft::Snapshot<TypeConfig>, openraft::StorageError<s3_core::NodeId>> {
        let this = self.clone();
        let (meta, data) = tokio::task::spawn_blocking(move || -> Result<(SnapshotMeta, Vec<u8>), MetaError> {
            let applied = this.read_applied_state()?;
            let data = snapshot_blob(&this.db)?;
            let meta = SnapshotMeta {
                last_log_id: applied.last_applied_log,
                last_membership: applied.last_membership,
                snapshot_id: format!(
                    "{}-{}",
                    applied
                        .last_applied_log
                        .map(|l| l.index)
                        .unwrap_or_default(),
                    uuid::Uuid::now_v7()
                ),
            };

            let write_txn = this.db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(SNAPSHOT).map_err(db_err)?;
                let stored = StoredSnapshot {
                    meta: meta.clone(),
                    data: data.clone(),
                };
                let bytes = serde_json::to_vec(&stored)?;
                table.insert(SNAPSHOT_KEY, bytes.as_slice()).map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;

            Ok((meta, data))
        })
        .await
        .map_err(|e| openraft::StorageIOError::read_state_machine(&e))?
        .map_err(|e| openraft::StorageIOError::read_state_machine(&e))?;

        Ok(openraft::Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl openraft::storage::RaftStateMachine<TypeConfig> for RedbStateMachineStore {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (Option<openraft::LogId<s3_core::NodeId>>, StoredMembership),
        openraft::StorageError<s3_core::NodeId>,
    > {
        let this = self.clone();
        let applied = tokio::task::spawn_blocking(move || this.read_applied_state())
            .await
            .map_err(|e| openraft::StorageIOError::read_state_machine(&e))?
            .map_err(|e| openraft::StorageIOError::read_state_machine(&e))?;
        Ok((applied.last_applied_log, applied.last_membership))
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<crate::raft::types::CommandResult>, openraft::StorageError<s3_core::NodeId>>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<crate::raft::types::CommandResult>, MetaError> {
            let mut results = Vec::with_capacity(entries.len());
            for entry in entries {
                let log_id = entry.log_id;
                let new_membership = match &entry.payload {
                    openraft::EntryPayload::Membership(membership) => {
                        Some(StoredMembership::new(Some(log_id), membership.clone()))
                    }
                    _ => None,
                };
                let result = match entry.payload {
                    openraft::EntryPayload::Blank | openraft::EntryPayload::Membership(_) => {
                        Ok(crate::raft::types::CommandResponse::Unit)
                    }
                    openraft::EntryPayload::Normal(cmd) => applier::apply_metadata_command(&this.db, cmd),
                };

                // The command's own transaction (if `Normal`) already committed
                // separately above; this records "this log id is now applied" -- and,
                // for a membership-change entry, the new membership alongside it -- in
                // its own transaction, same as `redb_store.rs`'s one-transaction-per-
                // mutation discipline applied to Raft's own bookkeeping.
                let write_txn = this.db.begin_write().map_err(db_err)?;
                {
                    let mut table = write_txn.open_table(SM_META).map_err(db_err)?;
                    let mut applied = match table.get(SM_META_KEY).map_err(db_err)? {
                        Some(guard) => serde_json::from_slice(guard.value())?,
                        None => AppliedState {
                            last_applied_log: None,
                            last_membership: StoredMembership::default(),
                        },
                    };
                    applied.last_applied_log = Some(log_id);
                    if let Some(membership) = new_membership {
                        applied.last_membership = membership;
                    }
                    let bytes = serde_json::to_vec(&applied)?;
                    table.insert(SM_META_KEY, bytes.as_slice()).map_err(db_err)?;
                }
                write_txn.commit().map_err(db_err)?;

                results.push(result);
            }
            Ok(results)
        })
        .await
        .map_err(|e| openraft::StorageIOError::write(&e))?
        .map_err(|e| openraft::StorageIOError::write(&e).into())
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, openraft::StorageError<s3_core::NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), openraft::StorageError<s3_core::NodeId>> {
        let data = snapshot.into_inner();
        let this = self.clone();
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || -> Result<(), MetaError> {
            let write_txn = this.db.begin_write().map_err(db_err)?;
            restore_blob(&write_txn, &data)?;
            {
                let mut table = write_txn.open_table(SM_META).map_err(db_err)?;
                let applied = AppliedState {
                    last_applied_log: meta.last_log_id,
                    last_membership: meta.last_membership.clone(),
                };
                let bytes = serde_json::to_vec(&applied)?;
                table.insert(SM_META_KEY, bytes.as_slice()).map_err(db_err)?;
            }
            {
                let mut table = write_txn.open_table(SNAPSHOT).map_err(db_err)?;
                let stored = StoredSnapshot {
                    meta: meta.clone(),
                    data: data.clone(),
                };
                let bytes = serde_json::to_vec(&stored)?;
                table.insert(SNAPSHOT_KEY, bytes.as_slice()).map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
        .map_err(|e| openraft::StorageIOError::write_snapshot(None, &e))?
        .map_err(|e| openraft::StorageIOError::write_snapshot(None, &e).into())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<openraft::Snapshot<TypeConfig>>, openraft::StorageError<s3_core::NodeId>> {
        let this = self.clone();
        let stored = tokio::task::spawn_blocking(move || -> Result<Option<StoredSnapshot>, MetaError> {
            let read_txn = this.db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(SNAPSHOT).map_err(db_err)?;
            match table.get(SNAPSHOT_KEY).map_err(db_err)? {
                Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| openraft::StorageIOError::read_snapshot(None, &e))?
        .map_err(|e| openraft::StorageIOError::read_snapshot(None, &e))?;

        Ok(stored.map(|s| openraft::Snapshot {
            meta: s.meta,
            snapshot: Box::new(Cursor::new(s.data)),
        }))
    }
}
