//! `RedbLogStore`: `openraft`'s [`RaftLogReader`] + [`RaftLogStorage`], persisting the
//! Raft log and vote in their own redb tables (architecture.md §5's `raft_log`), kept
//! entirely separate from the state machine's tables (`state_machine.rs`) — a purge or
//! truncate here never touches applied business state, and vice versa.
//!
//! Row keys are the log index directly (`u64`), so `range`/`iter` already return entries
//! in log order for free — no separate index needed for "get the last entry" or
//! "entries in [start, end)".

use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use redb::{Database, ReadableTable, TableDefinition};

use crate::error::MetaError;
use crate::raft::applier::db_err;
use crate::raft::types::TypeConfig;

const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
const VOTE: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_vote");
const VOTE_KEY: &str = "vote";
const LAST_PURGED: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_last_purged");
const LAST_PURGED_KEY: &str = "last_purged";

type LogId = openraft::LogId<loony_core::NodeId>;
type Vote = openraft::Vote<loony_core::NodeId>;
type Entry = openraft::Entry<TypeConfig>;

/// Cheaply `Clone` (an `Arc<Database>` handle), matching `RedbStateMachineStore` and the
/// same reason: `get_log_reader()` returns an owned `Self::LogReader`, so `Self` being a
/// plain handle clone is the simplest correct implementation.
#[derive(Clone)]
pub struct RedbLogStore {
    db: Arc<Database>,
}

impl RedbLogStore {
    pub(crate) fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    fn read_vote_sync(&self) -> Result<Option<Vote>, MetaError> {
        let read_txn = self.db.begin_read().map_err(db_err)?;
        let table = read_txn.open_table(VOTE).map_err(db_err)?;
        match table.get(VOTE_KEY).map_err(db_err)? {
            Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
            None => Ok(None),
        }
    }

    fn read_last_purged_sync(&self) -> Result<Option<LogId>, MetaError> {
        let read_txn = self.db.begin_read().map_err(db_err)?;
        let table = read_txn.open_table(LAST_PURGED).map_err(db_err)?;
        match table.get(LAST_PURGED_KEY).map_err(db_err)? {
            Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
            None => Ok(None),
        }
    }

    fn entries_in_range(&self, range: (Bound<u64>, Bound<u64>)) -> Result<Vec<Entry>, MetaError> {
        let read_txn = self.db.begin_read().map_err(db_err)?;
        let table = read_txn.open_table(LOG).map_err(db_err)?;
        let mut entries = Vec::new();
        for item in table.range(range).map_err(db_err)? {
            let (_, value) = item.map_err(db_err)?;
            entries.push(serde_json::from_slice(value.value())?);
        }
        Ok(entries)
    }
}

impl openraft::storage::RaftLogReader<TypeConfig> for RedbLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + std::fmt::Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, openraft::StorageError<loony_core::NodeId>> {
        // `RB` isn't `'static`, so it can't cross into `spawn_blocking`'s closure as-is
        // -- but its bounds are just `u64`s, which are, so extract those before moving.
        let owned_range = (range.start_bound().cloned(), range.end_bound().cloned());
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.entries_in_range(owned_range))
            .await
            .map_err(|e| openraft::StorageIOError::<loony_core::NodeId>::read_logs(&e))?
            .map_err(|e| openraft::StorageIOError::<loony_core::NodeId>::read_logs(&e).into())
    }
}

impl openraft::storage::RaftLogStorage<TypeConfig> for RedbLogStore {
    type LogReader = Self;

