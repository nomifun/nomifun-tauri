mod conpty;
mod handles;

use std::{
    cmp::Ordering,
    collections::HashMap,
    ffi::{OsStr, OsString, c_void},
    io,
    mem,
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::sync::watch;
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, HANDLE,
        HANDLE_FLAG_INHERIT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, SetHandleInformation,
    },
    Globalization::{CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
            Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        SystemInformation::GetWindowsDirectoryW,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
            CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
            OpenThread, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, THREAD_SUSPEND_RESUME, TerminateProcess, WaitForSingleObject,
        },
    },
};

use self::handles::{OwnedHandle, ProcThreadAttributeList};
use self::conpty::{PreparedConPty, PseudoConsoleControl};
use super::{ExitFact, ProcessOwner, SpawnedPlatformProcess};
use crate::{
    CleanupReport, CommandSpec, ExecutionError, NormalizedExecutionRequest, OutputBuffer,
    OutputStream, ProcessSnapshot, ProcessState, SandboxPolicy, ShellKind, SpawnFailure,
};

const READ_BUFFER_BYTES: usize = 8 * 1024;
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const POST_EXIT_READER_DRAIN: Duration = Duration::from_millis(120);
const CONPTY_NATURAL_CLOSE_WAIT: Duration = Duration::from_millis(250);
const CONPTY_INPUT_CLOSE_GRACE: Duration = Duration::from_millis(250);
const JOB_EMPTY_POLL: Duration = Duration::from_millis(2);
const WRITE_CHUNK_BYTES: usize = 64 * 1024;
const LIFECYCLE_WAIT_HORIZON: Duration = Duration::from_secs(60 * 60 * 24 * 365);
const MAX_COMMAND_LINE_UNITS: usize = 32_767;
const TERMINATED_BY_HOST_EXIT_CODE: u32 = 0xC000_013A;

pub(super) async fn spawn_pipe(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_pipe_inner(request, output, Arc::new(SystemWin32)).await
}

pub(crate) fn spawn_legacy(
    mut command: tokio::process::Command,
    hand_off: bool,
) -> io::Result<tokio::process::Child> {
    if hand_off {
        return command.spawn();
    }

    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let job = Arc::new(JobControl::new(create_execution_job()?));
    let child = command.spawn()?;
    let pid = child.id().ok_or_else(|| {
        io::Error::other("legacy child exited before its process Job could be registered")
    })?;
    let process = child.raw_handle().ok_or_else(|| {
        io::Error::other("legacy child handle disappeared before Job assignment")
    })?;
    let job_handle = job
        .raw_handle()
        .ok_or_else(|| io::Error::other("legacy execution Job closed before assignment"))?;
    // SAFETY: both handles are live for this call. The process handle remains
    // owned by tokio::process::Child and the Job is retained by `job`.
    if unsafe { AssignProcessToJobObject(job_handle, process.cast()) } == 0 {
        let assignment_error = io::Error::last_os_error();
        // Assignment failure must not return a live unowned child. The exact
        // process handle is still valid even though Tokio owns it.
        let _ = unsafe { TerminateProcess(process.cast(), TERMINATED_BY_HOST_EXIT_CODE) };
        let _ = unsafe { WaitForSingleObject(process.cast(), 5_000) };
        return Err(io::Error::new(
            assignment_error.kind(),
            format!("assign legacy child to execution Job: {assignment_error}"),
        ));
    }

    let process = match duplicate_non_inheritable(process.cast()) {
        Ok(process) => process,
        Err(error) => {
            let _ = job.close_for_kill();
            let _ = unsafe { WaitForSingleObject(process.cast(), 5_000) };
            return Err(io::Error::new(
                error.kind(),
                format!("duplicate legacy child process handle: {error}"),
            ));
        }
    };
    let execution = Arc::new(LegacyExecution { process, job });
    if let Err(error) = register_legacy_job(pid, Arc::clone(&execution)) {
        let _ = execution.job.close_for_kill();
        let _ = unsafe { WaitForSingleObject(execution.process.as_raw(), 5_000) };
        return Err(error);
    }
    if let Err(error) = resume_legacy_primary_thread(pid) {
        remove_legacy_job(pid, &execution);
        let _ = execution.job.close_for_kill();
        let _ = unsafe { WaitForSingleObject(execution.process.as_raw(), 5_000) };
        return Err(io::Error::new(
            error.kind(),
            format!("resume legacy child primary thread: {error}"),
        ));
    }
    Ok(child)
}

pub(crate) async fn kill_legacy_process_tree(
    child: &mut tokio::process::Child,
) -> io::Result<()> {
    let Some(pid) = child.id() else {
        return child.wait().await.map(|_| ());
    };
    let Some(execution) = legacy_job(pid)? else {
        child.kill().await?;
        return child.wait().await.map(|_| ());
    };

    execution.job.terminate()?;
    let cleanup = start_legacy_cleanup(pid, Arc::clone(&execution))?;
    let child_result = child.wait().await.map(|_| ());
    let cleanup_result = cleanup
        .await
        .map_err(|_| io::Error::other("legacy Job cleanup worker dropped without a result"))?;
    child_result?;
    cleanup_result
}

struct LegacyExecution {
    process: OwnedHandle,
    job: Arc<JobControl>,
}

fn legacy_jobs() -> &'static Mutex<HashMap<u32, Arc<LegacyExecution>>> {
    static JOBS: OnceLock<Mutex<HashMap<u32, Arc<LegacyExecution>>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_legacy_job(pid: u32, execution: Arc<LegacyExecution>) -> io::Result<()> {
    let start_gate = Arc::new(std::sync::Barrier::new(2));
    let worker_gate = Arc::clone(&start_gate);
    let worker_execution = Arc::clone(&execution);
    let worker = std::thread::Builder::new()
        .name(format!("nomi-legacy-job-{pid}"))
        .spawn(move || {
            worker_gate.wait();
            reap_legacy_job(pid, worker_execution);
        })?;

    let registered = {
        let mut jobs = match legacy_jobs().lock() {
            Ok(jobs) => jobs,
            Err(_) => {
                let _ = execution.job.close_for_kill();
                start_gate.wait();
                let _ = worker.join();
                return Err(io::Error::other(
                    "legacy execution Job registry is poisoned",
                ));
            }
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = jobs.entry(pid) {
            entry.insert(Arc::clone(&execution));
            true
        } else {
            false
        }
    };
    start_gate.wait();
    if !registered {
        let _ = execution.job.close_for_kill();
        let _ = worker.join();
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("legacy execution Job already exists for PID {pid}"),
        ));
    }
    drop(worker);
    Ok(())
}

fn legacy_job(pid: u32) -> io::Result<Option<Arc<LegacyExecution>>> {
    legacy_jobs()
        .lock()
        .map_err(|_| io::Error::other("legacy execution Job registry is poisoned"))
        .map(|jobs| jobs.get(&pid).cloned())
}

fn remove_legacy_job(pid: u32, expected: &Arc<LegacyExecution>) {
    let Ok(mut jobs) = legacy_jobs().lock() else {
        return;
    };
    if jobs
        .get(&pid)
        .is_some_and(|registered| Arc::ptr_eq(registered, expected))
    {
        jobs.remove(&pid);
    }
}

fn reap_legacy_job(pid: u32, execution: Arc<LegacyExecution>) {
    let deadline = Instant::now()
        .checked_add(LIFECYCLE_WAIT_HORIZON)
        .unwrap_or_else(Instant::now);
    let result = wait_handle_until(execution.process.as_raw(), deadline).and_then(|()| {
        if execution.job.active_processes()? != 0 {
            execution.job.terminate()?;
        }
        execution.job.wait_empty_until(deadline)?;
        execution.job.close_proven_empty()
    });
    if let Err(error) = result {
        tracing::warn!(
            pid,
            %error,
            "legacy execution Job reaper could not prove process-tree cleanup"
        );
        let _ = execution.job.close_for_kill();
    }
    remove_legacy_job(pid, &execution);
}

fn start_legacy_cleanup(
    pid: u32,
    execution: Arc<LegacyExecution>,
) -> io::Result<tokio::sync::oneshot::Receiver<io::Result<()>>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("nomi-legacy-job-cleanup-{pid}"))
        .spawn(move || {
            let deadline = Instant::now()
                .checked_add(CLEANUP_TIMEOUT)
                .unwrap_or_else(Instant::now);
            let result = wait_handle_until(execution.process.as_raw(), deadline)
                .and_then(|()| execution.job.wait_empty_until(deadline))
                .and_then(|()| execution.job.close_proven_empty());
            remove_legacy_job(pid, &execution);
            let _ = sender.send(result);
        })?;
    Ok(receiver)
}

fn duplicate_non_inheritable(handle: HANDLE) -> io::Result<OwnedHandle> {
    // SAFETY: the current process pseudo-handle and source process handle are
    // valid; DuplicateHandle writes one fresh non-inheritable handle.
    let current = unsafe { GetCurrentProcess() };
    let mut duplicate = ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            current,
            handle,
            current,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: DuplicateHandle returned a fresh owned handle.
    unsafe { OwnedHandle::from_raw(duplicate) }
}

fn resume_legacy_primary_thread(pid: u32) -> io::Result<()> {
    // SAFETY: the snapshot flags and process id are plain values.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    // SAFETY: a successful snapshot call returns a fresh owned handle.
    let snapshot = unsafe { OwnedHandle::from_raw(snapshot)? };
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(mem::size_of::<THREADENTRY32>())
            .expect("THREADENTRY32 fits in u32"),
        ..THREADENTRY32::default()
    };
    // SAFETY: snapshot is live and entry is writable storage with dwSize set.
    if unsafe { Thread32First(snapshot.as_raw(), &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut only_thread = None;
    loop {
        if entry.th32OwnerProcessID == pid
            && only_thread.replace(entry.th32ThreadID).is_some()
        {
            return Err(io::Error::other(format!(
                "suspended legacy PID {pid} exposed more than one thread before resume"
            )));
        }
        // SAFETY: snapshot remains live and entry remains writable.
        if unsafe { Thread32Next(snapshot.as_raw(), &mut entry) } == 0 {
            break;
        }
    }
    let thread_id = only_thread.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("primary thread for legacy PID {pid} was not found"),
        )
    })?;
    // SAFETY: OpenThread validates the unique thread id discovered for the
    // still-suspended exact process and returns a fresh handle on success.
    let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    // SAFETY: a non-null OpenThread result is a fresh owned handle.
    let thread = unsafe { OwnedHandle::from_raw(raw)? };
    // SAFETY: CREATE_SUSPENDED prevents user code from creating any other
    // threads before this call, and the exact process handle remains live.
    let previous = unsafe { ResumeThread(thread.as_raw()) };
    match previous {
        1 => Ok(()),
        u32::MAX => Err(io::Error::last_os_error()),
        count => Err(io::Error::other(format!(
            "legacy primary thread had unexpected suspend count {count}"
        ))),
    }
}

