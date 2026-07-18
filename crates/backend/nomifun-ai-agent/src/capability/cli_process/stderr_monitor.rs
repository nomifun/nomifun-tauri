#[cfg(test)]
mod tests {
    use super::super::CliAgentProcess;
    use super::super::tests::simple_script_config;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn stderr_captured_in_buffer() {
        let config = simple_script_config("echo 'error line 1' >&2 && echo 'error line 2' >&2");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stderr = proc.take_stderr().await;
        assert!(stderr.contains("error line 1"), "stderr: {stderr}");
        assert!(stderr.contains("error line 2"), "stderr: {stderr}");
    }

    #[tokio::test]
    async fn take_stderr_is_consuming() {
        let config = simple_script_config("echo 'hello' >&2");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let first = proc.take_stderr().await;
        assert!(!first.is_empty());

        let second = proc.take_stderr().await;
        assert!(second.is_empty(), "Second take should be empty");
    }

    #[tokio::test]
    async fn peek_stderr_tail_returns_last_n_lines() {
        let config = simple_script_config("for i in 1 2 3 4 5; do echo \"line $i\" >&2; done");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let tail = proc.peek_stderr_tail(3).await;
        // Last three lines, in original order.
        assert!(tail.contains("line 3"), "tail: {tail}");
        assert!(tail.contains("line 4"), "tail: {tail}");
        assert!(tail.contains("line 5"), "tail: {tail}");
        assert!(!tail.contains("line 1"), "tail must drop earliest line; got {tail}");
        assert!(!tail.contains("line 2"), "tail must drop earliest line; got {tail}");
    }

    #[tokio::test]
    async fn peek_stderr_tail_does_not_drain() {
        let config = simple_script_config("echo 'first' >&2 && echo 'second' >&2");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let peek1 = proc.peek_stderr_tail(10).await;
        let peek2 = proc.peek_stderr_tail(10).await;
        assert_eq!(peek1, peek2, "peek must be idempotent");

        let drained = proc.take_stderr().await;
        assert!(drained.contains("first"));
        assert!(drained.contains("second"));
    }

    #[tokio::test]
    async fn peek_stderr_tail_zero_returns_empty() {
        let config = simple_script_config("echo 'noise' >&2");
        let proc = CliAgentProcess::spawn(config).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(proc.peek_stderr_tail(0).await, "");
    }
}
