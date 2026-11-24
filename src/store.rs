use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use rusqlite::{Connection, params};

use crate::module::Scan;

pub struct Store(Connection);

pub struct Due {
    pub name: String,
    pub digest: String,
    pub at: i64,
}

impl Store {
    pub fn open(path: &Path, directory: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                directory BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS schedules (
                name TEXT PRIMARY KEY NOT NULL,
                digest TEXT NOT NULL,
                next_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS schedules_due ON schedules(next_at, name);",
        )?;
        let directory = directory.as_os_str().as_encoded_bytes();
        db.execute("INSERT OR IGNORE INTO config VALUES (1, ?1)", [directory])?;
        let configured: Vec<u8> =
            db.query_row("SELECT directory FROM config WHERE id = 1", [], |r| {
                r.get(0)
            })?;
        ensure!(
            configured == directory,
            "this state directory belongs to a different modules directory"
        );
        Ok(Self(db))
    }

    pub fn reconcile(&mut self, scan: &Scan) -> Result<()> {
        let tx = self.0.transaction()?;
        let mut known = {
            let mut query = tx.prepare("SELECT name, digest FROM schedules")?;
            query
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        for (name, module) in &scan.modules {
            if known.remove(name).as_deref() != Some(module.digest.as_str()) {
                tx.execute(
                    "INSERT INTO schedules (name, digest, next_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(name) DO UPDATE SET digest = excluded.digest, next_at = excluded.next_at",
                    params![name, module.digest, module.first_at],
                )?;
            }
        }
        for name in known.keys() {
            if !scan.unreadable.contains(name) {
                tx.execute("DELETE FROM schedules WHERE name = ?1", [name])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn due(&self, now: i64, scan: &Scan) -> Result<Option<Due>> {
        let mut query = self.0.prepare(
            "SELECT name, digest, next_at FROM schedules WHERE next_at <= ?1 ORDER BY next_at, name",
        )?;
        let mut rows = query.query([now])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            if !scan.unreadable.contains(&name) {
                return Ok(Some(Due {
                    name,
                    digest: row.get(1)?,
                    at: row.get(2)?,
                }));
            }
        }
        Ok(None)
    }

    // Persist the claim before running module code.
    pub fn claim(&self, due: &Due, next_at: Option<i64>) -> Result<()> {
        let changes = self.0.execute(
            "UPDATE schedules SET next_at = ?1 WHERE name = ?2 AND digest = ?3 AND next_at = ?4",
            params![next_at, due.name, due.digest, due.at],
        )?;
        ensure!(
            changes == 1,
            "schedule changed before dispatch for {:?}",
            due.name
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::Module;
    use chrono::{DateTime, Utc};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    #[test]
    fn now_survives_restart_and_reschedules_after_edit_or_removal() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = tmp.path().join("test.sqlite3");
        let mut scan = Scan {
            modules: BTreeMap::from([(
                "job.rhai".to_owned(),
                Module::parse("// schedule: now".into(), at(100))?,
            )]),
            ..Scan::default()
        };
        let mut store = Store::open(&db, tmp.path())?;
        store.reconcile(&scan)?;
        assert!(store.due(99, &scan)?.is_none());
        store.claim(&store.due(100, &scan)?.unwrap(), None)?;
        drop(store);

        let mut store = Store::open(&db, tmp.path())?;
        store.reconcile(&scan)?;
        assert!(store.due(200, &scan)?.is_none());
        scan.modules.insert(
            "job.rhai".into(),
            Module::parse("// schedule: now\n// edited".into(), at(200))?,
        );
        store.reconcile(&scan)?;
        let due = store.due(200, &scan)?.unwrap();
        store.claim(&due, None)?;
        assert!(store.claim(&due, None).is_err());
        store.reconcile(&Scan::default())?;
        store.reconcile(&scan)?;
        assert!(store.due(201, &scan)?.is_some());
        Ok(())
    }

    #[test]
    fn cron_misses_coalesce_to_one_run_on_restart() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = tmp.path().join("test.sqlite3");
        let mut scan = Scan {
            modules: BTreeMap::from([(
                "job.rhai".to_owned(),
                Module::parse("// schedule: * * * * *".into(), at(1))?,
            )]),
            ..Scan::default()
        };
        let mut store = Store::open(&db, tmp.path())?;
        store.reconcile(&scan)?;
        assert!(store.due(59, &scan)?.is_none());
        drop(store);

        let mut store = Store::open(&db, tmp.path())?;
        let module = Module::parse("// schedule: * * * * *".into(), at(350))?;
        let next = module.schedule.next_after(at(350))?;
        scan.modules.insert("job.rhai".into(), module);
        store.reconcile(&scan)?;
        let overdue = store.due(350, &scan)?.unwrap();
        assert_eq!(overdue.at, 60);
        store.claim(&overdue, next)?;
        assert!(store.due(359, &scan)?.is_none());
        assert_eq!(store.due(360, &scan)?.unwrap().at, 360);
        assert!(Store::open(&db, &tmp.path().join("elsewhere")).is_err());
        Ok(())
    }

    #[test]
    fn unreadable_modules_keep_their_schedule_without_blocking_other_jobs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut store = Store::open(&tmp.path().join("test.sqlite3"), tmp.path())?;
        let mut scan = Scan::default();
        for name in ["completed.rhai", "pending.rhai"] {
            scan.modules.insert(
                name.into(),
                Module::parse("// schedule: now".into(), at(100))?,
            );
        }
        store.reconcile(&scan)?;
        let completed = store.due(100, &scan)?.unwrap();
        assert_eq!(completed.name, "completed.rhai");
        store.claim(&completed, None)?;

        let healthy = Scan {
            modules: BTreeMap::from([(
                "healthy.rhai".into(),
                Module::parse("// schedule: now".into(), at(200))?,
            )]),
            unreadable: scan.modules.keys().cloned().collect(),
            ..Scan::default()
        };
        store.reconcile(&healthy)?;
        let due = store.due(200, &healthy)?.unwrap();
        assert_eq!(due.name, "healthy.rhai");
        store.claim(&due, None)?;
        assert!(store.due(200, &healthy)?.is_none());

        store.reconcile(&scan)?;
        let pending = store.due(300, &scan)?.unwrap();
        assert_eq!(pending.name, "pending.rhai");
        assert_eq!(pending.at, 100);
        store.claim(&pending, None)?;
        assert!(store.due(300, &scan)?.is_none());
        Ok(())
    }
}
