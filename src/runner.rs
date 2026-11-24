use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use mlua::chunk::ChunkMode;
use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Value, VmState};
use rhai::{Array, Engine, EvalAltResult, ImmutableString, Position};

use crate::module::Language;

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;
const MAX_OPERATIONS: u64 = 1_000_000;

pub fn run(directory: &Path, name: &str, source: &str) -> Result<()> {
    let source = source.trim_start_matches('\u{feff}');
    match Language::from_path(Path::new(name)).context("unsupported script extension")? {
        Language::Rhai => run_rhai(directory, name, source),
        Language::Lua => {
            run_lua(directory, name, source).map_err(|err| anyhow::anyhow!("script error: {err}"))
        }
    }
}

fn run_rhai(directory: &Path, name: &str, source: &str) -> Result<()> {
    let engine = engine(directory, MAX_OPERATIONS);
    let mut script = engine
        .compile(source)
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
            append_text(&root.join(path), text.as_bytes())
                .map_err(|err| script_error(format!("append to {path:?}: {err}")))
        },
    );
    let root = directory.to_path_buf();
    engine.register_fn(
        "run_command",
        move |program: &str, args: Array| -> ScriptResult<rhai::INT> {
            let args = args
                .into_iter()
                .map(|arg| {
                    arg.try_cast::<ImmutableString>()
                        .ok_or_else(|| script_error("run_command arguments must be strings".into()))
                })
                .collect::<ScriptResult<Vec<_>>>()?;
            command(&root, program, args.iter().map(|arg| arg.as_str()))
                .map(rhai::INT::from)
                .map_err(|err| script_error(format!("{err:#}")))
        },
    );
    engine
}

fn run_lua(directory: &Path, name: &str, source: &str) -> mlua::Result<()> {
    let libraries = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
    let lua = Lua::new_with(libraries, LuaOptions::default())?;
    let globals = lua.globals();
    // Scripts use only the module source. A Lua protected call could catch the hook error.
    for name in ["load", "loadfile", "dofile", "pcall", "xpcall"] {
        globals.set(name, Value::Nil)?;
    }
    const HOOK_INTERVAL: u32 = 1_000;
    let remaining = Cell::new(MAX_OPERATIONS / u64::from(HOOK_INTERVAL));
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        move |_, _| {
            let ticks = remaining.get().saturating_sub(1);
            remaining.set(ticks);
            if ticks == 0 {
                Err(mlua::Error::runtime("Lua instruction limit exceeded"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )?;
    globals.set(
        "host_os",
        lua.create_function(|_, ()| Ok(std::env::consts::OS))?,
    )?;
    globals.set(
        "host_arch",
        lua.create_function(|_, ()| Ok(std::env::consts::ARCH))?,
    )?;

    let root = directory.to_path_buf();
    globals.set(
        "path_exists",
        lua.create_function(move |_, path: String| {
            root.join(&path)
                .try_exists()
                .map_err(|err| mlua::Error::runtime(format!("check {path:?}: {err}")))
        })?,
    )?;
    let root = directory.to_path_buf();
    globals.set(
        "read_text",
        lua.create_function(move |_, path: String| {
            fs::read_to_string(root.join(&path))
                .map_err(|err| mlua::Error::runtime(format!("read {path:?}: {err}")))
        })?,
    )?;
    let root = directory.to_path_buf();
    globals.set(
        "append_text",
        lua.create_function(move |_, (path, text): (String, String)| {
            append_text(&root.join(&path), text.as_bytes())
                .map_err(|err| mlua::Error::runtime(format!("append to {path:?}: {err}")))
        })?,
    )?;
    let root = directory.to_path_buf();
    globals.set(
        "run_command",
        lua.create_function(move |_, (program, args): (String, mlua::Table)| {
            let args = lua_command_args(args)?;
            command(&root, &program, args.values().map(String::as_str))
                .map_err(|err| mlua::Error::runtime(format!("{err:#}")))
        })?,
    )?;

    lua.load(source)
        .set_name(name)
        .set_mode(ChunkMode::Text)
        .exec()
}

fn lua_command_args(table: mlua::Table) -> mlua::Result<BTreeMap<mlua::Integer, String>> {
    let invalid = || mlua::Error::runtime("run_command arguments must be a list of strings");
    let mut args = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (Value::Integer(index), Value::String(arg)) = pair? else {
            return Err(invalid());
        };
        args.insert(index, arg.to_str()?.to_string());
    }
    if !args.keys().copied().eq((1..).take(args.len())) {
        return Err(invalid());
    }
    Ok(args)
}

fn append_text(path: &Path, text: &[u8]) -> std::io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text)
}

fn command<'a>(
    directory: &Path,
    program: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<i32> {
    let path = Path::new(program);
    anyhow::ensure!(
        path.is_absolute() || path.file_name() == Some(OsStr::new(program)),
        "use an absolute executable path or a bare name on PATH"
    );
    let mut command = Command::new(program);
    command
        .current_dir(directory)
        .stdin(Stdio::null())
        .args(args);
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
    fn lua_host_helpers_use_module_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        run(
            root.path(),
            "test.lua",
            "\u{feff}assert(not path_exists('output'))\n\
             append_text('output', host_os() .. '/' .. host_arch())\n\
             assert(path_exists('output'))\n\
             assert(read_text('output') == host_os() .. '/' .. host_arch())",
        )?;
        assert_eq!(
            fs::read_to_string(root.path().join("output"))?,
            format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
        );
        run(
            root.path(),
            "hello.lua",
            include_str!("../examples/hello.lua"),
        )
    }

    #[test]
    fn lua_stops_infinite_scripts() -> Result<()> {
        let root = tempfile::tempdir()?;
        let error = run(
            root.path(),
            "loop.lua",
            "assert(pcall == nil and xpcall == nil and coroutine == nil)\nwhile true do end",
        )
        .unwrap_err();
        assert!(error.to_string().contains("Lua instruction limit exceeded"));
        Ok(())
    }

    #[test]
    fn lua_validates_command_arguments_before_launch() -> Result<()> {
        let root = tempfile::tempdir()?;
        for args in ["{1}", "{[2] = 'gap'}", "{name = 'value'}"] {
            let error = run(
                root.path(),
                "bad.lua",
                &format!("run_command('unused', {args})"),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("arguments must be a list of strings")
            );
        }
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
