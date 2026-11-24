use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Daemon(Child);

impl Daemon {
    fn start(root: &Path) -> Result<Self> {
        Ok(Self(
            Command::new(env!("CARGO_BIN_EXE_moduled"))
                .current_dir(root)
                // No rustc, Cargo, or interpreter executable is available on PATH.
                .env("PATH", "")
                .spawn()?,
        ))
    }

    fn wait_for(&mut self, marker: &Path, expected: &str) -> Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if fs::read_to_string(marker).is_ok_and(|text| text == expected) {
                return Ok(());
            }
            ensure!(
                self.0.try_wait()?.is_none(),
                "daemon exited while waiting for {marker:?}"
            );
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for {marker:?}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(unix)]
    fn stop(mut self) -> Result<()> {
        let status = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status()?;
        ensure!(status.success(), "could not send SIGTERM");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait()? {
                ensure!(
                    status.success(),
                    "daemon did not shut down cleanly: {status}"
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "daemon did not stop after SIGTERM"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(windows)]
    fn stop(mut self) -> Result<()> {
        // Windows has no per-process POSIX SIGTERM. Check persistence after a forced stop.
        self.0.kill()?;
        self.0.wait()?;
        Ok(())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn deploy(modules: &Path, name: &str, output: &str, line: &str) -> Result<()> {
    let source = format!("// schedule: now\nappend_text(\"../{output}\", {line:?} + \"\\n\");\n");
    let stage = modules.join(".staging.rhai");
    fs::write(&stage, source)?;
    fs::rename(stage, modules.join(name))?;
    Ok(())
}

#[test]
fn lua_and_rhai_jobs_run_without_an_external_interpreter() -> Result<()> {
    let root = tempfile::tempdir()?;
    let modules = root.path().join("modules");
    fs::create_dir(&modules)?;
    fs::write(
        modules.join("job.lua"),
        "-- schedule: now\nappend_text('../lua-output', 'lua\\n')",
    )?;
    deploy(&modules, "job.rhai", "rhai-output", "rhai")?;
    let mut daemon = Daemon::start(root.path())?;
    daemon.wait_for(&root.path().join("lua-output"), "lua\n")?;
    daemon.wait_for(&root.path().join("rhai-output"), "rhai\n")?;
    daemon.stop()
}

#[test]
fn added_and_edited_modules_run_but_an_unchanged_module_stays_consumed() -> Result<()> {
    let root = tempfile::tempdir()?;
    let modules = root.path().join("modules");
    let output = root.path().join("output");
    let ready = root.path().join("ready");
    fs::create_dir(&modules)?;
    deploy(&modules, "ready.rhai", "ready", "started")?;
    let mut daemon = Daemon::start(root.path())?;
    daemon.wait_for(&ready, "started\n")?;

    deploy(&modules, "job.rhai", "output", "first")?;
    daemon.wait_for(&output, "first\n")?;
    deploy(&modules, "job.rhai", "output", "second")?;
    daemon.wait_for(&output, "first\nsecond\n")?;
    daemon.stop()?;

    let done = root.path().join("done");
    deploy(&modules, "zz-sentinel.rhai", "done", "restarted")?;
    let mut daemon = Daemon::start(root.path())?;
    daemon.wait_for(&done, "restarted\n")?;
    daemon.stop()?;
    assert_eq!(fs::read_to_string(output)?, "first\nsecond\n");
    assert_eq!(fs::read_to_string(ready)?, "started\n");
    Ok(())
}

#[cfg(unix)]
#[test]
fn unreadable_module_does_not_block_other_jobs() -> Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir()?;
    let modules = root.path().join("modules");
    fs::create_dir(&modules)?;
    deploy(&modules, "blocked.rhai", "blocked", "ran")?;
    deploy(&modules, "ready.rhai", "ready", "ran")?;
    let blocked = modules.join("blocked.rhai");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000))?;
    match fs::File::open(&blocked) {
        Ok(_) => return Ok(()), // A privileged test runner can bypass the mode bits.
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error.into()),
    }

    let mut daemon = Daemon::start(root.path())?;
    daemon.wait_for(&root.path().join("ready"), "ran\n")?;
    assert!(!root.path().join("blocked").exists());

    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o600))?;
    daemon.wait_for(&root.path().join("blocked"), "ran\n")?;
    daemon.stop()
}

#[test]
fn restart_corrects_a_saved_cron_occurrence_that_no_longer_matches() -> Result<()> {
    use chrono::{DateTime, Datelike, Timelike, Utc};
    use rusqlite::{Connection, params};

    let root = tempfile::tempdir()?;
    let modules = root.path().join("modules");
    fs::create_dir(&modules)?;
    let ready = root.path().join("ready");
    deploy(&modules, "ready.rhai", "ready", "started")?;
    let now = Utc::now().timestamp();
    let stale = DateTime::from_timestamp(now.div_euclid(60) * 60 - 60, 0).unwrap();
    let day = if stale.day() <= 14 { 28 } else { 14 };
    let source = format!(
        "// schedule: {} {} {day} * */1\nappend_text(\"../unexpected\", \"ran\");",
        stale.minute(),
        stale.hour()
    );
    fs::write(modules.join("job.rhai"), source)?;
    let mut daemon = Daemon::start(root.path())?;
    daemon.wait_for(&ready, "started\n")?;
    daemon.stop()?;

    let db = Connection::open(root.path().join(".moduled/schedule.sqlite3"))?;
    assert_eq!(
        db.execute(
            "UPDATE schedules SET next_at = ?1 WHERE name = 'job.rhai'",
            params![stale.timestamp()],
        )?,
        1
    );
    drop(db);
    deploy(&modules, "ready.rhai", "ready", "restarted")?;
    let mut daemon = Daemon::start(root.path())?;
    daemon.wait_for(&ready, "started\nrestarted\n")?;
    daemon.stop()?;

    assert!(!root.path().join("unexpected").exists());
    let db = Connection::open(root.path().join(".moduled/schedule.sqlite3"))?;
    let next: i64 = db.query_row(
        "SELECT next_at FROM schedules WHERE name = 'job.rhai'",
        [],
        |row| row.get(0),
    )?;
    assert!(next > now);
    assert_eq!(DateTime::from_timestamp(next, 0).unwrap().day(), day);
    Ok(())
}