pub(super) async fn spawn_pty(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    cols: u16,
    rows: u16,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_inner(
        request,
        output,
        Arc::new(SystemWin32),
        SpawnTransport::Pty { cols, rows },
    )
    .await
}

    async fn spawn_pipe_inner(
        request: NormalizedExecutionRequest,
        output: Arc<OutputBuffer>,
        api: Arc<dyn Win32Facade>,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_inner(request, output, api, SpawnTransport::Pipe).await
}

#[cfg(test)]
async fn spawn_pty_inner(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    api: Arc<dyn Win32Facade>,
    cols: u16,
    rows: u16,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    spawn_inner(
        request,
        output,
        api,
        SpawnTransport::Pty { cols, rows },
    )
    .await
}

async fn spawn_inner(
    request: NormalizedExecutionRequest,
    output: Arc<OutputBuffer>,
    api: Arc<dyn Win32Facade>,
    transport: SpawnTransport,
) -> Result<SpawnedPlatformProcess, ExecutionError> {
    enforce_sandbox(&request)?;
    let prepared = PreparedCommand::new(&request)?;
    let setup_timeout = request
        .policy
        .deadline
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(SETUP_TIMEOUT)
        .min(SETUP_TIMEOUT);
    if setup_timeout.is_zero() {
        return Err(spawn_failed(io::Error::new(
            io::ErrorKind::TimedOut,
            "execution deadline elapsed before Windows ownership setup",
        )));
    }
    let deadline = Instant::now()
        .checked_add(setup_timeout)
        .ok_or_else(|| invalid_command("Windows setup deadline overflowed"))?;
    let runtime = tokio::runtime::Handle::current();
    let mut cancellation = StartCancellationGuard::new();
    let cancelled = cancellation.worker_flag();
    let mut transaction = tokio::task::spawn_blocking(move || {
        spawn_transaction(
            prepared,
            output,
            api,
            runtime,
            deadline,
            cancelled,
            transport,
        )
    });

    let owner = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        &mut transaction,
    )
    .await
    {
        Ok(joined) => joined
            .map_err(|error| {
                start_lost(
                    "windows_spawn_worker_failed",
                    format!("Windows spawn transaction failed to join: {error}"),
                    None,
                    CleanupReport::default(),
                )
            })??,
        Err(_) => {
            cancellation.cancel();
            match transaction.await {
                Ok(Ok(owner)) => {
                    let now = Instant::now();
                    let last_known = Some(ProcessSnapshot {
                        pid: owner.pid,
                        state: ProcessState::Lost,
                        started_at: now,
                        last_activity_at: now,
                    });
                    let cleanup_started = Instant::now();
                    let mut cleanup = CleanupReport {
                        force_kill_attempted: true,
                        ..CleanupReport::default()
                    };
                    if let Err(error) = owner.force_kill().await {
                        cleanup
                            .errors
                            .push(format!("terminate execution Job after start timeout: {error}"));
                    }
                    let cleanup_deadline = Instant::now()
                        .checked_add(CLEANUP_TIMEOUT)
                        .unwrap_or_else(Instant::now);
                    match owner.wait_reaped(cleanup_deadline).await {
                        Ok(_) => cleanup.reaped = true,
                        Err(error) => cleanup.errors.push(format!(
                            "wait exact process and empty Job after start timeout: {error}"
                        )),
                    }
                    cleanup.elapsed = cleanup_started.elapsed();
                    return Err(start_lost(
                        "windows_spawn_deadline_after_resume",
                        "Windows spawn crossed its setup deadline after user code may have resumed"
                            .to_owned(),
                        last_known,
                        cleanup,
                    ));
                }
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    return Err(start_lost(
                        "windows_spawn_worker_failed",
                        format!(
                            "Windows spawn transaction failed to join after deadline cancellation: {error}"
                        ),
                        None,
                        CleanupReport {
                            errors: vec![
                                "the blocking transaction unwound before it could return a cleanup report"
                                    .to_owned(),
                            ],
                            ..CleanupReport::default()
                        },
                    ));
                }
            }
        }
    };
    cancellation.disarm();

    Ok(SpawnedPlatformProcess {
        owner: Arc::new(owner),
    })
}

#[derive(Clone, Copy)]
enum SpawnTransport {
    Pipe,
    Pty { cols: u16, rows: u16 },
}

fn spawn_transaction(
    mut prepared: PreparedCommand,
    output: Arc<OutputBuffer>,
    api: Arc<dyn Win32Facade>,
    runtime: tokio::runtime::Handle,
    deadline: Instant,
    cancellation: Arc<StartCancellation>,
    transport: SpawnTransport,
) -> Result<WindowsOwner, ExecutionError> {
    ensure_setup_active(deadline, &cancellation).map_err(spawn_failed)?;

    let io = PreparedIo::new(transport).map_err(spawn_failed)?;

    ensure_setup_active(deadline, &cancellation).map_err(spawn_failed)?;
    let job = create_execution_job().map_err(spawn_failed)?;
    let job = Arc::new(JobControl::new(job));
    let mut transaction = SpawnTransaction::new(Arc::clone(&job), io);

    let mut attributes = match ProcThreadAttributeList::new_one() {
        Ok(attributes) => attributes,
        Err(error) => return Err(transaction.into_failure(error, deadline)),
    };
    if let Err(error) = transaction.configure_attributes(&mut attributes) {
        return Err(transaction.into_failure(error, deadline));
    }

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb =
        u32::try_from(mem::size_of::<STARTUPINFOEXW>()).expect("STARTUPINFOEXW fits in u32");
    transaction.configure_startup(&mut startup);
    startup.lpAttributeList = attributes.as_mut_ptr();

    if let Err(error) = ensure_setup_active(deadline, &cancellation) {
        return Err(transaction.into_failure(error, deadline));
    }

    let mut process_information = PROCESS_INFORMATION::default();
    let flags = CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT
        | transaction.extra_creation_flags();
    // SAFETY: every pointer refers to live, NUL-terminated backing storage for
    // the duration of the call. The mutable command-line buffer is exclusively
    // owned by this transaction. The handle list contains exactly the three
    // inheritable child stdio handles.
    let created = unsafe {
        CreateProcessW(
            prepared.application.as_ptr(),
            prepared.command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            transaction.inherit_handles(),
            flags,
            prepared.environment.as_ptr().cast::<c_void>(),
            prepared.cwd.as_ptr(),
            &startup.StartupInfo,
            &mut process_information,
        )
    };
    if created == 0 {
        return Err(transaction.into_failure(io::Error::last_os_error(), deadline));
    }

    let process = match unsafe { OwnedHandle::from_raw(process_information.hProcess) } {
        Ok(process) => Arc::new(process),
        Err(error) => {
            if !process_information.hThread.is_null() {
                // SAFETY: CreateProcessW returned this fresh thread handle, but
                // the process handle was invalid so there is no safe transaction
                // object to adopt it into.
                let _ = unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(process_information.hThread)
                };
            }
            return Err(transaction.into_failure(error, deadline));
        }
    };
    let thread = match unsafe { OwnedHandle::from_raw(process_information.hThread) } {
        Ok(thread) => thread,
        Err(error) => {
            transaction.process = Some(process);
            transaction.phase = SpawnPhase::Suspended;
            return Err(transaction.into_failure(error, deadline));
        }
    };
    transaction.pid = Some(process_information.dwProcessId);
    transaction.process = Some(process);
    transaction.thread = Some(thread);
    transaction.phase = SpawnPhase::Suspended;
    #[cfg(test)]
    if let Err(error) = api.process_created(
        process_information.dwProcessId,
        transaction
            .process
            .as_ref()
            .expect("created process is owned")
            .as_raw(),
        transaction
            .thread
            .as_ref()
            .expect("created primary thread is owned")
            .as_raw(),
    ) {
        return Err(transaction.into_failure(error, deadline));
    }

    if let Err(error) = ensure_setup_active(deadline, &cancellation) {
        return Err(transaction.into_failure(error, deadline));
    }
    let job_handle = match job.raw_handle() {
        Some(handle) => handle,
        None => {
            return Err(transaction.into_failure(
                io::Error::other("execution Job closed before assignment"),
                deadline,
            ));
        }
    };
    if let Err(error) = api.assign_process_to_job(
        job_handle,
        transaction
            .process
            .as_ref()
            .expect("created process is owned")
            .as_raw(),
    ) {
        return Err(transaction.into_failure(error, deadline));
    }
    if let Err(error) = ensure_setup_active(deadline, &cancellation) {
        return Err(transaction.into_failure(error, deadline));
    }
    let previous_suspend_count = match cancellation.resume_thread_if_active(
        deadline,
        api.as_ref(),
        transaction
            .thread
            .as_ref()
            .expect("created primary thread is owned")
            .as_raw(),
    ) {
        Ok(count) => count,
        Err(error) => return Err(transaction.into_failure(error, deadline)),
    };
    match previous_suspend_count {
        1 => transaction.phase = SpawnPhase::Resumed,
        0 => {
            transaction.phase = SpawnPhase::Resumed;
            return Err(transaction.into_failure(
                io::Error::other(
                    "ResumeThread reported an already-running primary thread",
                ),
                deadline,
            ));
        }
        count => {
            return Err(transaction.into_failure(
                io::Error::other(format!(
                    "ResumeThread left an unexpected suspend count: {count}"
                )),
                deadline,
            ));
        }
    }

    transaction.close_child_only_handles();
    transaction.thread.take();

    if let Err(error) = ensure_setup_active(deadline, &cancellation) {
        return Err(transaction.into_failure(error, deadline));
    }

    let (stdin, readers, pseudoconsole) = transaction.take_runtime_io(&runtime, output);
    let owner_pseudoconsole = transaction.pseudoconsole_control();
    let pty_input_close_not_before = owner_pseudoconsole.as_ref().map(|_| {
        Instant::now()
            .checked_add(CONPTY_INPUT_CLOSE_GRACE)
            .unwrap_or_else(Instant::now)
    });
    let (completion_sender, completion_receiver) = watch::channel(LifecycleCompletion::Running);
    let lifecycle_process = Arc::clone(
        transaction
            .process
            .as_ref()
            .expect("created process remains owned"),
    );
    let lifecycle_job = Arc::clone(&job);
    runtime.spawn_blocking(move || {
        let failure_pseudoconsole = pseudoconsole.clone();
        let lifecycle_deadline = Instant::now()
            .checked_add(LIFECYCLE_WAIT_HORIZON)
            .unwrap_or_else(Instant::now);
        let result = run_lifecycle(
            Arc::clone(&lifecycle_process),
            Arc::clone(&lifecycle_job),
            readers,
            pseudoconsole,
            lifecycle_deadline,
        );
        let completion = match result {
            Ok(fact) => LifecycleCompletion::Reaped(fact),
            Err(error) => {
                let kind = error.kind();
                let mut message = lifecycle_failure_message(
                    error,
                    &lifecycle_process,
                    &lifecycle_job,
                );
                if let Some(pseudoconsole) = failure_pseudoconsole {
                    let close_deadline = Instant::now()
                        .checked_add(conpty::CLOSE_TIMEOUT)
                        .unwrap_or_else(Instant::now);
                    if let Err(error) = pseudoconsole.close_until(close_deadline) {
                        message.push_str(&format!(
                            "; close pseudoconsole after lifecycle failure: {error}"
                        ));
                    }
                }
                LifecycleCompletion::Failed {
                    kind,
                    message: Arc::from(message),
                }
            }
        };
        completion_sender.send_replace(completion);
    });

    let pid = transaction.pid.expect("created process has a PID");
    let process = transaction
        .process
        .take()
        .expect("created process is transferred to the owner");
    transaction.disarmed = true;
    Ok(WindowsOwner {
        pid,
        process,
        job,
        stdin: Arc::new(tokio::sync::Mutex::new(Some(stdin))),
        pseudoconsole: owner_pseudoconsole,
        pty_input_close_not_before,
        completion: completion_receiver,
    })
}

