use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::Snapshot;

#[derive(Clone, Debug)]
pub struct SnapshotStore {
    path: PathBuf,
}

impl SnapshotStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let store = Self { path };
        let connection = store.connect()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS snapshots (
               singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
               generation INTEGER NOT NULL,
               payload BLOB NOT NULL
             );",
            )
            .context("initialize snapshot database")?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open(&self.path).with_context(|| format!("open {}", self.path.display()))
    }

    pub fn load(&self) -> Result<Option<Snapshot>> {
        let connection = self.connect()?;
        let payload: Option<Vec<u8>> = connection
            .query_row(
                "SELECT payload FROM snapshots WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("load snapshot")?;
        payload
            .map(|bytes| serde_json::from_slice(&bytes).context("decode snapshot"))
            .transpose()
    }

    pub fn save(&self, snapshot: &Snapshot) -> Result<()> {
        let payload = serde_json::to_vec(snapshot).context("encode snapshot")?;
        let generation = i64::try_from(snapshot.generation).unwrap_or(i64::MAX);
        self.connect()?.execute(
            "INSERT INTO snapshots(singleton, generation, payload) VALUES(1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET generation=excluded.generation, payload=excluded.payload",
            params![generation, payload],
        ).context("save snapshot")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let store = SnapshotStore::open(directory.path().join("index.db")).unwrap();
        let mut expected = Snapshot::empty();
        expected.generation = 7;
        store.save(&expected).unwrap();
        assert_eq!(store.load().unwrap().unwrap().generation, 7);
    }
}
