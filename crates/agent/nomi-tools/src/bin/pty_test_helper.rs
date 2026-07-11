//! Cross-platform test fixture for the `nomi-tools` PTY/process unit tests.
//!
//! The PTY tests need a handful of deterministic child behaviours (echo stdin,
//! stay alive for N ms, exit with a code, emit output after a delay). The unix
//! programs they originally used (`cat`, `sleep`, `sh -c 'exit N'`) do not exist
//! under Windows `cmd`, so the tests spawn THIS binary instead — identical
//! behaviour on Windows and unix, no external dependencies, pure `std`.
//!
//! Subcommands (`pty_test_helper <subcommand> [args...]`):
//! - `echo-stdin`: read stdin line-by-line, echo each line to stdout (flushed),
//!   and exit on EOF. Replaces `cat`.
//! - `sleep <ms>`: sleep `ms` milliseconds, then exit 0. Replaces `sleep N`.
//! - `exit <code>`: exit immediately with `code`. Replaces `sh -c 'exit N'`.
//! - `emit-after <ms> <text> <keepalive_ms>`: sleep `ms`, print `text` plus a
//!   newline (flushed), then sleep `keepalive_ms` and exit. Models a process
//!   that emits delayed output and then lingers.
//! - `emit-twice <first_ms> <first> <second_ms> <second> <keepalive_ms>`: emit
//!   two flushed lines at independent delays, then remain alive. Models output
//!   arriving around an adapter settle boundary.
//! - `ignore-interrupt`: install an interrupt handler that ignores Ctrl-C,
//!   print `ready`, then remain alive.
//! - `write-marker-after <ms> <path>`: sleep `ms`, then atomically publish a
//!   marker.
//! - `spawn-marker-child <ms> <path> <ready_path> <keepalive_ms>`: spawn
//!   `write-marker-after`, print the child's PID, atomically publish both PIDs,
//!   then remain alive for `keepalive_ms`.
//! - `print-unicode`: print the Task 9 encoding sample.
//!
//! Kept dependency-free on purpose: it is compiled as part of the crate's normal
//! build (a `[[bin]]`) so the unit tests can locate it next to the test runner.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// This fixture deliberately leaves the spawned grandchild running so the
// supervisor tests can prove process-tree cleanup. Waiting here would erase
// the lifecycle being tested.
#[allow(clippy::zombie_processes)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(String::as_str).unwrap_or("");

    match sub {
        "echo-stdin" => echo_stdin(),
        "sleep" => {
            let ms = parse_u64(args.get(1), "sleep <ms>");
            std::thread::sleep(Duration::from_millis(ms));
        }
        "exit" => {
            let code = parse_i32(args.get(1), "exit <code>");
            std::process::exit(code);
        }
        "emit-after" => {
            let delay_ms = parse_u64(args.get(1), "emit-after <ms> <text> <keepalive_ms>");
            let text = args.get(2).cloned().unwrap_or_default();
            let keepalive_ms = parse_u64(args.get(3), "emit-after <ms> <text> <keepalive_ms>");
            std::thread::sleep(Duration::from_millis(delay_ms));
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            let _ = writeln!(w, "{text}");
            let _ = w.flush();
            drop(w);
            std::thread::sleep(Duration::from_millis(keepalive_ms));
        }
        "emit-twice" => {
            let usage = "emit-twice <first_ms> <first> <second_ms> <second> <keepalive_ms>";
            let first_ms = parse_u64(args.get(1), usage);
            let first = required_arg(args.get(2), usage);
            let second_ms = parse_u64(args.get(3), usage);
            let second = required_arg(args.get(4), usage);
            let keepalive_ms = parse_u64(args.get(5), usage);
            std::thread::sleep(Duration::from_millis(first_ms));
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            let _ = writeln!(w, "{first}");
            let _ = w.flush();
            std::thread::sleep(Duration::from_millis(second_ms));
            let _ = writeln!(w, "{second}");
            let _ = w.flush();
            drop(w);
            std::thread::sleep(Duration::from_millis(keepalive_ms));
        }
        "ignore-interrupt" => {
            ignore_interrupt().unwrap_or_else(|error| fail_io("ignore interrupt", error));
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            let _ = writeln!(w, "ready");
            let _ = w.flush();
            drop(w);
            std::thread::sleep(Duration::from_secs(60));
        }
        "write-marker-after" => {
            let delay_ms = parse_u64(args.get(1), "write-marker-after <ms> <path>");
            let marker = PathBuf::from(required_arg(args.get(2), "write-marker-after <ms> <path>"));
            std::thread::sleep(Duration::from_millis(delay_ms));
            write_marker_atomically(&marker)
                .unwrap_or_else(|error| fail_io("publish delayed marker", error));
        }
        "spawn-marker-child" => {
            let usage = "spawn-marker-child <ms> <path> <ready_path> <keepalive_ms>";
            let delay_ms = parse_u64(args.get(1), usage);
            let marker = PathBuf::from(required_arg(args.get(2), usage));
            let ready = PathBuf::from(required_arg(args.get(3), usage));
            let keepalive_ms = parse_u64(args.get(4), usage);
            let child = Command::new(std::env::current_exe().unwrap_or_else(|error| {
                fail_io("resolve helper executable", error)
            }))
            .arg("write-marker-after")
            .arg(delay_ms.to_string())
            .arg(&marker)
            .spawn()
            .unwrap_or_else(|error| fail_io("spawn delayed marker child", error));
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            let _ = writeln!(w, "grandchild_pid={}", child.id());
            let _ = w.flush();
            drop(w);
            write_text_atomically(
                &ready,
                &format!(
                    "helper_pid={}\ngrandchild_pid={}\n",
                    std::process::id(),
                    child.id()
                ),
            )
            .unwrap_or_else(|error| fail_io("publish helper PID marker", error));
            std::thread::sleep(Duration::from_millis(keepalive_ms));
        }
        "print-unicode" => {
            println!("中文🙂");
        }
        other => {
            eprintln!("pty_test_helper: unknown subcommand {other:?}");
            std::process::exit(2);
        }
    }
}