trait Win32Facade: Send + Sync {
    #[cfg(test)]
    fn process_created(&self, _pid: u32, _process: HANDLE, _thread: HANDLE) -> io::Result<()> {
        Ok(())
    }

    fn assign_process_to_job(&self, job: HANDLE, process: HANDLE) -> io::Result<()>;
    fn resume_thread(&self, thread: HANDLE) -> io::Result<u32>;
}

struct SystemWin32;

impl Win32Facade for SystemWin32 {
    fn assign_process_to_job(&self, job: HANDLE, process: HANDLE) -> io::Result<()> {
        // SAFETY: both handles are live kernel handles owned by the spawn
        // transaction, and the process is still suspended.
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn resume_thread(&self, thread: HANDLE) -> io::Result<u32> {
        // SAFETY: thread is the live primary thread returned by CreateProcessW.
        let previous = unsafe { ResumeThread(thread) };
        if previous == u32::MAX {
            Err(io::Error::last_os_error())
        } else {
            Ok(previous)
        }
    }
}

struct StartCancellationGuard {
    state: Arc<StartCancellation>,
    armed: bool,
}

impl StartCancellationGuard {
    fn new() -> Self {
        Self {
            state: Arc::new(StartCancellation {
                cancelled: AtomicBool::new(false),
                resume_gate: Mutex::new(()),
            }),
            armed: true,
        }
    }

    fn worker_flag(&self) -> Arc<StartCancellation> {
        Arc::clone(&self.state)
    }

    fn cancel(&self) {
        self.state.cancel();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

struct StartCancellation {
    cancelled: AtomicBool,
    resume_gate: Mutex<()>,
}

impl StartCancellation {
    fn cancel(&self) {
        let _gate = match self.resume_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.cancelled.store(true, AtomicOrdering::Release);
    }

    fn resume_thread_if_active(
        &self,
        deadline: Instant,
        api: &dyn Win32Facade,
        thread: HANDLE,
    ) -> io::Result<u32> {
        let _gate = self
            .resume_gate
            .lock()
            .map_err(|_| io::Error::other("Windows resume cancellation gate is poisoned"))?;
        ensure_setup_active(deadline, self)?;
        api.resume_thread(thread)
    }
}

impl Drop for StartCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpawnPhase {
    Preparing,
    Suspended,
    Resumed,
}

struct SpawnTransaction {
    pid: Option<u32>,
    process: Option<Arc<OwnedHandle>>,
    thread: Option<OwnedHandle>,
    job: Arc<JobControl>,
    io: Option<PreparedIo>,
    phase: SpawnPhase,
    disarmed: bool,
}

impl SpawnTransaction {
    fn new(job: Arc<JobControl>, io: PreparedIo) -> Self {
        Self {
            pid: None,
            process: None,
            thread: None,
            job,
            io: Some(io),
            phase: SpawnPhase::Preparing,
            disarmed: false,
        }
    }

    fn configure_attributes(
        &mut self,
        attributes: &mut ProcThreadAttributeList,
    ) -> io::Result<()> {
        self.io
            .as_ref()
            .expect("transport I/O is owned")
            .configure_attributes(attributes)
    }

    fn configure_startup(&self, startup: &mut STARTUPINFOEXW) {
        self.io
            .as_ref()
            .expect("transport I/O is owned")
            .configure_startup(startup);
    }

    fn inherit_handles(&self) -> i32 {
        self.io
            .as_ref()
            .expect("transport I/O is owned")
            .inherit_handles()
    }

    fn extra_creation_flags(&self) -> u32 {
        self.io
            .as_ref()
            .expect("transport I/O is owned")
            .extra_creation_flags()
    }

    fn close_child_only_handles(&mut self) {
        self.io
            .as_mut()
            .expect("transport I/O is owned")
            .close_child_only_handles();
    }

    fn take_runtime_io(
        &mut self,
        runtime: &tokio::runtime::Handle,
        output: Arc<OutputBuffer>,
    ) -> (
        OwnedHandle,
        Vec<ReaderCompletion>,
        Option<Arc<PseudoConsoleControl>>,
    ) {
        self.io
            .as_mut()
            .expect("transport I/O is owned")
            .take_runtime_io(runtime, output)
    }

    fn pseudoconsole_control(&self) -> Option<Arc<PseudoConsoleControl>> {
        self.io
            .as_ref()
            .and_then(PreparedIo::pseudoconsole_control)
    }

    fn into_failure(mut self, error: io::Error, setup_deadline: Instant) -> ExecutionError {
        let phase = self.phase;
        let last_known = self.pid.map(|pid| ProcessSnapshot {
            pid,
            state: ProcessState::Lost,
            started_at: Instant::now(),
            last_activity_at: Instant::now(),
        });
        let cleanup_deadline = setup_deadline.max(
            Instant::now()
                .checked_add(CLEANUP_TIMEOUT)
                .unwrap_or(setup_deadline),
        );
        let cleanup = self.cleanup(cleanup_deadline);
        self.disarmed = true;

        if phase != SpawnPhase::Resumed && cleanup.reaped && cleanup.errors.is_empty() {
            spawn_failed(error)
        } else {
            start_lost(
                if phase == SpawnPhase::Resumed {
                    "windows_post_resume_failure"
                } else {
                    "windows_pre_resume_cleanup_unproven"
                },
                error.to_string(),
                last_known,
                cleanup,
            )
        }
    }

    fn cleanup(&mut self, deadline: Instant) -> CleanupReport {
        let mut cleanup = CleanupReport {
            force_kill_attempted: self.process.is_some(),
            ..CleanupReport::default()
        };
        let started = Instant::now();
        self.io
            .as_mut()
            .map(PreparedIo::close_input_and_child_handles);
        self.thread.take();

        if let Some(process) = &self.process {
            if self.phase == SpawnPhase::Resumed {
                if let Err(error) = self.job.terminate() {
                    cleanup
                        .errors
                        .push(format!("terminate execution Job: {error}"));
                }
            } else {
                // SAFETY: process is the exact suspended process created by
                // this transaction. No user instruction has run.
                if unsafe { TerminateProcess(process.as_raw(), TERMINATED_BY_HOST_EXIT_CODE) } == 0 {
                    let error = io::Error::last_os_error();
                    if !handle_is_signaled(process.as_raw()).unwrap_or(false) {
                        cleanup
                            .errors
                            .push(format!("terminate suspended process: {error}"));
                    }
                }
            }

            match wait_handle_until(process.as_raw(), deadline) {
                Ok(()) => {
                    cleanup.reaped = true;
                    if let Err(error) = self.job.wait_empty_until(deadline) {
                        cleanup
                            .errors
                            .push(format!("prove execution Job empty: {error}"));
                    } else if let Err(error) = self.job.close_proven_empty() {
                        cleanup
                            .errors
                            .push(format!("close empty execution Job: {error}"));
                    }
                }
                Err(error) => cleanup
                    .errors
                    .push(format!("wait exact process handle: {error}")),
            }
        } else {
            if let Err(error) = self.job.wait_empty_until(deadline) {
                cleanup
                    .errors
                    .push(format!("prove uncommitted execution Job empty: {error}"));
            } else if let Err(error) = self.job.close_proven_empty() {
                cleanup
                    .errors
                    .push(format!("close uncommitted execution Job: {error}"));
            } else {
                cleanup.reaped = true;
            }
        }
        if !cleanup.errors.is_empty() {
            let _ = self.job.close_for_kill();
        }
        if let Some(io) = self.io.as_mut()
            && let Err(error) = io.finish_cleanup(deadline)
        {
            cleanup
                .errors
                .push(format!("close Windows transport: {error}"));
        }
        self.io.take();
        self.process.take();
        cleanup.elapsed = started.elapsed();
        cleanup
    }
}

impl Drop for SpawnTransaction {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let deadline = Instant::now()
            .checked_add(CLEANUP_TIMEOUT)
            .unwrap_or_else(Instant::now);
        let _ = self.cleanup(deadline);
    }
}

struct PipePair {
    read: OwnedHandle,
    write: OwnedHandle,
}

enum PreparedIo {
    Pipe {
        parent_stdin: Option<OwnedHandle>,
        child_stdin: Option<OwnedHandle>,
        parent_stdout: Option<OwnedHandle>,
        child_stdout: Option<OwnedHandle>,
        parent_stderr: Option<OwnedHandle>,
        child_stderr: Option<OwnedHandle>,
    },
    Pty {
        input: Option<OwnedHandle>,
        output: Option<OwnedHandle>,
        control: Arc<PseudoConsoleControl>,
    },
}

impl PreparedIo {
    fn new(transport: SpawnTransport) -> io::Result<Self> {
        match transport {
            SpawnTransport::Pipe => {
                let stdin = create_inheritable_pipe()?;
                let stdout = create_inheritable_pipe()?;
                let stderr = create_inheritable_pipe()?;
                clear_inheritance(stdin.write.as_raw())?;
                clear_inheritance(stdout.read.as_raw())?;
                clear_inheritance(stderr.read.as_raw())?;
                Ok(Self::Pipe {
                    parent_stdin: Some(stdin.write),
                    child_stdin: Some(stdin.read),
                    parent_stdout: Some(stdout.read),
                    child_stdout: Some(stdout.write),
                    parent_stderr: Some(stderr.read),
                    child_stderr: Some(stderr.write),
                })
            }
            SpawnTransport::Pty { cols, rows } => {
                let prepared = PreparedConPty::create(cols, rows, || {
                    let pair = create_inheritable_pipe()?;
                    Ok((pair.read, pair.write))
                })?;
                clear_inheritance(prepared.input.as_raw())?;
                clear_inheritance(prepared.output.as_raw())?;
                let (control, input, output) = prepared.into_parts();
                Ok(Self::Pty {
                    input: Some(input),
                    output: Some(output),
                    control,
                })
            }
        }
    }

