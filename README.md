# moduled

`moduled` watches a directory for plain-text [Rhai](https://rhai.rs/) scripts and runs them on a schedule. The interpreter and SQLite are embedded in the daemon. **A deployed host does not need Rust, Cargo, SQLite, or a separate Rhai interpreter.** There is no dependency on system crontab.

Use a `moduled` executable built for your OS and CPU: Linux, macOS, or Windows. The same `.rhai` files work on each as long as they use portable operations or check the platform before calling OS-specific tools. It is not one universal binary; the executable still needs a compatible OS version and system ABI (such as glibc for a standard Linux GNU build). The optional [Native builds workflow](.github/workflows/build.yml) tests and packages builds on those three platforms when run manually.

## Quick start

To build from source, install Rust 1.89 or later and a native C compiler/linker (used to build bundled SQLite):

```sh
cargo build --locked --release
```

Run the built `target/release/moduled` (or `target\release\moduled.exe` on Windows). With a prebuilt copy, run `./moduled` or `.\moduled.exe` instead. The daemon creates `modules/` if needed, then watches it. From another terminal, copy a script into it:

```sh
cp examples/hello.rhai modules/.hello.tmp
mv modules/.hello.tmp modules/hello.rhai
```

In PowerShell:

```powershell
Copy-Item examples/hello.rhai modules/.hello.tmp
Move-Item modules/.hello.tmp modules/hello.rhai
```

The daemon prints a greeting with the host OS and architecture. Use `--modules PATH` and `--state PATH` to override `modules/` and `.moduled/`, which are relative to the daemon's working directory by default. Stop with Ctrl-C, or SIGTERM on Unix. The daemon stops dispatching new jobs and waits for the active one. It stays in the foreground; use a service supervisor appropriate to the host for unattended operation.

## Write a script

A module is a UTF-8 `.rhai` file, up to 1 MiB, with one schedule directive in the opening `//` comment block. There is no `main` function or build step: top-level statements run directly. Standalone scripts do not import other Rhai modules. Old `.rs` modules are ignored; rewrite their logic in Rhai and save them as `.rhai` files rather than just renaming them.

```rhai
// schedule: 30 3 * * SUN

print("Running on " + host_os() + "/" + host_arch());
append_text("runs.log", "weekly job ran\n");
```

- **`// schedule: now`** — runs when first seen and again when the file contents change. Touching the file or restarting the daemon does not rerun an unchanged script. If the daemon observes removal or invalid schedule metadata, a subsequent valid file is treated as newly added.
- **`// schedule: <minute> <hour> <day-of-month> <month> <day-of-week>`** — five-field cron syntax in **UTC**. Standard expressions follow Cronie's day rule: if either day field starts with `*` (including steps such as `*/2`), both day fields must match. Otherwise, either day field can match. A new or edited cron module starts at the next match. Missed occurrences while the daemon is offline or busy are combined into one catch-up run.

The daemon ignores dot-prefixed files, symlinks, and subdirectories. Save changes to a temporary file and rename it into place to avoid partial reads. Removing a module cancels future runs, not a run already dispatched. The watcher also rescans periodically in case it misses events.

If a script cannot be read, the daemon skips that script and keeps its last known schedule. Other scripts continue to run. Once the file is readable again, an unchanged script resumes from its saved state.

### Host functions

| Function | Behavior |
| --- | --- |
| `print(value)` | Print to the daemon's stdout. |
| `host_os()`, `host_arch()` | Return Rust platform identifiers, e.g. `linux`, `macos`, `windows`, `x86_64`, `aarch64`. |
| `path_exists(path)` | Return whether a path exists; surface I/O errors as script errors. |
| `read_text(path)` | Read a UTF-8 file. |
| `append_text(path, text)` | Append UTF-8 text, creating the file if needed. |
| `run_command(program, args)` | Run a host executable with a string array of arguments and wait for its numeric exit code. Use an absolute executable path or a bare name on `PATH`. The daemon does not insert a shell. |

Relative file paths and the working directory of child commands use the watched directory. Child commands inherit the daemon's environment, stdout, and stderr, but aren't given interactive stdin. A missing executable or a process terminated without an exit code raises a script error. A nonzero exit code is returned to the script for it to handle.

## Execution and trust

Scripts run in the daemon process on a worker thread, one at a time. The watcher stays active. There is an interpreter operation limit to stop infinite script loops, but it does not impose a wall-clock timeout on native calls or child processes.

**Scripts are trusted code, not sandboxed plugins.** The file and command functions operate with the daemon account's permissions. Keep both the module directory and the state directory under your control, and run as a dedicated unprivileged account by default. On Unix the state directory must have no group or other permissions (`chmod 700`); on Windows choose a private directory with appropriate ACLs.

The daemon locks the state directory to prevent competing instances from using the same DB. Each DB is tied to the absolute path of its watched directory. A dispatch is recorded in `schedule.sqlite3` **before** evaluation. Failed or interrupted jobs are not replayed automatically; a cron job waits for its next scheduled occurrence. A crash after recording the dispatch can therefore lose that run.

## OS update example

[`examples/os-update.rhai`](examples/os-update.rhai) is not enabled by default. It checks the platform and skips non-Linux hosts. On Linux it reads `os-release` and checks the expected executable for exact Debian, Ubuntu, Fedora, Arch, and CachyOS IDs. It does not guess from `ID_LIKE` and rejects OSTree systems.

The example only prints the proposed commands unless you set `apply_updates` to `true`. To preview it now, change the schedule to `now` in a staging copy and then place it in `modules/`. Actual upgrades require appropriate privileges and a maintenance window. The example does not elevate privileges, handle reboots, or replace distro-specific update tooling.

## Checks

To enable manual builds on GitHub, set the repository variable `NATIVE_BUILD_OWNERS` under **Settings → Secrets and variables → Actions → Variables**. Enter the GitHub usernames of the code owners who should be allowed to build, separated by commas with no spaces or `@` prefixes (for example, `alice,bob`). If the variable is unset, the build job is skipped.

Listed owners with repository write access can click **Actions → Native builds → Run workflow**. GitHub shows the button to other users with write access too, but the build job checks both who started the run and who requested a rerun. This check applies to the workflow as written; it doesn't prevent users who can edit the workflow from removing it.

Run the checks locally at any time:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```
