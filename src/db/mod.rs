use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, TransactionBehavior};

// note: one connection behind a std Mutex, held only for short synchronous queries;
// move to a pool + spawn_blocking if query volume ever grows.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 4;

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.set_prepared_statement_cache_capacity(128);
        // WAL lets other processes read while one writes; NORMAL sync is durable in WAL mode except on power loss.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        for (pragma, value) in [("synchronous", "NORMAL"), ("foreign_keys", "ON"), ("temp_store", "MEMORY"), ("cache_size", "-16000"), ("mmap_size", "268435456")] {
            conn.pragma_update(None, pragma, value)?;
        }

        // IMMEDIATE so two processes starting together cannot both create the schema.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match tx.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            other => anyhow::bail!(
                "{} has schema version {other}, this build uses {SCHEMA_VERSION} (a pre-release library): move the file                  aside or delete it to start a fresh one",
                path.display()
            ),
        }
        tx.commit()?;
        conn.execute_batch("PRAGMA optimize = 0x10002")?;

        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Short random id for jobs and batches.
pub fn new_id() -> String {
    let mut id = uuid::Uuid::new_v4().simple().to_string();
    id.truncate(12);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_schema_reopens_and_refuses_other_versions() {
        let dir = std::env::temp_dir().join(format!("ai-media-router-db-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let db = Db::open(&path).unwrap();
        let plan: String = db
            .conn()
            .query_row("EXPLAIN QUERY PLAN SELECT id FROM jobs WHERE status = 'running' AND lease_until <= 5", [], |r| r.get(3))
            .unwrap();
        assert!(plan.contains("jobs_due"), "the worker's claim uses the partial index: {plan}");
        drop(db);
        Db::open(&path).unwrap();
        Connection::open(&path).unwrap().pragma_update(None, "user_version", 7).unwrap();
        let err = Db::open(&path).err().expect("old schema refused").to_string();
        assert!(err.contains("schema version 7") && err.contains("move the file"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
