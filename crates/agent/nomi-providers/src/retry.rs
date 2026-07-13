use std::future::Future;
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::Value;

use super::ProviderError;
use super::anthropic_shared::StreamOutcome;

pub const MAX_STREAM_RETRIES: u32 = 2;
pub const MAX_INITIAL_REQUEST_RETRIES: u32 = 2;
const MAX_BACKOFF: Duration = Duration::from_secs(15);
const INITIAL_REQUEST_BACKOFF: Duration = Duration::from_millis(300);
const MAX_INITIAL_REQUEST_BACKOFF: Duration = Duration::from_secs(2);

/// Retry bounded, side-effect-free initial request failures: connection
/// failures and transient gateway/service 500/502/503/504 responses. Client
/// errors and rate limits are surfaced immediately.
pub async fn with_initial_request_retry<F, Fut, T>(f: F) -> Result<T, ProviderError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut backoff = INITIAL_REQUEST_BACKOFF;
    for attempt in 0..=MAX_INITIAL_REQUEST_RETRIES {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) if is_retryable_initial_request_error(&e) && attempt < MAX_INITIAL_REQUEST_RETRIES => {
                let (error_kind, status) = retry_log_classification(&e);
                tracing::warn!(
                    attempt = attempt + 1,
                    max_retries = MAX_INITIAL_REQUEST_RETRIES,
                    error_kind,
                    status = status.unwrap_or_default(),
                    "retrying transient initial provider request failure"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_INITIAL_REQUEST_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn retry_log_classification(error: &ProviderError) -> (&'static str, Option<u16>) {
    match error {
        ProviderError::Http(_) => ("http_connect", None),
        ProviderError::Connection(_) => ("connection", None),
        ProviderError::Api { status, .. } => ("transient_api", Some(*status)),
        _ => ("other", None),
    }
}

fn is_retryable_initial_request_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Http(err) => err.is_connect(),
        ProviderError::Connection(_) => true,
        ProviderError::Api { status, .. } => matches!(status, 500 | 502 | 503 | 504),
        _ => false,
    }
}