    async fn get_log_state(
        &mut self,
    ) -> Result<openraft::storage::LogState<TypeConfig>, openraft::StorageError<loony_core::NodeId>> {
        let this = self.clone();
        let (last_purged, last) = tokio::task::spawn_blocking(move || -> Result<_, MetaError> {
            let last_purged = this.read_last_purged_sync()?;
            let read_txn = this.db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(LOG).map_err(db_err)?;
            let last = match table.iter().map_err(db_err)?.next_back() {
                Some(item) => {
                    let (_, value) = item.map_err(db_err)?;
                    let entry: Entry = serde_json::from_slice(value.value())?;
                    Some(entry.log_id)
                }
                None => None,
            };
            Ok((last_purged, last))
        })
        .await
        .map_err(|e| openraft::StorageIOError::read_logs(&e))?
        .map_err(|e| openraft::StorageIOError::read_logs(&e))?;

        Ok(openraft::storage::LogState {
            last_purged_log_id: last_purged,
            last_log_id: last.or(last_purged),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> Result<(), openraft::StorageError<loony_core::NodeId>> {
        let this = self.clone();
        let vote = *vote;
        tokio::task::spawn_blocking(move || -> Result<(), MetaError> {
            let write_txn = this.db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(VOTE).map_err(db_err)?;
                let bytes = serde_json::to_vec(&vote)?;
                table.insert(VOTE_KEY, bytes.as_slice()).map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
        .map_err(|e| openraft::StorageIOError::write_vote(&e))?
        .map_err(|e| openraft::StorageIOError::write_vote(&e).into())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, openraft::StorageError<loony_core::NodeId>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_vote_sync())
            .await
            .map_err(|e| openraft::StorageIOError::read_vote(&e))?
            .map_err(|e| openraft::StorageIOError::read_vote(&e).into())
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: openraft::storage::LogFlushed<TypeConfig>,
    ) -> Result<(), openraft::StorageError<loony_core::NodeId>>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<Entry> = entries.into_iter().collect();
        let this = self.clone();
        let joined: Result<Result<(), MetaError>, tokio::task::JoinError> =
            tokio::task::spawn_blocking(move || -> Result<(), MetaError> {
                let write_txn = this.db.begin_write().map_err(db_err)?;
                {
                    let mut table = write_txn.open_table(LOG).map_err(db_err)?;
                    for entry in &entries {
                        let bytes = serde_json::to_vec(entry)?;
                        table.insert(entry.log_id.index, bytes.as_slice()).map_err(db_err)?;
                    }
                }
                write_txn.commit().map_err(db_err)?;
                Ok(())
            })
            .await;

        match joined {
            Ok(Ok(())) => callback.log_io_completed(Ok(())),
            Ok(Err(e)) => callback.log_io_completed(Err(std::io::Error::other(e.to_string()))),
            Err(e) => callback.log_io_completed(Err(std::io::Error::other(e.to_string()))),
        }
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId) -> Result<(), openraft::StorageError<loony_core::NodeId>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<(), MetaError> {
            let write_txn = this.db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(LOG).map_err(db_err)?;
                let keys: Vec<u64> = table
                    .range(log_id.index..)
                    .map_err(db_err)?
                    .map(|item| item.map(|(k, _)| k.value()).map_err(db_err))
                    .collect::<Result<_, _>>()?;
                for key in keys {
                    table.remove(key).map_err(db_err)?;
                }
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
        .map_err(|e| openraft::StorageIOError::write_logs(&e))?
        .map_err(|e| openraft::StorageIOError::write_logs(&e).into())
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), openraft::StorageError<loony_core::NodeId>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<(), MetaError> {
            let write_txn = this.db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(LAST_PURGED).map_err(db_err)?;
                let bytes = serde_json::to_vec(&log_id)?;
                table.insert(LAST_PURGED_KEY, bytes.as_slice()).map_err(db_err)?;
            }
            {
                let mut table = write_txn.open_table(LOG).map_err(db_err)?;
                let keys: Vec<u64> = table
                    .range(..=log_id.index)
                    .map_err(db_err)?
                    .map(|item| item.map(|(k, _)| k.value()).map_err(db_err))
                    .collect::<Result<_, _>>()?;
                for key in keys {
                    table.remove(key).map_err(db_err)?;
                }
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
        .map_err(|e| openraft::StorageIOError::write_logs(&e))?
        .map_err(|e| openraft::StorageIOError::write_logs(&e).into())
    }
}
