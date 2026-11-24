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
