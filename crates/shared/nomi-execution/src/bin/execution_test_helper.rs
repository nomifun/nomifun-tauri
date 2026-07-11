use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{self, Command},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const LONG_SLEEP: Duration = Duration::from_secs(60);
const UTF8_SAMPLE: &str = "中文🙂";

fn main() {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().and_then(|arg| arg.to_str()) else {
        fail("missing helper subcommand");
    };

    match command {
        "exit" => {
            require_len(&args, 2);
            process::exit(parse_i32(&args[1], "exit code"));
        }
        "sleep" => {
            require_len(&args, 2);
            thread::sleep(Duration::from_millis(parse_u64(&args[1], "sleep duration")));
        }
        "echo-stdin" => {
            require_len(&args, 1);
            copy_stdin().unwrap_or_else(|error| fail_io("echo stdin", error));
        }
        "emit-interleaved" => {
            require_len(&args, 1);
            emit_interleaved().unwrap_or_else(|error| fail_io("emit interleaved", error));
        }
        "emit-split-utf8" => {
            require_len(&args, 1);
            emit_split_utf8().unwrap_or_else(|error| fail_io("emit split UTF-8", error));
        }
        "emit-delayed" => {
            require_len(&args, 3);
            emit_delayed(
                parse_u64(&args[1], "delayed output count"),
                Duration::from_millis(parse_u64(&args[2], "delayed output interval")),
            )
            .unwrap_or_else(|error| fail_io("emit delayed output", error));
        }
        "flood" => {
            require_len(&args, 2);
            flood(parse_u64(&args[1], "flood byte count"))
                .unwrap_or_else(|error| fail_io("flood stdout", error));
        }
        "spawn-grandchild" => {
            require_len(&args, 2);
            spawn_grandchild(Path::new(&args[1]))
                .unwrap_or_else(|error| fail_io("spawn grandchild", error));
        }
        "spawn-ignore-group" => {
            require_len(&args, 2);
            spawn_ignore_group(Path::new(&args[1]))
                .unwrap_or_else(|error| fail_io("spawn interrupt-ignoring group", error));
        }
        "ignore-interrupt-pid" => {
            require_len(&args, 2);
            ignore_interrupt().unwrap_or_else(|error| fail_io("ignore interrupt", error));
            write_pid_atomically(Path::new(&args[1]), process::id())
                .unwrap_or_else(|error| fail_io("write ready PID marker", error));
            thread::sleep(LONG_SLEEP);
        }
        "leader-first" => {
            require_len(&args, 2);
            spawn_leader_first(Path::new(&args[1]))
                .unwrap_or_else(|error| fail_io("spawn leader-first descendant", error));
        }
        #[cfg(windows)]
        "leader-first-gated" => {
            require_len(&args, 3);
            spawn_leader_first_gated(Path::new(&args[1]), Path::new(&args[2]))
                .unwrap_or_else(|error| fail_io("spawn gated leader-first descendant", error));
        }
        #[cfg(unix)]
        "setsid-escape" => {
            require_len(&args, 2);
            spawn_surviving_descendant(Path::new(&args[1]), true)
                .unwrap_or_else(|error| fail_io("spawn setsid descendant", error));
        }
        "ignore-interrupt" => {
            require_len(&args, 1);
            ignore_interrupt().unwrap_or_else(|error| fail_io("ignore interrupt", error));
            emit_ready().unwrap_or_else(|error| fail_io("emit interrupt readiness", error));
            thread::sleep(LONG_SLEEP);
        }
        "write-pid" => {
            require_len(&args, 2);
            write_pid_atomically(Path::new(&args[1]), process::id())
                .unwrap_or_else(|error| fail_io("write PID marker", error));
        }
        "write-file" => {
            require_len(&args, 2);
            fs::write(Path::new(&args[1]), b"written by execution_test_helper\n")
                .unwrap_or_else(|error| fail_io("write file", error));
        }
        "print-args-env-cwd" => {
            require_len(&args, 5);
            print_args_env_cwd(&args[1], &args[2], &args[3], &args[4])
                .unwrap_or_else(|error| fail_io("print argv/env/cwd", error));
        }
        _ => fail("unknown helper subcommand"),
    }
}

fn copy_stdin() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    io::copy(&mut input, &mut output)?;
    output.flush()
}

fn emit_interleaved() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    stdout.write_all(b"stdout-1\n")?;
    stdout.flush()?;
    stderr.write_all(b"stderr-1\n")?;
    stderr.flush()?;
    stdout.write_all(b"stdout-2\n")?;
    stdout.flush()?;
    stderr.write_all(b"stderr-2\n")?;
    stderr.flush()
}

fn emit_split_utf8() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    for byte in UTF8_SAMPLE.as_bytes() {
        stdout.write_all(&[*byte])?;
        stdout.flush()?;
    }
    Ok(())
}

fn emit_delayed(count: u64, interval: Duration) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    for index in 0..count {
        writeln!(stdout, "tick-{index}")?;
        stdout.flush()?;
        thread::sleep(interval);
    }
    Ok(())
}

fn emit_ready() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"ready\n")?;
    stdout.flush()
}