/// Send an HTTP request and check status, returning the response on success.
/// Used by provider-specific retry loops to avoid duplicating request logic.
pub async fn send_and_check(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<reqwest::Response, ProviderError> {
    let response = client
        .post(url)
        .headers(headers.clone())
        .json(body)
        .send()
        .await
        .map_err(|e| ProviderError::Connection(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        return Err(ProviderError::Api {
            status: status.as_u16(),
            message: body_text,
        });
    }

    Ok(response)
}

/// Sleep with exponential backoff and log the retry attempt.
/// Returns the next backoff duration.
pub async fn backoff_sleep(attempt: u32, current_backoff: Duration) -> Duration {
    tracing::warn!(
        attempt,
        max = MAX_STREAM_RETRIES,
        "retrying stream after mid-stream disconnect"
    );
    tokio::time::sleep(current_backoff).await;
    (current_backoff * 2).min(MAX_BACKOFF)
}

/// Evaluate a `StreamOutcome` within a retry loop. Returns:
/// - `Ok(None)` — stream succeeded, stop retrying
/// - `Ok(Some(err))` — non-retryable failure, caller should emit error
/// - `Err(err)` — retryable failure, caller should continue loop
pub fn evaluate_outcome(
    outcome: StreamOutcome,
    attempt: u32,
) -> Result<Option<ProviderError>, ProviderError> {
    match outcome {
        StreamOutcome::Ok => Ok(None),
        StreamOutcome::FailedPartial(e) => Ok(Some(e)),
        StreamOutcome::FailedEmpty(e) => {
            if attempt == MAX_STREAM_RETRIES {
                Ok(Some(e))
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::ProviderError;

    #[tokio::test]
    async fn test_initial_connect_retry_succeeds_after_connection_failures() {
        tokio::time::pause();

        let counter = Arc::new(AtomicU32::new(0));
        let result = with_initial_request_retry(|| {
            let counter = Arc::clone(&counter);
            async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(ProviderError::Connection("connection refused".into()))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_initial_connect_retry_does_not_retry_rate_limit() {
        let counter = Arc::new(AtomicU32::new(0));
        let result = with_initial_request_retry(|| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(ProviderError::RateLimited {
                    retry_after_ms: 5000,
                    message: "Too Many Requests".into(),
                })
            }
        })
        .await;

        assert!(matches!(
            result.unwrap_err(),
            ProviderError::RateLimited {
                retry_after_ms: 5000,
                ..
            }
        ));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_initial_request_retries_transient_502_then_succeeds() {
        tokio::time::pause();
        let counter = Arc::new(AtomicU32::new(0));
        let result = with_initial_request_retry(|| {
            let counter = Arc::clone(&counter);
            async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(ProviderError::Api {
                        status: 502,
                        message: "bad gateway".into(),
                    })
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_initial_request_does_not_retry_400() {
        let counter = Arc::new(AtomicU32::new(0));
        let result = with_initial_request_retry(|| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(ProviderError::Api {
                    status: 400,
                    message: "bad request".into(),
                })
            }
        })
        .await;

        assert!(matches!(result, Err(ProviderError::Api { status: 400, .. })));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    // --- evaluate_outcome tests ---

    #[test]
    fn test_evaluate_outcome_ok_stops_retry() {
        let result = evaluate_outcome(StreamOutcome::Ok, 1);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn test_evaluate_outcome_failed_partial_always_stops() {
        let err = ProviderError::Connection("disconnect".into());
        let result = evaluate_outcome(StreamOutcome::FailedPartial(err), 1);
        // FailedPartial means content was already emitted — cannot retry regardless of attempt
        let Ok(Some(e)) = result else {
            panic!("expected Ok(Some(err))")
        };
        assert!(matches!(e, ProviderError::Connection(_)));
    }

    #[test]
    fn test_evaluate_outcome_failed_partial_on_last_attempt() {
        let err = ProviderError::Connection("disconnect".into());
        let result = evaluate_outcome(StreamOutcome::FailedPartial(err), MAX_STREAM_RETRIES);
        let Ok(Some(_)) = result else {
            panic!("expected Ok(Some(err))")
        };
    }

    #[test]
    fn test_evaluate_outcome_failed_empty_retries_when_not_exhausted() {
        let err = ProviderError::Connection("disconnect".into());
        // attempt 1 < MAX_STREAM_RETRIES(2), should signal "continue retrying"
        let result = evaluate_outcome(StreamOutcome::FailedEmpty(err), 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_evaluate_outcome_failed_empty_stops_on_last_attempt() {
        let err = ProviderError::Connection("disconnect".into());
        // attempt == MAX_STREAM_RETRIES, should stop and return error
        let result = evaluate_outcome(StreamOutcome::FailedEmpty(err), MAX_STREAM_RETRIES);
        let Ok(Some(e)) = result else {
            panic!("expected Ok(Some(err))")
        };
        assert!(matches!(e, ProviderError::Connection(_)));
    }

    // --- backoff_sleep tests ---

    #[tokio::test]
    async fn test_backoff_sleep_doubles_duration() {
        tokio::time::pause();

        let next = backoff_sleep(1, Duration::from_secs(1)).await;
        assert_eq!(next, Duration::from_secs(2));

        let next = backoff_sleep(2, Duration::from_secs(4)).await;
        assert_eq!(next, Duration::from_secs(8));
    }

    #[tokio::test]
    async fn test_backoff_sleep_caps_at_max() {
        tokio::time::pause();

        // 10s * 2 = 20s, but MAX_BACKOFF is 15s
        let next = backoff_sleep(1, Duration::from_secs(10)).await;
        assert_eq!(next, Duration::from_secs(15));

        // Already at max
        let next = backoff_sleep(2, Duration::from_secs(15)).await;
        assert_eq!(next, Duration::from_secs(15));
    }
}
