//! Main-thread dispatch for macOS native APIs with AppKit / input-source
//! affinity. Non-macOS builds run the task inline.

#[cfg(target_os = "macos")]
pub(crate) type MainTask<T> = Box<dyn FnOnce() -> Result<T, String> + Send + 'static>;

#[cfg(target_os = "macos")]
pub(crate) fn run_task_with<T, D, F>(dispatch: D, task: F) -> Result<T, String>
where
    T: Send + 'static,
    D: FnOnce(MainTask<T>) -> Result<T, String>,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    dispatch(Box::new(task))
}

#[cfg(target_os = "macos")]
pub(crate) fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    run_task_with(|task| dispatch2::run_on_main(move |_mtm| task()), task)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    task()
}
