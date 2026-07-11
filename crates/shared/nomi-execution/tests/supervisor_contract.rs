use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use nomi_execution::SupervisorConfig;
use tempfile::tempdir;

fn helper_binary() -> &'static str {
    env!("CARGO_BIN_EXE_execution_test_helper")
}

#[test]
fn helper_exit_returns_the_requested_code() {
    let status = Command::new(helper_binary())
        .args(["exit", "17"])
        .status()
        .expect("execution_test_helper should start");

    assert_eq!(status.code(), Some(17));
}

#[test]
fn supervisor_defaults_are_stable() {
    let config = SupervisorConfig::default();

    assert_eq!(config.max_sessions, 64);
    assert_eq!(config.reaper_interval, Duration::from_secs(30));
}

#[test]
fn helper_sleep_stays_alive_for_the_requested_interval() {
    let started = Instant::now();
    let status = Command::new(helper_binary())
        .args(["sleep", "40"])
        .status()
        .expect("execution_test_helper should start");

    assert!(status.success());
    assert!(started.elapsed() >= Duration::from_millis(35));
}

#[test]
fn helper_echo_stdin_copies_bytes_verbatim() {
    let mut child = Command::new(helper_binary())
        .arg("echo-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("execution_test_helper should start");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(b"hello\0world\n")
        .expect("stdin write should succeed");

    let output = child.wait_with_output().expect("helper should finish");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"hello\0world\n");
}

#[test]
fn helper_emit_interleaved_writes_flushed_records_to_both_streams() {
    let output = Command::new(helper_binary())
        .arg("emit-interleaved")
        .output()
        .expect("execution_test_helper should start");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"stdout-1\nstdout-2\n");
    assert_eq!(output.stderr, b"stderr-1\nstderr-2\n");
}

#[test]
fn helper_emit_split_utf8_preserves_the_exact_bytes() {
    let output = Command::new(helper_binary())
        .arg("emit-split-utf8")
        .output()
        .expect("execution_test_helper should start");

    assert!(output.status.success());
    assert_eq!(output.stdout, "中文🙂".as_bytes());
}

#[test]
fn helper_emit_delayed_writes_each_flushed_record() {
    let output = Command::new(helper_binary())
        .args(["emit-delayed", "3", "5"])
        .output()
        .expect("execution_test_helper should start");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"tick-0\ntick-1\ntick-2\n");
}

#[test]
fn helper_flood_writes_the_requested_number_of_bytes() {
    let output = Command::new(helper_binary())
        .args(["flood", "16385"])
        .output()
        .expect("execution_test_helper should start");

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 16_385);
    assert!(output.stdout.iter().all(|byte| *byte == b'x'));
}

#[test]
fn helper_write_pid_publishes_its_pid_atomically() {
    let directory = tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("pid.marker");
    let mut child = Command::new(helper_binary())
        .arg("write-pid")
        .arg(&marker)
        .spawn()
        .expect("execution_test_helper should start");
    let expected_pid = child.id();

    let status = child.wait().expect("helper should finish");

    assert!(status.success());
    assert_eq!(read_pid(&marker), expected_pid);
}

#[test]
#[cfg(unix)]
fn helper_spawn_grandchild_publishes_the_child_pid() {
    let directory = tempdir().expect("temporary directory should be created");
    let marker = directory.path().join("grandchild.marker");
    let child = Command::new(helper_binary())
        .arg("spawn-grandchild")
        .arg(&marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("execution_test_helper should start");
    let parent_pid = child.id();
    let mut tree = HelperTree {
        parent: child,
        #[cfg(unix)]
        grandchild_pid: None,
    };

    let grandchild_pid = wait_for_pid(&marker, Duration::from_secs(5));
    tree.grandchild_pid = Some(grandchild_pid);

    assert_ne!(grandchild_pid, parent_pid);
    assert!(tree
        .parent
        .try_wait()
        .expect("helper status should be readable")
        .is_none());
}

#[test]
fn helper_ignore_interrupt_remains_alive() {
    let mut command = Command::new(helper_binary());
    command
        .arg("ignore-interrupt")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_interrupt_process(&mut command);
    let child = command
        .spawn()
        .expect("execution_test_helper should start");
    let mut tree = HelperTree {
        parent: child,
        #[cfg(unix)]
        grandchild_pid: None,
    };
    let stdout = tree
        .parent
        .stdout
        .take()
        .expect("helper readiness should be piped");
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut ready = String::new();
        let result = BufReader::new(stdout).read_line(&mut ready).map(|_| ready);
        let _ = ready_tx.send(result);
    });
    let ready = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("helper should publish interrupt readiness")
        .expect("helper readiness should be readable");
    assert_eq!(ready, "ready\n");

    send_interrupt(tree.parent.id()).expect("interrupt should be delivered");
    thread::sleep(Duration::from_millis(50));

    assert!(tree
        .parent
        .try_wait()
        .expect("helper status should be readable")
        .is_none());
}

#[cfg(unix)]
fn wait_for_pid(path: &std::path::Path, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    loop {
        if path.is_file() {
            return read_pid(path);
        }
        assert!(Instant::now() < deadline, "PID marker was not written");
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_pid(path: &std::path::Path) -> u32 {
    fs::read_to_string(path)
        .expect("PID marker should be readable")
        .trim()
        .parse()
        .expect("PID marker should contain a decimal PID")
}

struct HelperTree {
    parent: Child,
    #[cfg(unix)]
    grandchild_pid: Option<u32>,
}

impl Drop for HelperTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        kill_process_tree(self.parent.id());
        let _ = self.parent.kill();
        let _ = self.parent.wait();
        #[cfg(unix)]
        if let Some(pid) = self.grandchild_pid {
            kill_process(pid);
        }
    }
}

#[cfg(windows)]
fn configure_interrupt_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn configure_interrupt_process(_command: &mut Command) {}

#[cfg(windows)]
fn send_interrupt(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

    // SAFETY: `pid` is the process-group id created for this helper child.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn send_interrupt(pid: u32) -> std::io::Result<()> {
    // SAFETY: `pid` came from the live child and SIGINT has no memory-safety contract.
    if unsafe { libc::kill(pid as i32, libc::SIGINT) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    kill_process(pid);
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    // SAFETY: `pid` came from the helper marker and SIGKILL has no memory-safety contract.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}
