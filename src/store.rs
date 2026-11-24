use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use crate::module::Module;

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

    pub fn reconcile(&mut self, modules: &BTreeMap<String, Module>) -> Result<()> {
        let tx = self.0.transaction()?;
        let mut known = {
            let mut query = tx.prepare("SELECT name, digest FROM schedules")?;
            query
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        for (name, module) in modules {
            if known.remove(name).as_deref() != Some(module.digest.as_str()) {
                tx.execute(
                    "INSERT INTO schedules (name, digest, next_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(name) DO UPDATE SET digest = excluded.digest, next_at = excluded.next_at",
                    params![name, module.digest, module.first_at],
                )?;
            }
        }
        for name in known.keys() {
            tx.execute("DELETE FROM schedules WHERE name = ?1", [name])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn due(&self, now: i64) -> Result<Option<Due>> {
        let next = self.0.query_row(
            "SELECT name, digest, next_at FROM schedules WHERE next_at <= ?1 ORDER BY next_at, name LIMIT 1",
            [now],
            |row| Ok(Due { name: row.get(0)?, digest: row.get(1)?, at: row.get(2)? }),
        ).optional()?;
        Ok(next)
    }

    // Persist the claim before invoking rustc or any module code.
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
    use chrono::{DateTime, Utc};

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    #[test]
    fn now_survives_restart_and_reschedules_after_edit_or_removal() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = tmp.path().join("test.sqlite3");
        let mut modules = BTreeMap::from([(
            "job.rhai".to_owned(),
            Module::parse("// schedule: now".into(), at(100))?,
        )]);
        let mut store = Store::open(&db, tmp.path())?;
        store.reconcile(&modules)?;
        assert!(store.due(99)?.is_none());
        store.claim(&store.due(100)?.unwrap(), None)?;
        drop(store);

        let mut store = Store::open(&db, tmp.path())?;
        store.reconcile(&modules)?;
        assert!(store.due(200)?.is_none());
        modules.insert(
            "job.rhai".into(),
            Module::parse("// schedule: now\n// edited".into(), at(200))?,
        );
        store.reconcile(&modules)?;
        let due = store.due(200)?.unwrap();
        store.claim(&due, None)?;
        assert!(store.claim(&due, None).is_err());
        store.reconcile(&BTreeMap::new())?;
        store.reconcile(&modules)?;
        assert!(store.due(201)?.is_some());
        Ok(())
    }

    #[test]
    fn cron_misses_coalesce_to_one_run_on_restart() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = tmp.path().join("test.sqlite3");
        let mut modules = BTreeMap::from([(
            "job.rhai".to_owned(),
            Module::parse("// schedule: * * * * *".into(), at(1))?,
        )]);
        let mut store = Store::open(&db, tmp.path())?;
        store.reconcile(&modules)?;
        assert!(store.due(59)?.is_none());
        drop(store);

        let mut store = Store::open(&db, tmp.path())?;
        let module = Module::parse("// schedule: * * * * *".into(), at(350))?;
        let next = module.schedule.next_after(at(350))?;
        modules.insert("job.rhai".into(), module);
        store.reconcile(&modules)?;
        let overdue = store.due(350)?.unwrap();
        assert_eq!(overdue.at, 60);
        store.claim(&overdue, next)?;
        assert!(store.due(359)?.is_none());
        assert_eq!(store.due(360)?.unwrap().at, 360);
        assert!(Store::open(&db, &tmp.path().join("elsewhere")).is_err());
        Ok(())
    }
}
