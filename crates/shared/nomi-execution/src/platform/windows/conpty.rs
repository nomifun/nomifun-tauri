use std::{
    io,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

use windows_sys::{
    Win32::{
        System::Console::{
            COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
        },
    },
    core::HRESULT,
};

use super::handles::OwnedHandle;

pub(super) const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
static CLOSE_RELAY: OnceLock<mpsc::Sender<CloseJob>> = OnceLock::new();
static CLOSE_RELAY_INIT: Mutex<()> = Mutex::new(());

struct CloseJob {
    handle: HPCON,
    action: CloseAction,
    completed: mpsc::SyncSender<()>,
}

#[derive(Clone)]
enum CloseAction {
    System,
    #[cfg(test)]
    Test(Arc<dyn Fn(HPCON) + Send + Sync>),
}

impl CloseAction {
    fn run(&self, handle: HPCON) {
        match self {
            Self::System => unsafe { ClosePseudoConsole(handle) },
            #[cfg(test)]
            Self::Test(close) => close(handle),
        }
    }
}

pub(super) struct PreparedConPty {
    pub(super) control: Arc<PseudoConsoleControl>,
    pub(super) input: OwnedHandle,
    pub(super) output: OwnedHandle,
}

impl PreparedConPty {
    pub(super) fn create(
        cols: u16,
        rows: u16,
        create_pipe: impl Fn() -> io::Result<(OwnedHandle, OwnedHandle)>,
    ) -> io::Result<Self> {
        // Establish durable close ownership before an HPCON can exist. The
        // relay is process-lifetime and catches panics so its receiver never
        // disappears while a pseudoconsole is live.
        close_relay_sender()?;
        let size = checked_coord(cols, rows)?;
        let (input_read, input_write) = create_pipe()?;
        let (output_read, output_write) = create_pipe()?;
        let mut pseudoconsole = 0;
        let flags = 0x2 | 0x4;
        let result = unsafe {
            CreatePseudoConsole(
                size,
                input_read.as_raw(),
                output_write.as_raw(),
                flags,
                &mut pseudoconsole,
            )
        };
        hresult(result, "CreatePseudoConsole")?;

        let control = Arc::new(PseudoConsoleControl::new(pseudoconsole));
        drop(input_read);
        drop(output_write);
        Ok(Self {
            control,
            input: input_write,
            output: output_read,
        })
    }

    pub(super) fn into_parts(self) -> (Arc<PseudoConsoleControl>, OwnedHandle, OwnedHandle) {
        (self.control, self.input, self.output)
    }
}

pub(super) struct PseudoConsoleControl {
    state: Mutex<PseudoConsoleState>,
    close_action: CloseAction,
}

struct PseudoConsoleState {
    handle: Option<HPCON>,
    closing: Option<Arc<CloseCompletion>>,
}

struct CloseCompletion {
    receiver: Mutex<mpsc::Receiver<()>>,
}

impl PseudoConsoleControl {
    fn new(handle: HPCON) -> Self {
        Self {
            state: Mutex::new(PseudoConsoleState {
                handle: Some(handle),
                closing: None,
            }),
            close_action: CloseAction::System,
        }
    }

    #[cfg(test)]
    fn new_with_close(
        handle: HPCON,
        close: Arc<dyn Fn(HPCON) + Send + Sync>,
    ) -> io::Result<Self> {
        close_relay_sender()?;
        Ok(Self {
            state: Mutex::new(PseudoConsoleState {
                handle: Some(handle),
                closing: None,
            }),
            close_action: CloseAction::Test(close),
        })
    }

    pub(super) fn raw(&self) -> io::Result<HPCON> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("pseudoconsole state is poisoned"))?
            .handle
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pseudoconsole is closing"))
    }

    pub(super) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = checked_coord(cols, rows)?;
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("pseudoconsole state is poisoned"))?;
        let handle = state
            .handle
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pseudoconsole is closed"))?;
        let result = unsafe { ResizePseudoConsole(handle, size) };
        hresult(result, "ResizePseudoConsole")
    }

    pub(super) fn begin_close(&self) -> io::Result<()> {
        let relay = close_relay_sender()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("pseudoconsole state is poisoned"))?;
        if state.closing.is_some() {
            return Ok(());
        }
        let Some(handle) = state.handle.take() else {
            return Ok(());
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        if let Err(error) = relay.send(CloseJob {
            handle,
            action: self.close_action.clone(),
            completed: sender,
        }) {
            state.handle = Some(error.0.handle);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pseudoconsole close relay is unavailable",
            ));
        }
        state.closing = Some(Arc::new(CloseCompletion {
            receiver: Mutex::new(receiver),
        }));
        Ok(())
    }

    pub(super) fn close_until(&self, deadline: Instant) -> io::Result<()> {
        self.begin_close()?;
        let closing = self
            .state
            .lock()
            .map_err(|_| io::Error::other("pseudoconsole state is poisoned"))?
            .closing
            .clone();
        let Some(closing) = closing else {
            return Ok(());
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let receiver = closing
            .receiver
            .lock()
            .map_err(|_| io::Error::other("pseudoconsole close receiver is poisoned"))?;
        match receiver.recv_timeout(remaining) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "ClosePseudoConsole did not finish within the {:?} wait budget; \
                     the background close relay retains sole ownership",
                    remaining
                ),
            )),
        }
    }
}

