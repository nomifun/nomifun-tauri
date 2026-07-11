#[cfg(any(unix, windows))]
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::Duration,
};

#[cfg(any(unix, windows))]
use nomi_execution::{
    CapabilityPolicy, CommandSpec, ExecutionOwner, ExecutionPolicy, NormalizedExecutionRequest,
    ProcessSupervisor, SupervisorConfig, Transport,
};

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("parent-death harness failed: {error}");
        process::exit(2);
    }
    // SAFETY: the harness intentionally bypasses every Rust drop and runtime cleanup path.
    unsafe { libc::_exit(0) }
}

#[cfg(not(unix))]
#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run_windows().await {
        eprintln!("parent-death harness failed: {error}");
        process::exit(2);
    }
}

#[cfg(unix)]
async fn run() -> io::Result<()> {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if !(args.len() == 3 || args.len() == 4 || args.len() == 6 || args.len() == 7) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected helper, leader marker, grandchild marker, optional 'pty', and optional start/ready/exit gates",
        ));
    }
    let helper = args[0].clone();
    let leader_marker = PathBuf::from(&args[1]);
    let grandchild_marker = PathBuf::from(&args[2]);
    let transport_arg = match args.len() {
        4 | 7 => args.get(3).and_then(|value| value.to_str()),
        _ => None,
    };
    let transport = match transport_arg {
        None => Transport::Pipe,
        Some("pty") => Transport::Pty { cols: 80, rows: 24 },
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "optional Unix transport must be 'pty'",
            ));
        }
    };
    let gate_offset = usize::from(matches!(args.len(), 4 | 7));
    let gates = (args.len() >= 6).then(|| {
        (
            PathBuf::from(&args[3 + gate_offset]),
            PathBuf::from(&args[4 + gate_offset]),
            PathBuf::from(&args[5 + gate_offset]),
        )
    });
    if let Some((start_gate, _, _)) = gates.as_ref() {
        wait_for_gate(start_gate, "start").await?;
    }
    let cwd = env::current_dir()?;
    let request = NormalizedExecutionRequest {
        owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
        command: CommandSpec::Program {
            program: helper,
            args: vec![
                OsString::from("spawn-grandchild"),
                grandchild_marker.as_os_str().to_owned(),
            ],
        },
        cwd: cwd.clone(),
        env: BTreeMap::new(),
        transport,
        policy: ExecutionPolicy::default(),
        capability: CapabilityPolicy::local_owner(cwd),
    };
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(request)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    let leader = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
        .pid;
    wait_for_pid_marker(&grandchild_marker).await?;
    write_pid_atomically(&leader_marker, leader)?;
    if let Some((_, ready_gate, exit_gate)) = gates.as_ref() {
        write_gate_atomically(ready_gate)?;
        wait_for_gate(exit_gate, "exit").await?;
    }
    Ok(())
}

#[cfg(any(unix, windows))]
async fn wait_for_pid_marker(path: &Path) -> io::Result<u32> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                return Ok(pid);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "grandchild PID marker timed out"))?
}

#[cfg(any(unix, windows))]
fn write_pid_atomically(path: &Path, pid: u32) -> io::Result<()> {
    write_atomically(path, pid.to_string().as_bytes())
}

#[cfg(unix)]
fn write_gate_atomically(path: &Path) -> io::Result<()> {
    write_atomically(path, b"ready")
}

#[cfg(any(unix, windows))]
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "marker has no file name"))?;
    let pid = process::id();
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
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(contents)?;
                    file.write_all(b"\n")?;
                    file.flush()?;
                    file.sync_all()?;
                    drop(file);
                    fs::rename(&temporary_path, path)
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary_path);
                }
                return result;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary marker",
    ))
}

#[cfg(unix)]
async fn wait_for_gate(path: &Path, label: &str) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if path.is_file() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{label} gate timed out: {}", path.display()),
        )
    })
}

#[cfg(windows)]
async fn run_windows() -> io::Result<()> {
    use windows_sys::Win32::System::Threading::ExitProcess;

    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected helper, leader marker, grandchild marker, start gate, and exit gate paths",
        ));
    }
    let helper = args[0].clone();
    let leader_marker = PathBuf::from(&args[1]);
    let grandchild_marker = PathBuf::from(&args[2]);
    let start_gate = PathBuf::from(&args[3]);
    let exit_gate = PathBuf::from(&args[4]);

    wait_for_gate(&start_gate, "start").await?;

    let cwd = env::current_dir()?;
    let request = NormalizedExecutionRequest {
        owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
        command: CommandSpec::Program {
            program: helper,
            args: vec![
                OsString::from("spawn-grandchild"),
                grandchild_marker.as_os_str().to_owned(),
            ],
        },
        cwd: cwd.clone(),
        env: BTreeMap::new(),
        transport: Transport::Pipe,
        policy: ExecutionPolicy::default(),
        capability: CapabilityPolicy::local_owner(cwd),
    };
    let supervisor = ProcessSupervisor::new(SupervisorConfig::default());
    let handle = supervisor
        .start(request)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    let leader = supervisor
        .status(&handle.owner, &handle.session_id)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
        .pid;
    wait_for_pid_marker(&grandchild_marker).await?;
    write_pid_atomically(&leader_marker, leader)?;
    wait_for_gate(&exit_gate, "exit").await?;

    // SAFETY: the harness intentionally exits while `supervisor` and `handle` still own the
    // execution Job. ExitProcess bypasses Rust drops, so the kernel closing the last Job handle
    // is the behavior under test.
    unsafe { ExitProcess(0) }
}

#[cfg(windows)]
async fn wait_for_gate(path: &Path, label: &str) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if path.is_file() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{label} gate timed out: {}", path.display()),
        )
    })
}