    fn configure_attributes(
        &self,
        attributes: &mut ProcThreadAttributeList,
    ) -> io::Result<()> {
        match self {
            Self::Pipe {
                child_stdin,
                child_stdout,
                child_stderr,
                ..
            } => attributes.set_handle_list(&[
                child_stdin.as_ref().expect("child stdin is owned").as_raw(),
                child_stdout
                    .as_ref()
                    .expect("child stdout is owned")
                    .as_raw(),
                child_stderr
                    .as_ref()
                    .expect("child stderr is owned")
                    .as_raw(),
            ]),
            Self::Pty { control, .. } => attributes.set_pseudoconsole(control.raw()?),
        }
    }

    fn configure_startup(&self, startup: &mut STARTUPINFOEXW) {
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        match self {
            Self::Pipe {
                child_stdin,
                child_stdout,
                child_stderr,
                ..
            } => {
                startup.StartupInfo.hStdInput =
                    child_stdin.as_ref().expect("child stdin is owned").as_raw();
                startup.StartupInfo.hStdOutput = child_stdout
                    .as_ref()
                    .expect("child stdout is owned")
                    .as_raw();
                startup.StartupInfo.hStdError = child_stderr
                    .as_ref()
                    .expect("child stderr is owned")
                    .as_raw();
            }
            Self::Pty { .. } => {
                startup.StartupInfo.hStdInput = -1isize as HANDLE;
                startup.StartupInfo.hStdOutput = -1isize as HANDLE;
                startup.StartupInfo.hStdError = -1isize as HANDLE;
            }
        }
    }

    const fn inherit_handles(&self) -> i32 {
        match self {
            Self::Pipe { .. } => 1,
            Self::Pty { .. } => 0,
        }
    }

    const fn extra_creation_flags(&self) -> u32 {
        match self {
            Self::Pipe { .. } => CREATE_NO_WINDOW,
            Self::Pty { .. } => 0,
        }
    }

    fn close_child_only_handles(&mut self) {
        if let Self::Pipe {
            child_stdin,
            child_stdout,
            child_stderr,
            ..
        } = self
        {
            child_stdin.take();
            child_stdout.take();
            child_stderr.take();
        }
    }

    fn take_runtime_io(
        &mut self,
        runtime: &tokio::runtime::Handle,
        output: Arc<OutputBuffer>,
    ) -> (
        OwnedHandle,
        Vec<ReaderCompletion>,
        Option<Arc<PseudoConsoleControl>>,
    ) {
        match self {
            Self::Pipe {
                parent_stdin,
                parent_stdout,
                parent_stderr,
                ..
            } => {
                let stdin = parent_stdin.take().expect("parent stdin is owned");
                let stdout = parent_stdout.take().expect("parent stdout is owned");
                let stderr = parent_stderr.take().expect("parent stderr is owned");
                (
                    stdin,
                    vec![
                        start_reader(
                            runtime,
                            stdout,
                            OutputStream::Stdout,
                            Arc::clone(&output),
                        ),
                        start_reader(runtime, stderr, OutputStream::Stderr, output),
                    ],
                    None,
                )
            }
            Self::Pty {
                input,
                output: pty_output,
                control,
            } => {
                let input = input.take().expect("ConPTY input is owned");
                let output_handle = pty_output.take().expect("ConPTY output is owned");
                (
                    input,
                    vec![start_reader(runtime, output_handle, OutputStream::Pty, output)],
                    Some(Arc::clone(control)),
                )
            }
        }
    }

    fn pseudoconsole_control(&self) -> Option<Arc<PseudoConsoleControl>> {
        match self {
            Self::Pipe { .. } => None,
            Self::Pty { control, .. } => Some(Arc::clone(control)),
        }
    }

    fn close_input_and_child_handles(&mut self) {
        match self {
            Self::Pipe {
                parent_stdin,
                child_stdin,
                child_stdout,
                child_stderr,
                ..
            } => {
                parent_stdin.take();
                child_stdin.take();
                child_stdout.take();
                child_stderr.take();
            }
            Self::Pty { input, .. } => {
                input.take();
            }
        }
    }

    fn finish_cleanup(&mut self, deadline: Instant) -> io::Result<()> {
        match self {
            Self::Pipe { .. } => Ok(()),
            Self::Pty {
                input,
                output,
                control,
            } => {
                input.take();
                let close_deadline = deadline.min(
                    Instant::now()
                        .checked_add(conpty::CLOSE_TIMEOUT)
                        .unwrap_or(deadline),
                );
                let result = control.close_until(close_deadline);
                output.take();
                result
            }
        }
    }
}

fn create_inheritable_pipe() -> io::Result<PipePair> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(mem::size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES fits in u32"),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    // SAFETY: output pointers and SECURITY_ATTRIBUTES are valid for the call.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe succeeded and returned two fresh owned handles.
    let read = unsafe { OwnedHandle::from_raw(read)? };
    // SAFETY: CreatePipe succeeded and returned a second fresh owned handle.
    let write = unsafe { OwnedHandle::from_raw(write)? };
    Ok(PipePair { read, write })
}

fn clear_inheritance(handle: HANDLE) -> io::Result<()> {
    // SAFETY: handle is a live pipe handle owned by the transaction.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_execution_job() -> io::Result<OwnedHandle> {
    // SAFETY: null security/name pointers request a non-inheritable anonymous Job.
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    // SAFETY: a non-null result is a fresh Job handle.
    let job = unsafe { OwnedHandle::from_raw(raw)? };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: job and limits remain valid for the duration of the call.
    if unsafe {
        SetInformationJobObject(
            job.as_raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
            u32::try_from(mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("JOBOBJECT_EXTENDED_LIMIT_INFORMATION fits in u32"),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

struct JobControl {
    state: Mutex<JobState>,
}

struct JobState {
    handle: Option<OwnedHandle>,
    termination_requested: bool,
    empty_proven: bool,
}

impl JobControl {
    fn new(handle: OwnedHandle) -> Self {
        Self {
            state: Mutex::new(JobState {
                handle: Some(handle),
                termination_requested: false,
                empty_proven: false,
            }),
        }
    }

    fn raw_handle(&self) -> Option<HANDLE> {
        let state = self.state.lock().ok()?;
        state.handle.as_ref().map(OwnedHandle::as_raw)
    }

    fn terminate(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("execution Job state is poisoned"))?;
        if state.empty_proven {
            return Ok(());
        }
        if state.termination_requested {
            return Ok(());
        }
        let handle = state
            .handle
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "execution Job is closed"))?
            .as_raw();
        // SAFETY: handle is kept live by the locked Job state.
        if unsafe { TerminateJobObject(handle, TERMINATED_BY_HOST_EXIT_CODE) } == 0 {
            let error = io::Error::last_os_error();
            if query_active_processes(handle)? != 0 {
                return Err(error);
            }
            state.empty_proven = true;
        }
        state.termination_requested = true;
        Ok(())
    }

    fn active_processes(&self) -> io::Result<u32> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("execution Job state is poisoned"))?;
        if state.empty_proven {
            return Ok(0);
        }
        let handle = state
            .handle
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "execution Job is closed"))?
            .as_raw();
        let active = query_active_processes(handle)?;
        if active == 0 {
            state.empty_proven = true;
        }
        Ok(active)
    }

    fn close_proven_empty(&self) -> io::Result<()> {
        let handle = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("execution Job state is poisoned"))?;
            if !state.empty_proven {
                return Err(io::Error::other(
                    "execution Job cannot close before empty membership is proven",
                ));
            }
            state.handle.take()
        };
        drop(handle);
        Ok(())
    }

    fn wait_empty_until(&self, deadline: Instant) -> io::Result<()> {
        loop {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "execution Job did not become empty before the cleanup deadline",
                ));
            }
            std::thread::sleep(
                JOB_EMPTY_POLL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn close_for_kill(&self) -> io::Result<()> {
        let handle = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("execution Job state is poisoned"))?;
            state.termination_requested = true;
            state.handle.take()
        };
        drop(handle);
        Ok(())
    }
}

fn query_active_processes(job: HANDLE) -> io::Result<u32> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: job is live and accounting is valid writable storage.
    if unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast::<c_void>(),
            u32::try_from(mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                .expect("JOBOBJECT_BASIC_ACCOUNTING_INFORMATION fits in u32"),
            ptr::null_mut(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(accounting.ActiveProcesses)
    }
}

#[derive(Clone)]
enum LifecycleCompletion {
    Running,
    Reaped(ExitFact),
    Failed {
        kind: io::ErrorKind,
        message: Arc<str>,
    },
}

