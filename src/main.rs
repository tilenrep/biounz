mod module;
mod runner;
mod store;

use std::fs::{self, DirBuilder, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use clap::Parser;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::module::Scan;
use crate::store::Store;

const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Watch standalone Rhai (.rhai) scripts in this directory.
    #[arg(long, default_value = "modules")]
    modules: PathBuf,

    /// Store the SQLite database and lock here (private directory).
    #[arg(long, default_value = ".moduled")]
    state: PathBuf,
}

fn main() -> ExitCode {
    match serve(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("moduled: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: Args) -> Result<()> {
    fs::create_dir_all(&args.modules)
        .with_context(|| format!("create modules directory {}", args.modules.display()))?;
    let directory = args
        .modules
        .canonicalize()
        .context("resolve modules directory")?;
    let state = open_state_dir(&args.state)?;
    ensure!(
        state != directory,
        "the modules and state directories must differ"
    );
    let _lock = lock_state(&state)?;
    let mut store = Store::open(&state.join("schedule.sqlite3"), &directory)
        .context("open schedule database")?;

    let stopping = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stopping);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .context("install shutdown signal handler")?;

    let (events, wake) = mpsc::sync_channel(1);
    let _watcher = match watch(&directory, events.clone()) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            eprintln!("moduled: filesystem watch unavailable, using periodic scans: {error:#}");
            None
        }
    };
    eprintln!(
        "moduled: watching {} (UTC); state in {}",
        directory.display(),
        state.display()
    );

    let mut worker = None;
    let result = schedule(&directory, &mut store, &wake, &stopping, &mut worker);
    if let Some(job) = worker {
        eprintln!("moduled: waiting for active job before shutdown");
        join(job);
    }
    result
}

fn open_state_dir(path: &Path) -> Result<PathBuf> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("create state directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect state directory {}", path.display()))?;
    ensure!(
        metadata.is_dir(),
        "{} must be a real directory",
        path.display()
    );
    #[cfg(unix)]
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "{} must have no group or other permissions (chmod 700)",
        path.display()
    );
    path.canonicalize().context("resolve state directory")
}

fn lock_state(state: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let lock = options
        .open(state.join("daemon.lock"))
        .context("open daemon lock")?;
    lock.try_lock()
        .context("lock state directory (is another daemon running?)")?;
    Ok(lock)
}

fn watch(directory: &Path, sender: SyncSender<()>) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        match event {
            Ok(event) if event.kind.is_access() => return,
            Err(error) => eprintln!("moduled: watch error: {error}"),
            _ => (),
        }
        let _ = sender.try_send(());
    })?;
    watcher.watch(directory, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

fn schedule(
    directory: &Path,
    store: &mut Store,
    wake: &Receiver<()>,
    stopping: &AtomicBool,
    worker: &mut Option<JoinHandle<()>>,
) -> Result<()> {
    let mut current = Scan::default();
    let mut last_scan_error = None;
    let mut scan_again = true;
    let mut next_scan = Instant::now();
    while !stopping.load(Ordering::Relaxed) {
        if scan_again || Instant::now() >= next_scan {
            match module::scan(directory, Utc::now()) {
                Ok(scan) => {
                    for (name, error) in &scan.errors {
                        if current.errors.get(name) != Some(error) {
                            eprintln!("moduled: ignoring {name:?}: {error}");
                        }
                    }
                    store
                        .reconcile(&scan.modules)
                        .context("reconcile module schedules")?;
                    current = scan;
                    last_scan_error = None;
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if last_scan_error.as_ref() != Some(&message) {
                        eprintln!("moduled: scan failed; pausing dispatch: {message}");
                    }
                    last_scan_error = Some(message);
                }
            }
            next_scan = Instant::now() + RESCAN_INTERVAL;
        }
        if worker.as_ref().is_some_and(|job| job.is_finished()) {
            join(worker.take().expect("worker just checked"));
        }
        let now = Utc::now();
        if worker.is_none()
            && last_scan_error.is_none()
            && !stopping.load(Ordering::Relaxed)
            && let Some(due) = store.due(now.timestamp())?
        {
            let job = current
                .modules
                .get(&due.name)
                .context("missing scheduled module snapshot")?;
            ensure!(
                job.digest == due.digest,
                "revision mismatch for {:?}",
                due.name
            );
            let next = job.schedule.next_after(now)?;
            store.claim(&due, next).context("persist dispatch claim")?;
            let name = due.name;
            let source = job.source.clone();
            let directory = directory.to_path_buf();
            *worker = Some(
                thread::Builder::new()
                    .name("module-worker".into())
                    .spawn(move || {
                        eprintln!("moduled: running {name:?}");
                        match runner::run(&directory, &name, &source) {
                            Ok(()) => eprintln!("moduled: completed {name:?}"),
                            Err(error) => eprintln!("moduled: {name:?}: {error:#}"),
                        }
                    })
                    .context("start module worker")?,
            );
        }
        scan_again = wake.recv_timeout(TICK).is_ok();
    }
    Ok(())
}

fn join(job: JoinHandle<()>) {
    if job.join().is_err() {
        eprintln!("moduled: module worker panicked");
    }
}
