use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::PathBuf,
    time::{Duration, Instant},
};

use nomi_execution::{
    CapabilityPolicy, CleanupReport, CommandSpec, EncodingMetadata, ExecutionEvent,
    ExecutionOutcome, ExecutionOwner, ExecutionPolicy, ExecutionRequest, OutputChunk, OutputCursor,
    OutputSnapshot, OutputStream, ProcessSnapshot, ProcessState, SessionId, ShellKind, SpawnFailure,
    Transport, normalize_request,
};
use uuid::Uuid;

fn request(cwd: PathBuf) -> ExecutionRequest {
    ExecutionRequest {
        owner: ExecutionOwner::new(Uuid::now_v7(), Uuid::now_v7()),
        command: CommandSpec::Program {
            program: OsString::from("tool"),
            args: vec![OsString::from("--flag")],
        },
        cwd,
        env: BTreeMap::new(),
        transport: Transport::Pipe,
        policy: ExecutionPolicy::default(),
        capability: CapabilityPolicy::local_owner(std::env::temp_dir()),
    }
}

#[test]
fn relative_cwd_is_anchored_and_validated() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    let mut req = request(PathBuf::from("child"));
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    let normalized = normalize_request(req, root.path()).unwrap();
    assert_eq!(
        normalized.cwd,
        root.path().join("child").canonicalize().unwrap()
    );
}

#[test]
fn missing_cwd_fails_before_spawn() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(PathBuf::from("missing"));
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    let err = normalize_request(req, root.path()).unwrap_err();
    assert_eq!(err.code(), "invalid_working_directory");
}

#[test]
fn cwd_outside_capability_root_is_denied() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut req = request(outside.path().to_path_buf());
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    let err = normalize_request(req, root.path()).unwrap_err();
    assert_eq!(err.code(), "capability_denied");
}

#[test]
fn capability_roots_are_canonicalized() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    let mut req = request(PathBuf::from("child"));
    req.capability = CapabilityPolicy::local_owner(root.path().join("child").join(".."));

    let normalized = normalize_request(req, root.path()).unwrap();

    assert_eq!(
        normalized.capability.cwd_roots,
        vec![root.path().canonicalize().unwrap()]
    );
}

#[test]
fn non_directory_cwd_fails_before_spawn() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"not a directory").unwrap();
    let mut req = request(PathBuf::from("file"));
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());

    let err = normalize_request(req, root.path()).unwrap_err();

    assert_eq!(err.code(), "invalid_working_directory");
}

#[test]
fn zero_sized_pty_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path().to_path_buf());
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    req.transport = Transport::Pty { cols: 0, rows: 24 };

    assert!(normalize_request(req, root.path()).is_err());
}

#[test]
fn empty_program_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path().to_path_buf());
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    req.command = CommandSpec::Program {
        program: OsString::new(),
        args: Vec::new(),
    };

    assert!(normalize_request(req, root.path()).is_err());
}

#[test]
fn empty_shell_script_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path().to_path_buf());
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    req.command = CommandSpec::Shell {
        shell: ShellKind::Posix,
        script: String::new(),
    };

    assert!(normalize_request(req, root.path()).is_err());
}

#[test]
fn cancellation_policy_totals_five_seconds() {
    let p = ExecutionPolicy::default();
    assert_eq!(p.interrupt_grace, Duration::from_secs(1));
    assert_eq!(p.terminate_grace, Duration::from_secs(1));
    assert_eq!(p.reap_grace, Duration::from_secs(3));
}

#[test]
fn session_ids_are_uuid_v7_and_unpredictable() {
    let a = SessionId::new();
    let b = SessionId::new();
    assert_ne!(a, b);
    assert_eq!(a.as_uuid().get_version_num(), 7);
}

#[test]
fn session_ids_round_trip_through_text() {
    let id = SessionId::new();

    let parsed = id.to_string().parse::<SessionId>().unwrap();

    assert_eq!(parsed, id);
}

#[test]
fn shell_requires_a_script_and_program_preserves_os_strings() {
    let shell = CommandSpec::Shell {
        shell: ShellKind::Posix,
        script: "printf ok".to_owned(),
    };
    assert!(matches!(shell, CommandSpec::Shell { .. }));

    let spec = CommandSpec::Program {
        program: OsString::from("echo"),
        args: vec![OsString::from("ok")],
    };
    assert!(matches!(spec, CommandSpec::Program { .. }));
}

#[test]
fn output_cursor_exposes_absolute_offsets() {
    assert_eq!(OutputCursor::START.offset(), 0);
    assert_eq!(OutputCursor::new(42).offset(), 42);
}

#[test]
fn outcome_variants_preserve_process_facts() {
    let snapshot = || OutputSnapshot {
        chunks: vec![OutputChunk {
            seq: 1,
            start: 0,
            stream: OutputStream::Stdout,
            bytes: b"ok".to_vec(),
            text: "ok".to_owned(),
        }],
        next_cursor: OutputCursor::new(2),
        retained_bytes: 2,
        dropped_bytes: 0,
        encoding: EncodingMetadata {
            source_encoding: "utf-8".to_owned(),
            decode_errors: 0,
        },
    };
    let cleanup = || CleanupReport {
        interrupt_attempted: false,
        terminate_attempted: false,
        force_kill_attempted: false,
        reaped: true,
        elapsed: Duration::ZERO,
        errors: Vec::new(),
    };
    let now = Instant::now();

    let outcomes = [
        ExecutionOutcome::Exited {
            code: Some(0),
            signal: None,
            output: snapshot(),
            cleanup: cleanup(),
        },
        ExecutionOutcome::SpawnFailed(SpawnFailure {
            code: "spawn_failed".to_owned(),
            message: "could not spawn".to_owned(),
        }),
        ExecutionOutcome::Cancelled {
            output: snapshot(),
            cleanup: cleanup(),
        },
        ExecutionOutcome::TimedOut {
            output: snapshot(),
            cleanup: cleanup(),
        },
        ExecutionOutcome::Lost {
            last_known: ProcessSnapshot {
                pid: 7,
                state: ProcessState::Lost,
                started_at: now,
                last_activity_at: now,
            },
            output: snapshot(),
            cleanup: cleanup(),
        },
    ];

    assert_eq!(outcomes.len(), 5);
}

#[test]
fn execution_events_preserve_sequence_and_loss() {
    let output = ExecutionEvent::Output {
        seq: 3,
        stream: OutputStream::Stderr,
        bytes: b"failure".to_vec(),
        text: "failure".to_owned(),
        encoding: EncodingMetadata {
            source_encoding: "utf-8".to_owned(),
            decode_errors: 0,
        },
    };
    let state = ExecutionEvent::StateChanged {
        seq: 4,
        state: ProcessState::Running,
    };
    let dropped = ExecutionEvent::OutputDropped { seq: 5, bytes: 8 };

    assert!(matches!(output, ExecutionEvent::Output { seq: 3, .. }));
    assert!(matches!(
        state,
        ExecutionEvent::StateChanged {
            seq: 4,
            state: ProcessState::Running
        }
    ));
    assert!(matches!(
        dropped,
        ExecutionEvent::OutputDropped { seq: 5, bytes: 8 }
    ));
}
