use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use rhai::{Array, Engine, EvalAltResult, ImmutableString, Position};

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;
const MAX_OPERATIONS: u64 = 1_000_000;

pub fn run(directory: &Path, name: &str, source: &str) -> Result<()> {
    let engine = engine(directory, MAX_OPERATIONS);
    let mut script = engine
        .compile(source.trim_start_matches('\u{feff}'))
        .map_err(|err| anyhow::anyhow!("script syntax: {err}"))?;
    script.set_source(name);
    engine
        .run_ast(&script)
        .map_err(|err| anyhow::anyhow!("script error: {err}"))
}

fn engine(directory: &Path, max_operations: u64) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(max_operations);
    engine.register_fn("host_os", || std::env::consts::OS);
    engine.register_fn("host_arch", || std::env::consts::ARCH);

    let root = directory.to_path_buf();
    engine.register_fn("path_exists", move |path: &str| -> ScriptResult<bool> {
        root.join(path)
            .try_exists()
            .map_err(|err| script_error(format!("check {path:?}: {err}")))
    });
    let root = directory.to_path_buf();
    engine.register_fn("read_text", move |path: &str| -> ScriptResult<String> {
        fs::read_to_string(root.join(path))
            .map_err(|err| script_error(format!("read {path:?}: {err}")))
    });
    let root = directory.to_path_buf();
    engine.register_fn(
        "append_text",
        move |path: &str, text: &str| -> ScriptResult<()> {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join(path))
                .and_then(|mut file| file.write_all(text.as_bytes()))
                .map_err(|err| script_error(format!("append to {path:?}: {err}")))
        },
    );
    let root = directory.to_path_buf();
    engine.register_fn(
        "run_command",
        move |program: &str, args: Array| -> ScriptResult<rhai::INT> {
            command(&root, program, args).map_err(|err| script_error(format!("{err:#}")))
        },
    );
    engine
}

fn command(directory: &Path, program: &str, args: Array) -> Result<rhai::INT> {
    let path = Path::new(program);
    anyhow::ensure!(
        path.is_absolute() || path.file_name() == Some(OsStr::new(program)),
        "use an absolute executable path or a bare name on PATH"
    );
    let argv = args
        .into_iter()
        .map(|arg| {
            arg.try_cast::<ImmutableString>()
                .context("run_command arguments must be strings")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut command = Command::new(program);
    command
        .current_dir(directory)
        .stdin(Stdio::null())
        .args(argv.iter().map(|s| s.as_str()));
    // The daemon handles terminal Ctrl-C and waits for any active job.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    let status = command
        .status()
        .with_context(|| format!("start {program:?}"))?;
    status
        .code()
        .map(rhai::INT::from)
        .with_context(|| format!("{program:?} terminated without an exit code: {status}"))
}

fn script_error(message: String) -> Box<EvalAltResult> {
    EvalAltResult::ErrorRuntime(message.into(), Position::NONE).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_functions_and_relative_paths_work() -> Result<()> {
        let root = tempfile::tempdir()?;
        run(
            root.path(),
            "test.rhai",
            "if path_exists(\"output\") { throw \"already exists\"; } append_text(\"output\", host_os() + \"/\" + host_arch());",
        )?;
        assert_eq!(
            fs::read_to_string(root.path().join("output"))?,
            format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
        );
        run(
            root.path(),
            "read.rhai",
            "if read_text(\"output\") != host_os() + \"/\" + host_arch() { throw \"unexpected content\"; }",
        )?;
        Ok(())
    }

    #[test]
    fn infinite_script_hits_operation_limit() -> Result<()> {
        let root = tempfile::tempdir()?;
        let error = engine(root.path(), 100)
            .run("let x = 0; loop { x += 1; }")
            .unwrap_err();
        assert!(matches!(*error, EvalAltResult::ErrorTooManyOperations(_)));
        Ok(())
    }

    #[test]
    fn bundled_examples_parse() -> Result<()> {
        let root = tempfile::tempdir()?;
        let engine = engine(root.path(), MAX_OPERATIONS);
        for source in [
            include_str!("../examples/hello.rhai"),
            include_str!("../examples/os-update.rhai"),
        ] {
            engine.compile(source)?;
        }
        Ok(())
    }

    #[test]
    fn os_update_preview_never_launches_a_package_manager() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = engine(root.path(), MAX_OPERATIONS);
        engine.register_fn("host_os", || "linux");
        engine.register_fn("path_exists", |path: &str| -> ScriptResult<bool> {
            Ok(matches!(path, "/etc/os-release" | "/usr/bin/pacman"))
        });
        engine.register_fn("read_text", |_: &str| -> ScriptResult<String> {
            Ok("ID=\"cachyos\"\nID_LIKE=arch\n".into())
        });
        engine.register_fn(
            "run_command",
            |_: &str, _: Array| -> ScriptResult<rhai::INT> {
                Err(script_error(
                    "preview should never invoke run_command".into(),
                ))
            },
        );
        let lines = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let captured = lines.clone();
        engine.on_print(move |line| captured.borrow_mut().push(line.to_owned()));
        engine
            .run(include_str!("../examples/os-update.rhai"))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(
            lines.borrow().as_slice(),
            [
                "Detected cachyos; apply updates: false",
                "/usr/bin/pacman -Syu --noconfirm"
            ]
        );
        Ok(())
    }

    #[test]
    fn command_arguments_are_type_checked_before_launch() -> Result<()> {
        let root = tempfile::tempdir()?;
        let error = run(root.path(), "bad.rhai", "run_command(\"unused\", [1]);").unwrap_err();
        assert!(error.to_string().contains("arguments must be strings"));
        Ok(())
    }
}
