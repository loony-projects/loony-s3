//! Append-only record of what admins did: every admin write, every browse of a bucket's
//! contents, and every rejected request. One JSON object per line in this node's data
//! directory, so it isn't replicated: each node holds the record of its own admin API.
//! Never given secrets. LS3 data-plane requests (uploads, downloads) aren't recorded.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// How much of the end of the file a read looks at, so viewing the log stays cheap
/// however large it has grown.
const MAX_TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub actor: String,
    pub action: String,
    pub target: String,
    /// `ok`, `denied` or `failed`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub struct AuditLog {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    /// A failure to write is logged rather than returned: by this point the action has
    /// already happened, and reporting it as failed would be wrong.
    pub async fn record(&self, entry: AuditEntry) {
        if let Err(err) = self.append(&entry).await {
            tracing::error!(%err, action = %entry.action, target = %entry.target, "failed to write audit entry");
        }
    }

    async fn append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let _guard = self.write_lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .await?;
        file.write_all(&line).await?;
        file.flush().await
    }

    /// The most recent `limit` entries, newest first.
    pub async fn recent(&self, limit: usize) -> std::io::Result<Vec<AuditEntry>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || read_tail(&path, limit))
            .await
            .map_err(std::io::Error::other)?
    }
}

fn read_tail(path: &std::path::Path, limit: usize) -> std::io::Result<Vec<AuditEntry>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let start = file.metadata()?.len().saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut contents = String::new();
    // Lossy so that a read starting mid-character, or a damaged line, can't hide the rest.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    contents.push_str(&String::from_utf8_lossy(&bytes));

    let mut lines = contents.lines();
    if start > 0 {
        lines.next(); // almost certainly a partial line
    }
    Ok(lines
        .rev()
        .filter_map(|line| serde_json::from_str(line).ok())
        .take(limit)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(action: &str) -> AuditEntry {
        AuditEntry {
            at: OffsetDateTime::now_utc(),
            actor: "AK".into(),
            action: action.into(),
            target: "t".into(),
            outcome: "ok".into(),
            detail: None,
        }
    }

    #[tokio::test]
    async fn returns_newest_first_up_to_the_limit_and_skips_damaged_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::new(path.clone());
        assert!(log.recent(10).await.unwrap().is_empty());

        log.record(entry("one")).await;
        log.record(entry("two")).await;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"not json\n"))
            .unwrap();
        log.record(entry("three")).await;

        let actions: Vec<_> = log.recent(2).await.unwrap().into_iter().map(|e| e.action).collect();
        assert_eq!(actions, ["three", "two"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_file_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        AuditLog::new(path.clone()).record(entry("one")).await;
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