struct ReaderCompletion {
    receiver: mpsc::Receiver<io::Result<()>>,
}

fn start_reader(
    runtime: &tokio::runtime::Handle,
    handle: OwnedHandle,
    stream: OutputStream,
    output: Arc<OutputBuffer>,
) -> ReaderCompletion {
    let (sender, receiver) = mpsc::sync_channel(1);
    runtime.spawn_blocking(move || {
        let _ = sender.send(read_stream(handle, stream, output));
    });
    ReaderCompletion { receiver }
}

fn read_stream(
    handle: OwnedHandle,
    stream: OutputStream,
    output: Arc<OutputBuffer>,
) -> io::Result<()> {
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let mut read = 0_u32;
        // SAFETY: buffer is writable for its full length and handle is a live
        // synchronous pipe read handle owned by this worker.
        let result = unsafe {
            ReadFile(
                handle.as_raw(),
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).expect("reader buffer fits in u32"),
                &mut read,
                ptr::null_mut(),
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error().map(|code| code as u32),
                Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA)
            ) {
                return Ok(());
            }
            return Err(error);
        }
        if read == 0 {
            return Ok(());
        }
        output.push(stream, &buffer[..read as usize]);
    }
}

fn run_lifecycle(
    process: Arc<OwnedHandle>,
    job: Arc<JobControl>,
    readers: Vec<ReaderCompletion>,
    pseudoconsole: Option<Arc<PseudoConsoleControl>>,
    deadline: Instant,
) -> io::Result<ExitFact> {
    wait_handle_until(process.as_raw(), deadline)?;
    let mut exit_code = 0_u32;
    // SAFETY: process is signaled and its exact handle remains live.
    if unsafe { GetExitCodeProcess(process.as_raw(), &mut exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }

    if job.active_processes()? != 0 {
        job.terminate()?;
        job.wait_empty_until(deadline)?;
    }
    job.close_proven_empty()?;
    let mut cleanup_errors = Vec::new();
    if let Some(pseudoconsole) = pseudoconsole {
        let close_deadline = deadline.min(
            Instant::now()
                .checked_add(CONPTY_NATURAL_CLOSE_WAIT)
                .unwrap_or(deadline),
        );
        if let Err(error) = pseudoconsole.close_until(close_deadline) {
            cleanup_errors.push(format!("close pseudoconsole: {error}"));
        }
        drain_pty_readers_after_close(readers, deadline);
    } else {
        finish_readers(readers, deadline)?;
    }

    Ok(ExitFact {
        code: Some(exit_code as i32),
        signal: None,
        cleanup_errors,
    })
}

fn lifecycle_failure_message(
    error: io::Error,
    process: &OwnedHandle,
    job: &JobControl,
) -> String {
    let deadline = Instant::now()
        .checked_add(CLEANUP_TIMEOUT)
        .unwrap_or_else(Instant::now);
    let mut cleanup_errors = Vec::new();
    if let Err(cleanup_error) = job.terminate() {
        cleanup_errors.push(format!("terminate execution Job: {cleanup_error}"));
    }
    if let Err(cleanup_error) = wait_handle_until(process.as_raw(), deadline) {
        cleanup_errors.push(format!("wait exact process handle: {cleanup_error}"));
    }
    if let Err(cleanup_error) = job.wait_empty_until(deadline) {
        cleanup_errors.push(format!("prove execution Job empty: {cleanup_error}"));
    } else if let Err(cleanup_error) = job.close_proven_empty() {
        cleanup_errors.push(format!("close proven-empty execution Job: {cleanup_error}"));
    }
    if !cleanup_errors.is_empty() {
        let _ = job.close_for_kill();
        format!(
            "{error}; fail-closed lifecycle cleanup: {}",
            cleanup_errors.join("; ")
        )
    } else {
        error.to_string()
    }
}

fn finish_readers(readers: Vec<ReaderCompletion>, caller_deadline: Instant) -> io::Result<()> {
    let deadline = caller_deadline.min(
        Instant::now()
            .checked_add(POST_EXIT_READER_DRAIN)
            .unwrap_or(caller_deadline),
    );
    let mut first_error = None;
    for reader in readers {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = if remaining.is_zero() {
            Err(mpsc::RecvTimeoutError::Timeout)
        } else {
            reader.receiver.recv_timeout(remaining)
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) if first_error.is_none() => {
                first_error = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "output reader timed out",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) if first_error.is_none() => {
                first_error = Some(io::Error::other(
                    "output reader ended without publishing a result",
                ));
            }
            Err(_) => {}
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn drain_pty_readers_after_close(
    readers: Vec<ReaderCompletion>,
    caller_deadline: Instant,
) {
    let deadline = caller_deadline.min(
        Instant::now()
            .checked_add(POST_EXIT_READER_DRAIN)
            .unwrap_or(caller_deadline),
    );
    for reader in readers {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match reader.receiver.recv_timeout(remaining) {
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return,
        }
    }
}

struct WindowsOwner {
    pid: u32,
    process: Arc<OwnedHandle>,
    job: Arc<JobControl>,
    stdin: Arc<tokio::sync::Mutex<Option<OwnedHandle>>>,
    pseudoconsole: Option<Arc<PseudoConsoleControl>>,
    pty_input_close_not_before: Option<Instant>,
    completion: watch::Receiver<LifecycleCompletion>,
}

impl Drop for WindowsOwner {
    fn drop(&mut self) {
        let _ = self.job.close_for_kill();
        if let Some(pseudoconsole) = &self.pseudoconsole {
            let _ = pseudoconsole.begin_close();
        }
    }
}

#[async_trait]
impl ProcessOwner for WindowsOwner {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let stdin = Arc::clone(&self.stdin).lock_owned().await;
        if stdin.is_none() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed"));
        }
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            let handle = stdin
                .as_ref()
                .expect("stdin was validated before entering the blocking writer")
                .as_raw();
            write_all(handle, &bytes)
        })
            .await
            .map_err(|error| io::Error::other(format!("stdin writer task failed: {error}")))?
    }

    async fn close_stdin(&self) -> io::Result<()> {
        let mut stdin = self.stdin.lock().await;
        if stdin.is_none() {
            return Ok(());
        }
        if let Some(not_before) = self.pty_input_close_not_before {
            let remaining = not_before.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
            let stdin = stdin
                .take()
                .expect("ConPTY stdin was validated before the EOF write");
            tokio::task::spawn_blocking(move || {
                // In the default ConPTY cooked-input contract, CR submits any
                // pending line and SUB is the Windows console/CRT EOF
                // character. Closing the input pipe alone is observed by
                // console clients as CTRL_CLOSE/CONTROL_C_EXIT rather than a
                // successful stdin EOF. Arbitrary raw-mode clients cannot
                // expose a truthful generic EOF operation through ConPTY.
                write_all(stdin.as_raw(), b"\r\n\x1a")?;
                std::thread::sleep(Duration::from_millis(25));
                Ok::<(), io::Error>(())
            })
            .await
            .map_err(|error| io::Error::other(format!("PTY EOF writer task failed: {error}")))??;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ConPTY cannot prove generic stdin EOF; a default cooked-console EOF \
                 sequence was delivered before closing input",
            ));
        }
        stdin.take();
        Ok(())
    }

    async fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.pseudoconsole
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "pipe transport does not support terminal resize",
                )
            })?
            .resize(cols, rows)
    }

    async fn interrupt(&self) -> io::Result<()> {
        if self.pseudoconsole.is_some() {
            // In ConPTY's VT input contract, ETX is the terminal Ctrl+C
            // keystroke. Tree-wide escalation remains owned by the execution
            // Job if the foreground application ignores it.
            self.write(b"\x03").await
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "CREATE_NO_WINDOW pipe processes have no truthful console interrupt contract",
            ))
        }
    }

    async fn terminate(&self) -> io::Result<()> {
        self.job.terminate()
    }

    async fn force_kill(&self) -> io::Result<()> {
        self.job.terminate()
    }

    async fn wait_reaped(&self, deadline: Instant) -> io::Result<ExitFact> {
        let _keep_exact_process_handle_alive = &self.process;
        let mut completion = self.completion.clone();
        loop {
            match completion.borrow().clone() {
                LifecycleCompletion::Running => {}
                LifecycleCompletion::Reaped(fact) => return Ok(fact),
                LifecycleCompletion::Failed { kind, message } => {
                    return Err(io::Error::new(kind, message.to_string()));
                }
            }
            match tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                completion.changed(),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    let _ = self.job.close_for_kill();
                    return Err(io::Error::other(
                        "Windows lifecycle worker ended without a result",
                    ));
                }
                Err(_) => {
                    let _ = self.job.close_for_kill();
                    if let Some(pseudoconsole) = &self.pseudoconsole {
                        let _ = pseudoconsole.begin_close();
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "exact Windows process wait timed out",
                    ));
                }
            }
        }
    }
}

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let chunk_len = bytes.len().min(WRITE_CHUNK_BYTES);
        let mut written = 0_u32;
        // SAFETY: bytes points to chunk_len readable bytes and handle is kept
        // live by the Arc owned by the blocking writer.
        if unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                u32::try_from(chunk_len).expect("chunk length is bounded to u32"),
                &mut written,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "Windows stdin pipe accepted zero bytes",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn wait_handle_until(handle: HANDLE, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let milliseconds = if remaining.is_zero() {
            0
        } else {
            remaining
                .as_millis()
                .saturating_add(1)
                .min((u32::MAX - 1) as u128) as u32
        };
        // SAFETY: handle is a live process handle for the duration of the wait.
        match unsafe { WaitForSingleObject(handle, milliseconds) } {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_TIMEOUT if Instant::now() < deadline => {}
            WAIT_TIMEOUT => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "exact process termination timed out",
                ));
            }
            WAIT_FAILED => return Err(io::Error::last_os_error()),
            result => {
                return Err(io::Error::other(format!(
                    "unexpected exact process wait result: {result}"
                )));
            }
        }
    }
}

fn handle_is_signaled(handle: HANDLE) -> io::Result<bool> {
    // SAFETY: handle is live for this nonblocking probe.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        result => Err(io::Error::other(format!(
            "unexpected process liveness probe result: {result}"
        ))),
    }
}

