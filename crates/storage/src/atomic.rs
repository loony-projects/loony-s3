//! Atomic whole-file write: write to a temp path, fsync it, rename over the destination,
//! then fsync the parent directory so the rename itself survives a crash. Used for small
//! metadata files (`VOLUME_META`); the shard write path in `local.rs` follows the same
//! shape but streams instead of writing a single buffer.

use std::path::Path;

pub async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    let file = tokio::fs::File::open(&tmp).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    sync_parent_dir(path).await
}

pub async fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let dir = tokio::fs::File::open(parent).await?;
        dir.sync_all().await?;
    }
    Ok(())
}
