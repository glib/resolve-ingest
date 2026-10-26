use crate::{model::Batch, storage::mount_for};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, params};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::fd::{AsRawFd, RawFd},
    path::{Path, PathBuf},
};

pub struct Jobs {
    db: Connection,
    lock: File,
}
impl Jobs {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let mount = mount_for(&fs::canonicalize(dir)?)?;
        ensure!(
            !mount.is_network() && !mount.fs_type.starts_with("fuse"),
            "Job journal must be on local storage. Set XDG_DATA_HOME to a local directory."
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("application.lock"))?;
        lock.try_lock_exclusive().context("Another Resolve Ingest instance or supervised worker is active. Wait for it to stop before retrying.")?;
        let mut db = Connection::open(dir.join("jobs.sqlite3"))?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(version <= 3, "Unsupported job database version");
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS batches (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, status TEXT NOT NULL, manifest TEXT NOT NULL); CREATE TABLE IF NOT EXISTS transitions (seq INTEGER PRIMARY KEY, batch_id TEXT NOT NULL, recorded_at TEXT NOT NULL, manifest TEXT NOT NULL);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS batch_items (batch_id TEXT NOT NULL, position INTEGER NOT NULL, manifest TEXT NOT NULL, PRIMARY KEY(batch_id, position));")?;
        // Verified originals by archive folder and source fingerprint, for skipping repeat ingests.
        db.execute_batch("CREATE TABLE IF NOT EXISTS archived (archive TEXT NOT NULL, fingerprint TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL, bytes INTEGER NOT NULL, batch_id TEXT NOT NULL, PRIMARY KEY(archive, fingerprint));")?;
        if version < 2 {
            let old: Vec<String> = db
                .prepare("SELECT manifest FROM batches")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let tx = db.transaction()?;
            for data in old {
                let mut batch: Batch = serde_json::from_str(&data)?;
                for (index, item) in batch.items.iter().enumerate() {
                    tx.execute(
                        "INSERT OR REPLACE INTO batch_items VALUES (?1, ?2, ?3)",
                        params![batch.id, index as i64, serde_json::to_string(item)?],
                    )?;
                }
                batch.items.clear();
                tx.execute(
                    "UPDATE batches SET manifest=?1 WHERE id=?2",
                    params![serde_json::to_string(&batch)?, batch.id],
                )?;
            }
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }
        if version < 3 {
            db.pragma_update(None, "user_version", 3)?;
        }
        let mut jobs = Self { db, lock };
        for mut batch in jobs
            .history()?
            .into_iter()
            .filter(|b| b.status == "Running")
        {
            batch.status = "Interrupted · ready for recovery".into();
            for item in &mut batch.items {
                for stage in [&mut item.archive, &mut item.editing] {
                    if matches!(
                        stage,
                        crate::model::Stage::Copying
                            | crate::model::Stage::Staging
                            | crate::model::Stage::Checking
                            | crate::model::Stage::Converting
                            | crate::model::Stage::AwaitingArchive
                    ) {
                        *stage = crate::model::Stage::Interrupted;
                    }
                }
            }
            jobs.save(&batch)?;
        }
        Ok(jobs)
    }
    pub fn lock_fd(&self) -> RawFd {
        self.lock.as_raw_fd()
    }
    pub fn save(&mut self, batch: &Batch) -> Result<()> {
        self.save_indices(batch, 0..batch.items.len())
    }
    pub fn save_item(&mut self, batch: &Batch, index: usize) -> Result<()> {
        self.save_indices(batch, std::iter::once(index))
    }
    fn save_indices(&mut self, batch: &Batch, indices: impl Iterator<Item = usize>) -> Result<()> {
        // Header is small; each transition stores only its affected item and preserves old evidence.
        // Avoid serializing/cloning all probes on every file-stage change in a large batch.
        let header = Batch {
            items: vec![],
            ..batch_header(batch)
        };
        let data = serde_json::to_string(&header)?;
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO batches VALUES (?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET status=excluded.status, manifest=excluded.manifest", params![batch.id, batch.created_at, batch.status, data])?;
        for index in indices {
            let item = serde_json::to_string(&batch.items[index])?;
            let changed = tx.execute("INSERT INTO batch_items VALUES (?1, ?2, ?3) ON CONFLICT(batch_id, position) DO UPDATE SET manifest=excluded.manifest WHERE batch_items.manifest != excluded.manifest", params![batch.id, index as i64, item])?;
            let entry = &batch.items[index];
            if let (
                Some(archive),
                Some(fingerprint),
                Some(evidence),
                crate::model::Stage::Verified,
            ) = (
                &batch.settings.archive,
                &entry.source.fingerprint,
                &entry.archive_evidence,
                &entry.archive,
            ) {
                tx.execute(
                    "INSERT OR REPLACE INTO archived VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        archive_key(&archive.path),
                        fingerprint,
                        entry.archive_path.to_string_lossy(),
                        evidence.sha256,
                        evidence.bytes as i64,
                        batch.id
                    ],
                )?;
            }
            if changed > 0 {
                let audit = serde_json::json!({"position": index, "item": &batch.items[index]});
                tx.execute(
                    "INSERT INTO transitions(batch_id, recorded_at, manifest) VALUES (?1, ?2, ?3)",
                    params![
                        batch.id,
                        crate::model::now(),
                        serde_json::to_string(&audit)?
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn load(&self, id: &str) -> Result<Batch> {
        let data: String =
            self.db
                .query_row("SELECT manifest FROM batches WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
        let mut batch: Batch = serde_json::from_str(&data)?;
        ensure!(batch.schema == 1, "Unsupported batch schema");
        let mut stmt = self
            .db
            .prepare("SELECT manifest FROM batch_items WHERE batch_id=?1 ORDER BY position")?;
        batch.items = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect::<Result<_>>()?;
        Ok(batch)
    }
    /// Archive-relative paths of verified originals in `archive`, keyed by source fingerprint.
    pub fn archived(&self, archive: &Path) -> Result<HashMap<String, (PathBuf, u64)>> {
        let mut stmt = self
            .db
            .prepare("SELECT fingerprint, path, bytes FROM archived WHERE archive=?1")?;
        let rows = stmt.query_map([archive_key(archive)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    PathBuf::from(r.get::<_, String>(1)?),
                    r.get::<_, i64>(2)? as u64,
                ),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub fn history(&self) -> Result<Vec<Batch>> {
        let mut stmt = self
            .db
            .prepare("SELECT id FROM batches ORDER BY created_at DESC")?;
        let records = stmt.query_map([], |r| r.get::<_, String>(0))?;
        records.map(|r| self.load(&r?)).collect()
    }
}
fn archive_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn batch_header(b: &Batch) -> Batch {
    Batch {
        schema: b.schema,
        id: b.id.clone(),
        created_at: b.created_at.clone(),
        settings: b.settings.clone(),
        profile: b.profile.clone(),
        tools: b.tools.clone(),
        items: vec![],
        skipped: b.skipped,
        status: b.status.clone(),
        archive_bytes: b.archive_bytes,
        editing_bytes: b.editing_bytes,
        storage_notes: b.storage_notes.clone(),
    }
}