impl Drop for PseudoConsoleControl {
    fn drop(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(handle) = state.handle.take() else {
            return;
        };
        let relay = CLOSE_RELAY
            .get()
            .expect("close relay is initialized before an HPCON is created");
        let (completed, _receiver) = mpsc::sync_channel(1);
        if let Err(error) = relay.send(CloseJob {
            handle,
            action: self.close_action.clone(),
            completed,
        }) {
            // The process-lifetime relay receiver should never disappear; if
            // it nevertheless does, perform the dedicated close synchronously
            // rather than leak the sole HPCON ownership during final Drop.
            error.0.action.run(error.0.handle);
        }
    }
}

fn close_relay_sender() -> io::Result<mpsc::Sender<CloseJob>> {
    if let Some(sender) = CLOSE_RELAY.get() {
        return Ok(sender.clone());
    }
    let _initializing = CLOSE_RELAY_INIT
        .lock()
        .map_err(|_| io::Error::other("pseudoconsole close relay init lock is poisoned"))?;
    if let Some(sender) = CLOSE_RELAY.get() {
        return Ok(sender.clone());
    }
    let (sender, receiver) = mpsc::channel::<CloseJob>();
    std::thread::Builder::new()
        .name("nomi-conpty-close-relay".to_owned())
        .spawn(move || run_close_relay(receiver))?;
    CLOSE_RELAY
        .set(sender.clone())
        .map_err(|_| io::Error::other("pseudoconsole close relay initialization raced"))?;
    Ok(sender)
}

fn run_close_relay(receiver: mpsc::Receiver<CloseJob>) {
    while let Ok(job) = receiver.recv() {
        let job = Arc::new(job);
        loop {
            let worker_job = Arc::clone(&job);
            match std::thread::Builder::new()
                .name("nomi-conpty-close".to_owned())
                .spawn(move || {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        worker_job.action.run(worker_job.handle);
                    }));
                    let _ = worker_job.completed.send(());
                })
            {
                Ok(_) => break,
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

fn checked_coord(cols: u16, rows: u16) -> io::Result<COORD> {
    let x = i16::try_from(cols).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY columns exceed the signed 16-bit Win32 limit",
        )
    })?;
    let y = i16::try_from(rows).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY rows exceed the signed 16-bit Win32 limit",
        )
    })?;
    if x == 0 || y == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY dimensions must be non-zero",
        ));
    }
    Ok(COORD { X: x, Y: y })
}

fn hresult(result: HRESULT, operation: &'static str) -> io::Result<()> {
    if result >= 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{operation} failed with HRESULT {:#010x}",
            result as u32
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use serial_test::serial;

    use super::PseudoConsoleControl;

    #[test]
    #[serial(conpty_close_relay)]
    fn close_timeout_is_off_thread_bounded_and_single_owner() {
        let caller = std::thread::current().id();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_close = Arc::clone(&calls);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        let control = PseudoConsoleControl::new_with_close(
            1,
            Arc::new(move |_handle| {
                assert_ne!(std::thread::current().id(), caller);
                calls_for_close.fetch_add(1, Ordering::SeqCst);
                let _ = entered_tx.send(());
                let _ = release_rx
                    .lock()
                    .expect("release receiver lock")
                    .recv();
            }),
        )
        .expect("test close control should initialize");

        control.begin_close().expect("close should enqueue");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("close relay should enter the injected close");
        let started = Instant::now();
        let error = control
            .close_until(Instant::now() + Duration::from_millis(25))
            .expect_err("blocked close should time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(250));
        control.begin_close().expect("repeated close should be idempotent");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        release_tx.send(()).expect("close worker should be released");
        control
            .close_until(Instant::now() + Duration::from_secs(1))
            .expect("released close should complete");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