fn ensure_setup_active(deadline: Instant, cancellation: &StartCancellation) -> io::Result<()> {
    if cancellation.cancelled.load(AtomicOrdering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Windows start future was cancelled",
        ));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Windows spawn transaction exceeded its setup deadline",
        ));
    }
    Ok(())
}

struct PreparedCommand {
    application: Vec<u16>,
    command_line: Vec<u16>,
    cwd: Vec<u16>,
    environment: Vec<u16>,
}

impl PreparedCommand {
    fn new(request: &NormalizedExecutionRequest) -> Result<Self, ExecutionError> {
        let (program, args) = command_argv(&request.command)?;
        let application = encode_nul_terminated(&program, "program")?;
        let command_line = encode_command_line(&program, &args)?;
        let cwd = encode_nul_terminated(request.cwd.as_os_str(), "working directory").map_err(
            |error| ExecutionError::InvalidWorkingDirectory {
                path: request.cwd.clone(),
                reason: error.to_string(),
            },
        )?;
        let environment = encode_environment(&request.env)?;
        Ok(Self {
            application,
            command_line,
            cwd,
            environment,
        })
    }
}

fn command_argv(spec: &CommandSpec) -> Result<(OsString, Vec<OsString>), ExecutionError> {
    match spec {
        CommandSpec::Program { program, args } => Ok((program.clone(), args.clone())),
        CommandSpec::Shell {
            shell: ShellKind::PowerShell,
            script,
        } => Ok((
            powershell_executable()?,
            vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-Command"),
                OsString::from(powershell_payload(script)),
            ],
        )),
        CommandSpec::Shell {
            shell: ShellKind::PowerShellLiteral,
            script,
        } => Ok((
            powershell_executable()?,
            vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-Command"),
                OsString::from(script),
            ],
        )),
        CommandSpec::Shell {
            shell: ShellKind::Posix,
            ..
        } => Err(invalid_command(
            "POSIX shell text cannot be reinterpreted as PowerShell on Windows",
        )),
    }
}

fn powershell_executable() -> Result<OsString, ExecutionError> {
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: `buffer` is writable for its declared length and the API writes a
    // NUL-terminated Windows directory path or returns zero on failure.
    let length = unsafe {
        GetWindowsDirectoryW(
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).expect("Windows directory buffer fits in u32"),
        )
    };
    if length == 0 {
        return Err(spawn_failed(io::Error::last_os_error()));
    }
    let length = usize::try_from(length)
        .map_err(|_| invalid_command("Windows directory length did not fit usize"))?;
    if length >= buffer.len() {
        return Err(invalid_command(
            "Windows directory exceeded the fixed command-resolution buffer",
        ));
    }
    buffer.truncate(length);
    let executable = std::path::PathBuf::from(OsString::from_wide(&buffer))
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !executable.is_file() {
        return Err(ExecutionError::SpawnFailed {
            failure: SpawnFailure {
                code: "powershell_unavailable".to_owned(),
                message: format!(
                    "trusted Windows PowerShell executable is unavailable: {}",
                    executable.display()
                ),
            },
        });
    }
    Ok(executable.into_os_string())
}

fn powershell_payload(script: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)\n\
         [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n\
         $OutputEncoding = [Console]::OutputEncoding\n\
         $PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'\n\
         $global:LASTEXITCODE = $null\n\
         try {{\n\
         & {{\n\
         {script}\n\
         $nomifunSucceeded = $?\n\
         $nomifunLastExitCode = $global:LASTEXITCODE\n\
         if ($null -ne $nomifunLastExitCode -and -not $nomifunSucceeded) {{ exit $nomifunLastExitCode }}\n\
         if (-not $nomifunSucceeded) {{ exit 1 }}\n\
         }}\n\
         $commandSucceeded = $?\n\
         if (-not $commandSucceeded) {{\n\
           if ($null -ne $global:LASTEXITCODE) {{ exit $global:LASTEXITCODE }}\n\
           exit 1\n\
         }}\n\
         exit 0\n\
         }} catch {{\n\
         [Console]::Error.WriteLine($_.Exception.Message)\n\
         exit 1\n\
         }}"
    )
}

fn encode_command_line(program: &OsStr, args: &[OsString]) -> Result<Vec<u16>, ExecutionError> {
    let mut command_line = Vec::new();
    append_quoted(program, &mut command_line)?;
    for arg in args {
        command_line.push(b' ' as u16);
        append_quoted(arg, &mut command_line)?;
    }
    command_line.push(0);
    if command_line.len() > MAX_COMMAND_LINE_UNITS {
        return Err(invalid_command(
            "Windows command line exceeds 32,767 UTF-16 code units",
        ));
    }
    Ok(command_line)
}

fn append_quoted(arg: &OsStr, command_line: &mut Vec<u16>) -> Result<(), ExecutionError> {
    let encoded = arg.encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(invalid_command(
            "Windows command-line arguments cannot contain NUL",
        ));
    }
    let requires_quotes = encoded.is_empty()
        || encoded.iter().any(|unit| {
            matches!(
                *unit,
                value if value == b' ' as u16
                    || value == b'\t' as u16
                    || value == b'\n' as u16
                    || value == 0x0B
                    || value == b'"' as u16
            )
        });
    if !requires_quotes {
        command_line.extend_from_slice(&encoded);
        return Ok(());
    }

    command_line.push(b'"' as u16);
    let mut index = 0;
    while index < encoded.len() {
        let mut backslashes = 0_usize;
        while index < encoded.len() && encoded[index] == b'\\' as u16 {
            backslashes += 1;
            index += 1;
        }
        if index == encoded.len() {
            command_line.extend(std::iter::repeat_n(
                b'\\' as u16,
                backslashes.saturating_mul(2),
            ));
            break;
        }
        if encoded[index] == b'"' as u16 {
            command_line.extend(std::iter::repeat_n(
                b'\\' as u16,
                backslashes.saturating_mul(2).saturating_add(1),
            ));
        } else {
            command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        }
        command_line.push(encoded[index]);
        index += 1;
    }
    command_line.push(b'"' as u16);
    Ok(())
}

struct EnvironmentEntry {
    key: OsString,
    value: OsString,
}

fn encode_environment(
    overrides: &std::collections::BTreeMap<OsString, OsString>,
) -> Result<Vec<u16>, ExecutionError> {
    encode_environment_from(std::env::vars_os(), overrides)
}

fn encode_environment_from(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    overrides: &std::collections::BTreeMap<OsString, OsString>,
) -> Result<Vec<u16>, ExecutionError> {
    let mut entries = Vec::<EnvironmentEntry>::new();
    for (key, value) in inherited {
        if dangerous_inherited_environment(&key) {
            continue;
        }
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| compare_os_case_insensitive(&entry.key, &key) == Ordering::Equal)
        {
            entry.key = key;
            entry.value = value;
        } else {
            entries.push(EnvironmentEntry { key, value });
        }
    }

    for (key, value) in overrides {
        validate_environment_override(key, value)?;
        if dangerous_inherited_environment(key) {
            return Err(invalid_command(format!(
                "environment override {key:?} is forbidden at execution boundary"
            )));
        }
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| compare_os_case_insensitive(&entry.key, key) == Ordering::Equal)
        {
            entry.key = key.clone();
            entry.value = value.clone();
        } else {
            entries.push(EnvironmentEntry {
                key: key.clone(),
                value: value.clone(),
            });
        }
    }

    entries.sort_by(|left, right| {
        compare_os_case_insensitive(&left.key, &right.key)
            .then_with(|| compare_os_raw(&left.key, &right.key))
    });
    let mut block = Vec::new();
    for entry in entries {
        append_environment_component(&entry.key, "environment key", &mut block)?;
        block.push(b'=' as u16);
        append_environment_component(&entry.value, "environment value", &mut block)?;
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

fn dangerous_inherited_environment(key: &OsStr) -> bool {
    [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "NODE_OPTIONS",
        "NODE_INSPECT",
        "NODE_DEBUG",
        "CLAUDECODE",
    ]
    .iter()
    .any(|candidate| {
        compare_os_case_insensitive(key, OsStr::new(candidate)) == Ordering::Equal
    })
}

fn validate_environment_override(key: &OsStr, value: &OsStr) -> Result<(), ExecutionError> {
    let encoded_key = key.encode_wide().collect::<Vec<_>>();
    if encoded_key.is_empty() {
        return Err(invalid_command("environment keys cannot be empty"));
    }
    if encoded_key
        .iter()
        .any(|unit| *unit == 0 || *unit == b'=' as u16)
    {
        return Err(invalid_command(
            "environment keys cannot contain NUL or '='",
        ));
    }
    if value.encode_wide().any(|unit| unit == 0) {
        return Err(invalid_command(
            "environment values cannot contain NUL",
        ));
    }
    Ok(())
}

fn append_environment_component(
    value: &OsStr,
    label: &'static str,
    destination: &mut Vec<u16>,
) -> Result<(), ExecutionError> {
    for unit in value.encode_wide() {
        if unit == 0 {
            return Err(invalid_command(format!("{label} cannot contain NUL")));
        }
        destination.push(unit);
    }
    Ok(())
}

fn compare_os_case_insensitive(left: &OsStr, right: &OsStr) -> Ordering {
    let left = left.encode_wide().collect::<Vec<_>>();
    let right = right.encode_wide().collect::<Vec<_>>();
    let Some(left_len) = i32::try_from(left.len()).ok() else {
        return left.cmp(&right);
    };
    let Some(right_len) = i32::try_from(right.len()).ok() else {
        return left.cmp(&right);
    };
    // SAFETY: the explicit lengths bound both readable UTF-16 buffers.
    match unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left_len,
            right.as_ptr(),
            right_len,
            1,
        )
    } {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

fn compare_os_raw(left: &OsStr, right: &OsStr) -> Ordering {
    left.encode_wide()
        .collect::<Vec<_>>()
        .cmp(&right.encode_wide().collect::<Vec<_>>())
}

fn encode_nul_terminated(value: &OsStr, label: &'static str) -> Result<Vec<u16>, ExecutionError> {
    let mut encoded = value.encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(invalid_command(format!("{label} cannot contain NUL")));
    }
    encoded.push(0);
    Ok(encoded)
}

