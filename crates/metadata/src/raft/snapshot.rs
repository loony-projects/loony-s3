//! Whole-state-machine snapshot (de)serialization: every business table (`applier.rs`'s
//! `ALL_TABLES`) dumped to one JSON blob and restored from one, matching the upstream
//! `raft-kv-memstore` example's approach of `serde_json`-encoding the entire state
//! machine rather than copying the underlying store file.

use redb::{ReadableTable, TableHandle, WriteTransaction};

use crate::error::MetaError;
use crate::raft::applier::{ALL_TABLES, db_err};

/// One table's rows, keyed by the table's own name so `restore_blob` doesn't have to
/// assume `ALL_TABLES`'s order matches between the snapshot's producer and consumer
/// (different builds of this same binary, but still -- don't rely on it).
#[derive(serde::Serialize, serde::Deserialize)]
struct TableDump {
    table: String,
    rows: Vec<(String, Vec<u8>)>,
}

pub(crate) fn snapshot_blob(db: &redb::Database) -> Result<Vec<u8>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let mut dumps = Vec::with_capacity(ALL_TABLES.len());
    for table_def in ALL_TABLES {
        let table = read_txn.open_table(*table_def).map_err(db_err)?;
        let mut rows = Vec::new();
        for entry in table.iter().map_err(db_err)? {
            let (key, value) = entry.map_err(db_err)?;
            rows.push((key.value().to_string(), value.value().to_vec()));
        }
        dumps.push(TableDump {
            table: table_def.name().to_string(),
            rows,
        });
    }
    Ok(serde_json::to_vec(&dumps)?)
}

/// Wipes every business table and replays `data` into it, inside the caller's write
/// transaction -- so a mid-install crash leaves either the old state machine intact or
/// the new one fully applied, never a mix.
pub(crate) fn restore_blob(write_txn: &WriteTransaction, data: &[u8]) -> Result<(), MetaError> {
    let dumps: Vec<TableDump> = serde_json::from_slice(data)?;

    for table_def in ALL_TABLES {
        let mut table = write_txn.open_table(*table_def).map_err(db_err)?;
        table.retain(|_, _| false).map_err(db_err)?;
    }

    for dump in dumps {
        let table_def = ALL_TABLES
            .iter()
            .find(|t| t.name() == dump.table)
            .ok_or_else(|| MetaError::Db(format!("unknown table {:?} in snapshot", dump.table)))?;
        let mut table = write_txn.open_table(*table_def).map_err(db_err)?;
        for (key, value) in dump.rows {
            table.insert(key.as_str(), value.as_slice()).map_err(db_err)?;
        }
    }

    Ok(())
}