/// Read stdin line-by-line and echo each line back to stdout, flushing after
/// every line so a PTY consumer sees the echo promptly. Exits on EOF. This is
/// the cross-platform stand-in for `cat`.
fn echo_stdin() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                // `read_line` keeps the trailing newline; write it back verbatim.
                if out.write_all(line.as_bytes()).is_err() {
                    break;
                }
                let _ = out.flush();
            }
            Err(_) => break,
        }
    }
}

fn parse_u64(arg: Option<&String>, usage: &str) -> u64 {
    match arg.and_then(|s| s.parse::<u64>().ok()) {
        Some(v) => v,
        None => {
            eprintln!("pty_test_helper: expected {usage}");
            std::process::exit(2);
        }
    }
}

fn parse_i32(arg: Option<&String>, usage: &str) -> i32 {
    match arg.and_then(|s| s.parse::<i32>().ok()) {
        Some(v) => v,
        None => {
            eprintln!("pty_test_helper: expected {usage}");
            std::process::exit(2);
        }
    }
}

fn required_arg<'a>(arg: Option<&'a String>, usage: &str) -> &'a str {
    match arg {
        Some(value) => value,
        None => {
            eprintln!("pty_test_helper: expected {usage}");
            std::process::exit(2);
        }
    }
}

fn write_marker_atomically(marker: &Path) -> std::io::Result<()> {
    write_text_atomically(marker, &std::process::id().to_string())
}

fn write_text_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("marker path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".pty-test-helper-{}.tmp", std::process::id()));
    std::fs::write(&temporary, content)?;
    std::fs::rename(temporary, path)
}

fn fail_io(action: &str, error: std::io::Error) -> ! {
    eprintln!("pty_test_helper: {action}: {error}");
    std::process::exit(2);
}

#[cfg(windows)]
fn ignore_interrupt() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn ignore_control_event(_control_type: u32) -> i32 {
        1
    }

    // SAFETY: the handler has the required ABI and remains valid for the
    // lifetime of this helper process.
    if unsafe { SetConsoleCtrlHandler(Some(ignore_control_event), 1) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn ignore_interrupt() -> std::io::Result<()> {
    // SAFETY: installing SIG_IGN changes only this helper's SIGINT disposition.
    if unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) } == libc::SIG_ERR {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