fn enforce_sandbox(request: &NormalizedExecutionRequest) -> Result<(), ExecutionError> {
    match &request.capability.sandbox {
        SandboxPolicy::UnrestrictedLocalOwner => Ok(()),
        SandboxPolicy::DenyExecution => Err(ExecutionError::CapabilityDenied {
            path: request.cwd.clone(),
            reason: "execution is denied by the sandbox policy".to_owned(),
        }),
        SandboxPolicy::MacSeatbelt { .. } => Err(ExecutionError::CapabilityDenied {
            path: request.cwd.clone(),
            reason: "macOS Seatbelt cannot authorize Windows execution".to_owned(),
        }),
    }
}

fn invalid_command(reason: impl Into<String>) -> ExecutionError {
    ExecutionError::InvalidCommand {
        reason: reason.into(),
    }
}

fn spawn_failed(error: io::Error) -> ExecutionError {
    ExecutionError::SpawnFailed {
        failure: SpawnFailure {
            code: "spawn_failed".to_owned(),
            message: error.to_string(),
        },
    }
}

fn start_lost(
    code: &'static str,
    message: String,
    last_known: Option<ProcessSnapshot>,
    cleanup: CleanupReport,
) -> ExecutionError {
    ExecutionError::StartLost {
        failure: SpawnFailure {
            code: code.to_owned(),
            message,
        },
        last_known,
        cleanup,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::{OsStr, OsString},
        os::windows::ffi::OsStringExt,
        sync::Mutex,
        time::Duration,
    };

    use serial_test::serial;
    use tempfile::TempDir;
    use windows_sys::Win32::Foundation::HANDLE;

    use super::*;
    use crate::{CapabilityPolicy, ExecutionOwner, ExecutionPolicy, Transport};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SpawnAuditEvent {
        Created,
        Assigned,
        Resumed,
    }

    struct AuditFacade {
        events: Mutex<Vec<SpawnAuditEvent>>,
        fail_assignment: bool,
        created: Mutex<Option<CreatedProcess>>,
    }

    struct CreatedProcess {
        pid: u32,
        process: OwnedHandle,
    }

    impl AuditFacade {
        fn assignment_failure() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                fail_assignment: true,
                created: Mutex::new(None),
            }
        }

        fn successful() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                fail_assignment: false,
                created: Mutex::new(None),
            }
        }

        fn events(&self) -> Vec<SpawnAuditEvent> {
            self.events
                .lock()
                .expect("spawn audit event mutex should not be poisoned")
                .clone()
        }

        fn created_pid(&self) -> Option<u32> {
            self.created
                .lock()
                .expect("created process mutex should not be poisoned")
                .as_ref()
                .map(|created| created.pid)
        }

    fn created_process_is_signaled(&self) -> io::Result<bool> {
            let created = self
                .created
                .lock()
                .map_err(|_| io::Error::other("created process mutex is poisoned"))?;
            let created = created
                .as_ref()
                .ok_or_else(|| io::Error::other("CreateProcessW was not observed"))?;
            handle_is_signaled(created.process.as_raw())
        }

        fn wait_created_process_terminated(&self) -> io::Result<()> {
            let created = self
                .created
                .lock()
                .map_err(|_| io::Error::other("created process mutex is poisoned"))?;
            let created = created
                .as_ref()
                .ok_or_else(|| io::Error::other("CreateProcessW was not observed"))?;
            wait_handle_until(
                created.process.as_raw(),
                Instant::now() + Duration::from_secs(5),
            )
        }
    }

    impl Win32Facade for AuditFacade {
        fn process_created(
            &self,
            pid: u32,
            process: HANDLE,
            _thread: HANDLE,
        ) -> io::Result<()> {
            let process = duplicate_non_inheritable(process)?;
            self.events
                .lock()
                .map_err(|_| io::Error::other("spawn audit event mutex is poisoned"))?
                .push(SpawnAuditEvent::Created);
            *self
                .created
                .lock()
                .map_err(|_| io::Error::other("created process mutex is poisoned"))? =
                Some(CreatedProcess { pid, process });
            Ok(())
        }

        fn assign_process_to_job(&self, job: HANDLE, process: HANDLE) -> io::Result<()> {
            self.events
                .lock()
                .map_err(|_| io::Error::other("spawn audit event mutex is poisoned"))?
                .push(SpawnAuditEvent::Assigned);
            if self.fail_assignment {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected AssignProcessToJobObject failure",
                ))
            } else {
                SystemWin32.assign_process_to_job(job, process)
            }
        }

        fn resume_thread(&self, thread: HANDLE) -> io::Result<u32> {
            self.events
                .lock()
                .map_err(|_| io::Error::other("spawn audit event mutex is poisoned"))?
                .push(SpawnAuditEvent::Resumed);
            SystemWin32.resume_thread(thread)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn assignment_failure_never_resumes_or_executes_user_code() {
        let temporary = TempDir::new().expect("temporary marker directory should be created");
        let marker = temporary.path().join("must-not-exist.marker");
        let facade = Arc::new(AuditFacade::assignment_failure());
        let request = program_request(
            command_shell(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from(format!(">\"{}\" echo resumed", marker.display())),
            ],
        );
        let output = Arc::new(OutputBuffer::new(4096));

        let result = spawn_pipe_inner(request, output, facade.clone()).await;

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("injected Job assignment failure must reject start"),
        };
        let ExecutionError::SpawnFailed { failure } = error else {
            panic!("pre-resume assignment failure must be stable SpawnFailed, got {error:?}");
        };
        assert_eq!(failure.code, "spawn_failed");
        assert!(
            failure
                .message
                .contains("injected AssignProcessToJobObject failure"),
            "unexpected spawn failure message: {}",
            failure.message
        );
        assert_eq!(
            facade.events(),
            vec![SpawnAuditEvent::Created, SpawnAuditEvent::Assigned],
            "ResumeThread must never be attempted after assignment failure"
        );
        assert!(
            !marker.exists(),
            "the suspended child executed user code despite assignment failure"
        );
        assert!(
            facade
                .created_process_is_signaled()
                .expect("exact process handle liveness probe should succeed"),
            "the suspended child was not reaped before SpawnFailed returned"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn conpty_assignment_failure_never_resumes_or_executes_user_code() {
        let temporary = TempDir::new().expect("temporary marker directory should be created");
        let marker = temporary.path().join("conpty-must-not-exist.marker");
        let facade = Arc::new(AuditFacade::assignment_failure());
        let request = program_request(
            command_shell(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from(format!(">\"{}\" echo resumed", marker.display())),
            ],
        );

        let result = spawn_pty_inner(
            request,
            Arc::new(OutputBuffer::new(4096)),
            facade.clone(),
            80,
            24,
        )
        .await;

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("injected ConPTY Job assignment failure must reject start"),
        };
        assert!(matches!(error, ExecutionError::SpawnFailed { .. }));
        assert_eq!(
            facade.events(),
            vec![SpawnAuditEvent::Created, SpawnAuditEvent::Assigned]
        );
        assert!(!marker.exists(), "suspended ConPTY child executed user code");
        assert!(
            facade
                .created_process_is_signaled()
                .expect("exact ConPTY process liveness probe should succeed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn spawn_order_is_create_then_assign_then_resume() {
        let facade = Arc::new(AuditFacade::successful());
        let request = program_request(
            command_shell(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from("exit 0"),
            ],
        );
        let output = Arc::new(OutputBuffer::new(4096));

        let spawned = spawn_pipe_inner(request, output, facade.clone())
            .await
            .expect("audited helper should start");
        let fact = spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(5))
            .await
            .expect("audited helper should be reaped");

        assert_eq!(fact.code, Some(0));
        assert_eq!(fact.signal, None);
        assert_eq!(
            facade.events(),
            vec![
                SpawnAuditEvent::Created,
                SpawnAuditEvent::Assigned,
                SpawnAuditEvent::Resumed,
            ]
        );
        assert!(facade.created_pid().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn dropping_start_future_after_create_cannot_resume_later() {
        let temporary = TempDir::new().expect("temporary marker directory should be created");
        let marker = temporary.path().join("drop-must-not-resume.marker");
        let facade = Arc::new(PausedAssignFacade::new());
        let request = program_request(
            command_shell(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from(format!(">\"{}\" echo resumed", marker.display())),
            ],
        );
        let output = Arc::new(OutputBuffer::new(4096));
        let start = tokio::spawn(spawn_pipe_inner(request, output, facade.clone()));

        tokio::time::timeout(Duration::from_secs(5), facade.wait_until_assign_entered())
            .await
            .expect("spawn transaction should reach the injected assignment pause");
        start.abort();
        let _ = start.await;
        facade.release_assignment();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if facade.events().contains(&SpawnAuditEvent::Assigned) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("cancelled blocking transaction should finish assignment cleanup");

        assert_eq!(
            facade.events(),
            vec![SpawnAuditEvent::Created, SpawnAuditEvent::Assigned],
            "a dropped start future permitted a late ResumeThread"
        );
        assert!(!marker.exists(), "dropped start executed user code");
        facade
            .wait_created_process_terminated()
            .expect("cancelled start must leave no exact process behind");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn dropping_conpty_start_after_create_cannot_resume_later() {
        let temporary = TempDir::new().expect("temporary marker directory should be created");
        let marker = temporary.path().join("drop-conpty-must-not-resume.marker");
        let facade = Arc::new(PausedAssignFacade::new());
        let request = program_request(
            command_shell(),
            &[
                OsString::from("/D"),
                OsString::from("/C"),
                OsString::from(format!(">\"{}\" echo resumed", marker.display())),
            ],
        );
        let start = tokio::spawn(spawn_pty_inner(
            request,
            Arc::new(OutputBuffer::new(4096)),
            facade.clone(),
            80,
            24,
        ));

        tokio::time::timeout(Duration::from_secs(5), facade.wait_until_assign_entered())
            .await
            .expect("ConPTY transaction should reach the assignment pause");
        start.abort();
        let _ = start.await;
        facade.release_assignment();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if facade.events().contains(&SpawnAuditEvent::Assigned) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("cancelled ConPTY transaction should finish cleanup");

        assert_eq!(
            facade.events(),
            vec![SpawnAuditEvent::Created, SpawnAuditEvent::Assigned]
        );
        assert!(!marker.exists(), "dropped ConPTY start executed user code");
        facade
            .wait_created_process_terminated()
            .expect("cancelled ConPTY start must leave no exact process behind");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn dropped_write_keeps_stdin_open_until_the_blocking_write_finishes() {
        let spawned = spawn_pipe_inner(
            program_request(
                command_shell(),
                &[
                    OsString::from("/D"),
                    OsString::from("/C"),
                    OsString::from("ping -n 10 127.0.0.1 >NUL"),
                ],
            ),
            Arc::new(OutputBuffer::new(4096)),
            Arc::new(SystemWin32),
        )
        .await
        .expect("stdin sink should start");
        let owner = spawned.owner.clone();
        let write_owner = owner.clone();
        let write = tokio::spawn(async move {
            write_owner
                .write(&vec![b'x'; 32 * 1024 * 1024])
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        write.abort();
        let _ = write.await;
        let close_owner = owner.clone();
        let close = tokio::spawn(async move { close_owner.close_stdin().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), close)
                .await
                .is_err(),
            "close_stdin returned while the cancelled blocking write still owned stdin"
        );
        owner
            .force_kill()
            .await
            .expect("stdin sink Job should terminate");
        owner
            .wait_reaped(Instant::now() + Duration::from_secs(5))
            .await
            .expect("stdin sink should be reaped after dropped write");
        owner
            .close_stdin()
            .await
            .expect("stdin should close after the detached writer has finished");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn child_inherits_only_the_three_whitelisted_stdio_handles() {
        let sentinel = create_inheritable_pipe().expect("sentinel pipe should be created");
        clear_inheritance(sentinel.read.as_raw())
            .expect("sentinel parent read handle should not be inheritable");
        let spawned = spawn_pipe_inner(
            program_request(
                command_shell(),
                &[
                    OsString::from("/D"),
                    OsString::from("/C"),
                    OsString::from("ping -n 10 127.0.0.1 >NUL"),
                ],
            ),
            Arc::new(OutputBuffer::new(4096)),
            Arc::new(SystemWin32),
        )
        .await
        .expect("sentinel probe child should start");
        drop(sentinel.write);

        let eof = tokio::task::spawn_blocking(move || {
            let mut byte = 0_u8;
            let mut read = 0_u32;
            // SAFETY: byte is writable and the sentinel read handle remains
            // owned by this blocking task.
            let result = unsafe {
                ReadFile(
                    sentinel.read.as_raw(),
                    &mut byte,
                    1,
                    &mut read,
                    ptr::null_mut(),
                )
            };
            if result != 0 {
                return Ok(read == 0);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error().map(|code| code as u32) == Some(ERROR_BROKEN_PIPE) {
                Ok(true)
            } else {
                Err(error)
            }
        });

        let inherited = tokio::time::timeout(Duration::from_millis(250), eof)
            .await
            .map(|result| {
                result
                    .expect("sentinel reader task should not panic")
                    .expect("sentinel reader should observe EOF")
            })
            .unwrap_or(false);
        spawned
            .owner
            .force_kill()
            .await
            .expect("sentinel probe Job should terminate");
        spawned
            .owner
            .wait_reaped(Instant::now() + Duration::from_secs(5))
            .await
            .expect("sentinel probe child should be reaped");

        assert!(
            inherited,
            "an unrelated inheritable host handle reached the child despite HANDLE_LIST"
        );
    }

    #[test]
    fn crt_quoting_handles_empty_spaces_quotes_and_trailing_backslashes() {
        assert_eq!(quoted("plain"), "plain");
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted("two words"), r#""two words""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quoted(r#"ends with \"#), r#""ends with \\""#);
        assert_eq!(
            decode_wide(&encode_command_line(
                OsStr::new(r"C:\Program Files\helper.exe"),
                &[OsString::from(r#"a\"b"#), OsString::from("")],
            )
            .expect("valid command line should encode")),
            r#""C:\Program Files\helper.exe" "a\\\"b" """#
        );
    }

    #[test]
    fn literal_powershell_kind_does_not_rewrite_script_source() {
        let script = "param($Name)\n#requires -Version 5\nWrite-Output $Name";
        let (_, args) = command_argv(&CommandSpec::Shell {
            shell: ShellKind::PowerShellLiteral,
            script: script.to_owned(),
        })
        .expect("trusted PowerShell should resolve");

        assert_eq!(args.last(), Some(&OsString::from(script)));
        assert!(!args.last().unwrap().to_string_lossy().contains("ErrorActionPreference"));
    }

    #[test]
    fn environment_overlay_is_case_insensitive_sorted_and_double_nul_terminated() {
        let inherited = vec![
            (OsString::from("Path"), OsString::from("old")),
            (OsString::from("path"), OsString::from("newer inherited")),
            (OsString::from("ZETA"), OsString::from("last")),
            (OsString::from("alpha"), OsString::from("first")),
        ];
        let overrides = BTreeMap::from([
            (OsString::from("PATH"), OsString::from("new")),
            (OsString::from("Beta"), OsString::from("middle")),
        ]);

        let block = encode_environment_from(inherited, &overrides)
            .expect("valid environment should encode");

        assert!(block.ends_with(&[0, 0]));
        assert_eq!(
            environment_entries(&block),
            vec!["alpha=first", "Beta=middle", "PATH=new", "ZETA=last"]
        );
    }

    #[test]
    fn environment_rejects_nul_in_override_key_or_value() {
        let nul = OsString::from_wide(&[b'A' as u16, 0, b'B' as u16]);
        let invalid_key = BTreeMap::from([(nul.clone(), OsString::from("value"))]);
        let invalid_value = BTreeMap::from([(OsString::from("KEY"), nul)]);

        assert!(matches!(
            encode_environment_from(Vec::new(), &invalid_key),
            Err(ExecutionError::InvalidCommand { reason })
                if reason.contains("environment keys cannot contain NUL")
        ));
        assert!(matches!(
            encode_environment_from(Vec::new(), &invalid_value),
            Err(ExecutionError::InvalidCommand { reason })
                if reason.contains("environment values cannot contain NUL")
        ));
    }

    #[test]
    fn environment_strips_inherited_loader_and_node_injection_variables() {
        let inherited = vec![
            (OsString::from("PATH"), OsString::from("safe")),
            (
                OsString::from("NODE_OPTIONS"),
                OsString::from("--require malicious.js"),
            ),
            (
                OsString::from("dyld_insert_libraries"),
                OsString::from("malicious.dll"),
            ),
        ];
        let block = encode_environment_from(inherited, &BTreeMap::new())
            .expect("safe inherited environment should encode");

        assert_eq!(environment_entries(&block), vec!["PATH=safe"]);
    }

    #[test]
    fn environment_rejects_case_insensitive_dangerous_overrides() {
        for key in ["NODE_OPTIONS", "node_options", "Ld_PrElOaD"] {
            let overrides = BTreeMap::from([(
                OsString::from(key),
                OsString::from("malicious"),
            )]);
            assert!(matches!(
                encode_environment_from(Vec::new(), &overrides),
                Err(ExecutionError::InvalidCommand { reason })
                    if reason.contains("forbidden at execution boundary")
            ));
        }
    }

    fn program_request(program: OsString, args: &[OsString]) -> NormalizedExecutionRequest {
        let cwd = std::env::current_dir().expect("current directory should exist");
        NormalizedExecutionRequest {
            owner: ExecutionOwner::new(uuid::Uuid::now_v7(), uuid::Uuid::now_v7()),
            command: CommandSpec::Program {
                program,
                args: args.to_vec(),
            },
            cwd: cwd.clone(),
            env: BTreeMap::new(),
            transport: Transport::Pipe,
            policy: ExecutionPolicy::default(),
            capability: CapabilityPolicy::local_owner(cwd),
        }
    }

    fn command_shell() -> OsString {
        let system_root =
            std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        std::path::PathBuf::from(system_root)
            .join("System32")
            .join("cmd.exe")
            .into_os_string()
    }

    fn quoted(value: &str) -> String {
        let mut encoded = Vec::new();
        append_quoted(OsStr::new(value), &mut encoded).expect("valid argument should quote");
        String::from_utf16(&encoded).expect("ASCII fixture should remain valid UTF-16")
    }

    fn decode_wide(encoded: &[u16]) -> String {
        let encoded = encoded.strip_suffix(&[0]).unwrap_or(encoded);
        String::from_utf16(encoded).expect("ASCII fixture should remain valid UTF-16")
    }

    fn environment_entries(block: &[u16]) -> Vec<String> {
        block
            .split(|unit| *unit == 0)
            .take_while(|entry| !entry.is_empty())
            .map(|entry| String::from_utf16(entry).expect("ASCII fixture should remain UTF-16"))
            .collect()
    }

    struct PausedAssignFacade {
        audit: AuditFacade,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, std::sync::Condvar),
    }

    impl PausedAssignFacade {
        fn new() -> Self {
            Self {
                audit: AuditFacade::successful(),
                entered: tokio::sync::Notify::new(),
                release: (Mutex::new(false), std::sync::Condvar::new()),
            }
        }

        async fn wait_until_assign_entered(&self) {
            self.entered.notified().await;
        }

        fn release_assignment(&self) {
            let (released, condition) = &self.release;
            let mut released = match released.lock() {
                Ok(released) => released,
                Err(poisoned) => poisoned.into_inner(),
            };
            *released = true;
            condition.notify_all();
        }

        fn events(&self) -> Vec<SpawnAuditEvent> {
            self.audit.events()
        }

        fn wait_created_process_terminated(&self) -> io::Result<()> {
            self.audit.wait_created_process_terminated()
        }
    }

    impl Win32Facade for PausedAssignFacade {
        fn process_created(
            &self,
            pid: u32,
            process: HANDLE,
            thread: HANDLE,
        ) -> io::Result<()> {
            self.audit.process_created(pid, process, thread)
        }

        fn assign_process_to_job(&self, _job: HANDLE, _process: HANDLE) -> io::Result<()> {
            self.entered.notify_one();
            let (released, condition) = &self.release;
            let mut released = released
                .lock()
                .map_err(|_| io::Error::other("assignment pause mutex is poisoned"))?;
            while !*released {
                released = condition
                    .wait(released)
                    .map_err(|_| io::Error::other("assignment pause mutex is poisoned"))?;
            }
            self.audit
                .events
                .lock()
                .map_err(|_| io::Error::other("spawn audit event mutex is poisoned"))?
                .push(SpawnAuditEvent::Assigned);
            Ok(())
        }

        fn resume_thread(&self, thread: HANDLE) -> io::Result<u32> {
            self.audit.resume_thread(thread)
        }
    }
}
