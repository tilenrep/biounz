use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use croner::Cron;

const MAX_SOURCE_BYTES: usize = 1024 * 1024;

pub struct Module {
    pub source: String,
    pub digest: String,
    pub schedule: Schedule,
    pub first_at: i64,
}

pub enum Schedule {
    Now,
    Cron(Box<Cron>),
}

impl Schedule {
    fn parse(source: &str) -> Result<Self> {
        let mut directive = None;
        for line in source.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if line.is_empty() {
                continue;
            }
            let Some(comment) = line.strip_prefix("//") else {
                break;
            };
            if let Some(value) = comment.trim_start().strip_prefix("schedule:") {
                ensure!(directive.is_none(), "multiple schedule directives");
                directive = Some(value.trim());
            }
        }

        match directive.context("missing `// schedule: ...` in the file header")? {
            "now" => Ok(Self::Now),
            expression => {
                ensure!(
                    expression.split_whitespace().count() == 5,
                    "expected `now` or five cron fields: minute hour day-of-month month day-of-week"
                );
                Ok(Self::Cron(Box::new(
                    expression.parse().context("invalid cron schedule")?,
                )))
            }
        }
    }

    pub fn next_after(&self, now: DateTime<Utc>) -> Result<Option<i64>> {
        match self {
            Self::Now => Ok(None),
            Self::Cron(cron) => Ok(Some(
                cron.find_next_occurrence(&now, false)
                    .context("cron schedule has no reachable next occurrence")?
                    .timestamp(),
            )),
        }
    }
}

impl Module {
    pub fn parse(source: String, now: DateTime<Utc>) -> Result<Self> {
        let schedule = Schedule::parse(&source)?;
        let first_at = schedule.next_after(now)?.unwrap_or_else(|| now.timestamp());
        let digest = blake3::hash(source.as_bytes()).to_hex().to_string();
        Ok(Self {
            source,
            digest,
            schedule,
            first_at,
        })
    }
}

#[derive(Default)]
pub struct Scan {
    pub modules: BTreeMap<String, Module>,
    pub errors: BTreeMap<String, String>,
}

pub fn scan(directory: &Path, now: DateTime<Utc>) -> Result<Scan> {
    let mut scan = Scan::default();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(scan),
        Err(error) => return Err(error).with_context(|| format!("read {}", directory.display())),
    };

    for entry in entries {
        let entry = entry.context("read module directory entry")?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("rhai")) || !entry.file_type()?.is_file() {
            continue;
        }
        let filename = entry.file_name();
        let Some(name) = filename.to_str() else {
            scan.errors
                .insert(format!("{filename:?}"), "filename is not UTF-8".into());
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        match load_source(&path)?.and_then(|result| Module::parse(result, now)) {
            Ok(module) => {
                scan.modules.insert(name.into(), module);
            }
            Err(error) => {
                scan.errors.insert(name.into(), format!("{error:#}"));
            }
        }
    }
    Ok(scan)
}

// An I/O failure invalidates this scan. A malformed source only disables that file.
fn load_source(path: &Path) -> Result<Result<String>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take((MAX_SOURCE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .with_context(|| format!("read module {}", path.display()))?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Ok(Err(anyhow::anyhow!("source exceeds 1 MiB")));
    }
    Ok(String::from_utf8(bytes).context("source is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn validates_metadata() {
        assert!(matches!(
            Schedule::parse("\u{feff}// description\r\n// schedule: now\r\nfn main() {}\r\n")
                .unwrap(),
            Schedule::Now
        ));
        for text in [
            "fn main() {}\n// schedule: now",
            "// schedule: now\n// schedule: * * * * *",
            "// schedule: 0 * * * * *",
            "// schedule: 61 * * * *",
            "// schedule: @daily",
        ] {
            assert!(Schedule::parse(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn cron_uses_utc_and_excludes_current_instant() {
        let monday = Utc.with_ymd_and_hms(2026, 1, 5, 2, 30, 0).unwrap();
        let schedule = Schedule::parse("// schedule: 30 2 * * MON").unwrap();
        assert_eq!(
            schedule.next_after(monday).unwrap(),
            Some(monday.timestamp() + 7 * 86400)
        );
    }

    #[test]
    fn scan_only_accepts_regular_script_modules() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("good.rhai"), "// schedule: now\nprint(42);")?;
        fs::write(dir.path().join("bad.rhai"), "print(42);")?;
        fs::write(dir.path().join(".partial.rhai"), "// schedule: now")?;
        fs::write(dir.path().join("legacy.rs"), "// schedule: now")?;
        fs::create_dir(dir.path().join("nested.rhai"))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("good.rhai", dir.path().join("link.rhai"))?;
        let result = scan(dir.path(), Utc::now())?;
        assert_eq!(
            result
                .modules
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["good.rhai"]
        );
        assert_eq!(
            result.errors.keys().map(String::as_str).collect::<Vec<_>>(),
            ["bad.rhai"]
        );
        Ok(())
    }
}