fn print_args_env_cwd(
    first: &OsStr,
    second: &OsStr,
    env_key: &OsStr,
    expected_cwd: &OsStr,
) -> io::Result<()> {
    let cwd = env::current_dir()?;
    if cwd.as_os_str() != expected_cwd {
        return Err(io::Error::other(format!(
            "cwd mismatch: actual={cwd:?} expected={expected_cwd:?}"
        )));
    }
    let value = env::var_os(env_key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("environment key was not inherited: {env_key:?}"),
        )
    })?;
    let mut output = io::stdout().lock();
    for field in [first, second, value.as_os_str(), cwd.as_os_str()] {
        let encoded = field.to_string_lossy();
        writeln!(output, "{}:{encoded}", encoded.len())?;
    }
    output.flush()
}

fn flood(mut remaining: u64) -> io::Result<()> {
    const BLOCK: [u8; 8 * 1024] = [b'x'; 8 * 1024];
    let mut stdout = io::stdout().lock();
    while remaining > 0 {
        let count = remaining.min(BLOCK.len() as u64) as usize;
        stdout.write_all(&BLOCK[..count])?;
        remaining -= count as u64;
    }
    stdout.flush()
}

fn spawn_grandchild(marker: &Path) -> io::Result<()> {
    let executable = env::current_exe()?;
    let mut grandchild = Command::new(executable).args(["sleep", "60000"]).spawn()?;
    if let Err(error) = write_pid_atomically(marker, grandchild.id()) {
        let _ = grandchild.kill();
        let _ = grandchild.wait();
        return Err(error);
    }
    thread::sleep(LONG_SLEEP);
    let _ = grandchild.wait();
    Ok(())
}

fn spawn_ignore_group(marker: &Path) -> io::Result<()> {
    ignore_interrupt()?;
    let executable = env::current_exe()?;
    let mut grandchild = Command::new(executable)
        .arg("ignore-interrupt-pid")
        .arg(marker)
        .spawn()?;
    if let Err(error) = wait_for_marker(marker, Duration::from_secs(5)) {
        let _ = grandchild.kill();
        let _ = grandchild.wait();
        return Err(error);
    }
    emit_ready()?;
    thread::sleep(LONG_SLEEP);
    let _ = grandchild.wait();
    Ok(())
}

#[cfg(unix)]
fn spawn_surviving_descendant(marker: &Path, escape_session: bool) -> io::Result<()> {
    let executable = env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(["sleep", "60000"]);
    if escape_session {
        // SAFETY: setsid is async-signal-safe and this closure performs no allocation or I/O.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut descendant = command.spawn()?;
    if let Err(error) = write_pid_atomically(marker, descendant.id()) {
        let _ = descendant.kill();
        let _ = descendant.wait();
        return Err(error);
    }
    drop(descendant);
    Ok(())
}

fn spawn_leader_first(marker: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        spawn_surviving_descendant(marker, false)
    }

    #[cfg(windows)]
    {
        let executable = env::current_exe()?;
        let mut descendant = Command::new(executable)
            .args(["sleep", "60000"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        if let Err(error) = write_pid_atomically(marker, descendant.id()) {
            let _ = descendant.kill();
            let _ = descendant.wait();
            return Err(error);
        }
        drop(descendant);
        Ok(())
    }
}

#[cfg(windows)]
fn spawn_leader_first_gated(marker: &Path, exit_gate: &Path) -> io::Result<()> {
    let executable = env::current_exe()?;
    let mut descendant = Command::new(executable)
        .args(["sleep", "60000"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Err(error) = write_pid_atomically(marker, descendant.id())
        .and_then(|_| wait_for_marker(exit_gate, Duration::from_secs(5)))
    {
        let _ = descendant.kill();
        let _ = descendant.wait();
        return Err(error);
    }
    drop(descendant);
    Ok(())
}

fn wait_for_marker(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PID marker was not published before the deadline",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn write_pid_atomically(path: &Path, pid: u32) -> io::Result<()> {
    let (temporary_path, mut temporary) = create_temporary_marker(path, pid)?;
    let result = (|| {
        writeln!(temporary, "{pid}")?;
        temporary.flush()?;
        temporary.sync_all()?;
        drop(temporary);
        fs::rename(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_temporary_marker(path: &Path, pid: u32) -> io::Result<(PathBuf, fs::File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "marker has no file name"))?;
    for attempt in 0..100_u32 {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".{pid}.{attempt}.tmp"));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary PID marker",
    ))
}

#[cfg(windows)]
fn ignore_interrupt() -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn ignore_control_event(_control_type: u32) -> i32 {
        1
    }

    // SAFETY: the handler has the required system ABI, remains valid for the
    // process lifetime, and reports both CTRL+C and CTRL+BREAK as handled.
    if unsafe { SetConsoleCtrlHandler(Some(ignore_control_event), 1) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn ignore_interrupt() -> io::Result<()> {
    // SAFETY: installing SIG_IGN for SIGINT changes only this helper's signal disposition.
    if unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) } == libc::SIG_ERR {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn require_len(args: &[OsString], expected: usize) {
    if args.len() != expected {
        fail("invalid helper arguments");
    }
}

fn parse_i32(value: &OsStr, label: &str) -> i32 {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| fail(&format!("invalid {label}")))
}

fn parse_u64(value: &OsStr, label: &str) -> u64 {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| fail(&format!("invalid {label}")))
}

fn fail_io(action: &str, error: io::Error) -> ! {
    fail(&format!("{action}: {error}"))
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    process::exit(2);
}
