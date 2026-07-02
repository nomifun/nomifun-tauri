use std::sync::Arc;
use std::time::{Duration, Instant};

use nomifun_api_types::{
    DetectProtocolRequest, DetectedProtocol, DetectionSuggestion, KeyTestResult, MultiKeyResult,
    ProtocolDetectionResponse, SuggestionType,
};
use nomifun_common::{AppError, ProtocolType};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::debug;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_CONCURRENT_KEY_TESTS: usize = 5;

/// Mask an API key for display in multi-key probe results: preserve the
/// prefix up to the last dash before the secret part and the last 4
/// characters, replacing the middle with `***`.
///
/// Only used for diagnostic output of the protocol-detection endpoint;
/// provider responses now return plaintext keys.
fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        return "***".to_string();
    }

    let tail_len = 4;
    let prefix_end = key
        .rmatch_indices('-')
        .find(|(i, _)| key.len() - i > tail_len)
        .map(|(i, _)| i + 1);

    match prefix_end {
        Some(pe) => {
            let suffix_start = key.len().saturating_sub(tail_len);
            let prefix = &key[..pe];
            let suffix = &key[suffix_start..];
            format!("{prefix}***{suffix}")
        }
        None => {
            let suffix_start = key.len().saturating_sub(tail_len);
            let suffix = &key[suffix_start..];
            format!("***{suffix}")
        }
    }
}

// -- Shared response structs for probing --

#[derive(serde::Deserialize)]
struct DataResponse {
    data: Vec<IdEntry>,
}

#[derive(serde::Deserialize)]
struct IdEntry {
    id: String,
}

#[derive(serde::Deserialize)]
struct GeminiResponse {
    models: Vec<NameEntry>,
}

#[derive(serde::Deserialize)]
struct NameEntry {
    name: String,
}

// -- Probe outcome --

/// Outcome of probing a single protocol endpoint.
enum ProbeOutcome {
    /// Protocol confirmed, models returned successfully.
    Success {
        models: Vec<String>,
        fixed_base_url: Option<String>,
        confidence: u8,
    },
    /// Protocol likely correct but authentication failed.
    AuthFailure { fixed_base_url: Option<String> },
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Service for detecting API endpoint protocol type.
#[derive(Clone)]
pub struct ProtocolDetectionService {
    http_client: HttpClientFactory,
}

type HttpClientFactory = Arc<dyn Fn() -> reqwest::Client + Send + Sync>;

impl ProtocolDetectionService {
    pub fn new(http_client: reqwest::Client) -> Self {
        Self {
            http_client: Arc::new(move || http_client.clone()),
        }
    }

    pub fn new_dynamic() -> Self {
        Self {
            http_client: Arc::new(nomifun_net::http_client),
        }
    }

    fn http_client(&self) -> reqwest::Client {
        (self.http_client)()
    }

    pub async fn detect_protocol(&self, req: &DetectProtocolRequest) -> Result<ProtocolDetectionResponse, AppError> {
        validate_request(req)?;

        let keys = parse_keys(&req.api_key);
        let primary_key = &keys[0];
        let timeout = Duration::from_millis(req.timeout.unwrap_or(DEFAULT_TIMEOUT_MS));
        let url_inferred = infer_from_url(&req.base_url);
        let key_inferred = infer_from_key(primary_key);
        let test_order = build_test_order(req.preferred_protocol, url_inferred, key_inferred);

        debug!(
            ?url_inferred,
            ?key_inferred,
            ?test_order,
            "Protocol detection: built test order"
        );

        // Probe each protocol in priority order, collecting all successes
        let mut auth_failure: Option<(ProtocolType, Option<String>)> = None;
        let mut successes: Vec<(ProtocolType, Vec<String>, Option<String>, u8)> = Vec::new();

        for protocol in &test_order {
            match self
                .probe_protocol(*protocol, &req.base_url, primary_key, timeout)
                .await
            {
                Ok(ProbeOutcome::Success {
                    models,
                    fixed_base_url,
                    confidence,
                }) => {
                    successes.push((*protocol, models, fixed_base_url, confidence));
                }
                Ok(ProbeOutcome::AuthFailure { fixed_base_url }) => {
                    if auth_failure.is_none() {
                        auth_failure = Some((*protocol, fixed_base_url));
                    }
                }
                Err(e) => {
                    debug!(?protocol, error = %e, "Protocol probe failed");
                }
            }
        }

        // If we have at least one success, use the first (highest priority) as primary
        if let Some((protocol, models, fixed_base_url, confidence)) = successes.first().cloned() {
            let suggestion = success_suggestion(protocol, req.preferred_protocol);
            let multi_key_result = if req.test_all_keys && keys.len() > 1 {
                let effective = fixed_base_url.as_deref().unwrap_or(&req.base_url);
                Some(self.test_all_keys(&keys, protocol, effective, timeout).await)
            } else {
                None
            };

            // Build detected_protocols from all successes (dedup by protocol, preserving order)
            let mut seen = std::collections::HashSet::new();
            let detected_protocols: Vec<DetectedProtocol> = successes
                .iter()
                .filter(|(p, _, _, _)| seen.insert(*p))
                .map(|(p, m, _, c)| DetectedProtocol {
                    protocol: *p,
                    confidence: *c,
                    models: if m.is_empty() { None } else { Some(m.clone()) },
                })
                .collect();

            return Ok(ProtocolDetectionResponse {
                protocol,
                confidence,
                success: true,
                fixed_base_url,
                models: Some(models),
                suggestion: Some(suggestion),
                multi_key_result,
                detected_protocols,
            });
        }

        // No success — use auth failure result if available
        if let Some((protocol, fixed_base_url)) = auth_failure {
            let multi_key_result = if req.test_all_keys && keys.len() > 1 {
                let effective = fixed_base_url.as_deref().unwrap_or(&req.base_url);
                Some(self.test_all_keys(&keys, protocol, effective, timeout).await)
            } else {
                None
            };
            return Ok(ProtocolDetectionResponse {
                protocol,
                confidence: 50,
                success: false,
                fixed_base_url,
                models: None,
                suggestion: Some(check_key_suggestion()),
                multi_key_result,
                detected_protocols: vec![],
            });
        }

        // All probes failed
        Ok(ProtocolDetectionResponse {
            protocol: ProtocolType::Unknown,
            confidence: 0,
            success: false,
            fixed_base_url: None,
            models: None,
            suggestion: Some(check_key_suggestion()),
            multi_key_result: None,
            detected_protocols: vec![],
        })
    }

    // -- Per-protocol probing --

    async fn probe_protocol(
        &self,
        protocol: ProtocolType,
        base_url: &str,
        api_key: &str,
        timeout: Duration,
    ) -> Result<ProbeOutcome, AppError> {
        let base = base_url.trim_end_matches('/');
        match protocol {
            ProtocolType::OpenAI => self.probe_openai(base, api_key, timeout).await,
            ProtocolType::Anthropic => self.probe_anthropic(base, api_key, timeout).await,
            ProtocolType::Gemini => self.probe_gemini(base, api_key, timeout).await,
            ProtocolType::Unknown => Err(AppError::Internal("Cannot probe unknown".into())),
        }
    }

    async fn probe_openai(&self, base: &str, api_key: &str, timeout: Duration) -> Result<ProbeOutcome, AppError> {
        let client = self.http_client();
        let urls = [
            (format!("{base}/models"), None),
            (format!("{base}/v1/models"), Some(format!("{base}/v1"))),
        ];

        let mut last_auth_failure: Option<Option<String>> = None;

        for (url, fixed) in &urls {
            let resp = client
                .get(url)
                .header("Authorization", format!("Bearer {api_key}"))
                .timeout(timeout)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let body: DataResponse = r
                        .json()
                        .await
                        .map_err(|e| AppError::BadGateway(format!("Parse failed: {e}")))?;
                    let confidence = if fixed.is_some() { 80 } else { 90 };
                    return Ok(ProbeOutcome::Success {
                        models: body.data.into_iter().map(|m| m.id).collect(),
                        fixed_base_url: fixed.clone(),
                        confidence,
                    });
                }
                Ok(r) if is_auth_error(r.status()) => {
                    last_auth_failure = Some(fixed.clone());
                }
                _ => {}
            }
        }

        if let Some(fixed) = last_auth_failure {
            return Ok(ProbeOutcome::AuthFailure { fixed_base_url: fixed });
        }

        Err(AppError::BadGateway("OpenAI probe failed".into()))
    }

    async fn probe_anthropic(&self, base: &str, api_key: &str, timeout: Duration) -> Result<ProbeOutcome, AppError> {
        let client = self.http_client();
        let url = format!("{base}/v1/models");
        let resp = client
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| AppError::BadGateway(format!("Anthropic probe failed: {e}")))?;

        if resp.status().is_success() {
            let body: DataResponse = resp
                .json()
                .await
                .map_err(|e| AppError::BadGateway(format!("Parse failed: {e}")))?;
            return Ok(ProbeOutcome::Success {
                models: body.data.into_iter().map(|m| m.id).collect(),
                fixed_base_url: None,
                confidence: 95,
            });
        }

        if is_auth_error(resp.status()) {
            return Ok(ProbeOutcome::AuthFailure { fixed_base_url: None });
        }

        Err(AppError::BadGateway(format!("Anthropic returned {}", resp.status())))
    }

    async fn probe_gemini(&self, base: &str, api_key: &str, timeout: Duration) -> Result<ProbeOutcome, AppError> {
        let client = self.http_client();
        let url = format!("{base}/v1beta/models?key={api_key}");
        let resp = client
            .get(&url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| AppError::BadGateway(format!("Gemini probe failed: {e}")))?;

        if resp.status().is_success() {
            let body: GeminiResponse = resp
                .json()
                .await
                .map_err(|e| AppError::BadGateway(format!("Parse failed: {e}")))?;
            let models = body
                .models
                .into_iter()
                .map(|m| m.name.strip_prefix("models/").unwrap_or(&m.name).to_owned())
                .collect();
            return Ok(ProbeOutcome::Success {
                models,
                fixed_base_url: None,
                confidence: 90,
            });
        }

        if is_auth_error(resp.status()) {
            return Ok(ProbeOutcome::AuthFailure { fixed_base_url: None });
        }

        Err(AppError::BadGateway(format!("Gemini returned {}", resp.status())))
    }

    // -- Multi-key testing --

    async fn test_all_keys(
        &self,
        keys: &[String],
        protocol: ProtocolType,
        effective_base: &str,
        timeout: Duration,
    ) -> MultiKeyResult {
        let base = effective_base.trim_end_matches('/').to_owned();
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_KEY_TESTS));
        let mut set = JoinSet::new();

        for (i, key) in keys.iter().enumerate() {
            let make_client = Arc::clone(&self.http_client);
            let key = key.clone();
            let base = base.clone();
            let sem = sem.clone();

            set.spawn(async move {
                let _permit = sem.acquire().await;
                let start = Instant::now();
                let client = make_client();
                let ok = test_single_key(&client, protocol, &base, &key, timeout).await;
                let latency = start.elapsed().as_millis() as i64;

                KeyTestResult {
                    index: i,
                    masked_key: mask_api_key(&key),
                    valid: ok.is_ok(),
                    latency: Some(latency),
                    error: ok.err().map(|e| e.to_string()),
                }
            });
        }

        let mut details = Vec::with_capacity(keys.len());
        while let Some(result) = set.join_next().await {
            if let Ok(kr) = result {
                details.push(kr);
            }
        }
        details.sort_by_key(|r| r.index);

        let valid = details.iter().filter(|r| r.valid).count();
        MultiKeyResult {
            total: keys.len(),
            valid,
            invalid: keys.len() - valid,
            details,
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn validate_request(req: &DetectProtocolRequest) -> Result<(), AppError> {
    if req.base_url.trim().is_empty() {
        return Err(AppError::BadRequest("baseUrl is required".into()));
    }
    if req.api_key.trim().is_empty() {
        return Err(AppError::BadRequest("apiKey is required".into()));
    }
    Ok(())
}

fn parse_keys(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn infer_from_url(url: &str) -> Option<ProtocolType> {
    let lower = url.to_lowercase();
    if lower.contains("anthropic") {
        Some(ProtocolType::Anthropic)
    } else if lower.contains("generativelanguage.googleapis.com") {
        Some(ProtocolType::Gemini)
    } else if lower.contains("openai") {
        Some(ProtocolType::OpenAI)
    } else {
        None
    }
}

fn infer_from_key(key: &str) -> Option<ProtocolType> {
    if key.starts_with("sk-ant-") {
        Some(ProtocolType::Anthropic)
    } else if key.starts_with("AIza") {
        Some(ProtocolType::Gemini)
    } else {
        None
    }
}

/// Build ordered list of protocols to test.
/// Priority: preferred > URL inference > Key inference > default order.
fn build_test_order(
    preferred: Option<ProtocolType>,
    url_inferred: Option<ProtocolType>,
    key_inferred: Option<ProtocolType>,
) -> Vec<ProtocolType> {
    let defaults = [ProtocolType::OpenAI, ProtocolType::Anthropic, ProtocolType::Gemini];
    let mut order = Vec::with_capacity(3);

    for p in [preferred, url_inferred, key_inferred].into_iter().flatten() {
        if p != ProtocolType::Unknown && !order.contains(&p) {
            order.push(p);
        }
    }
    for p in defaults {
        if !order.contains(&p) {
            order.push(p);
        }
    }
    order
}

fn is_auth_error(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
}

fn protocol_display_name(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::OpenAI => "OpenAI",
        ProtocolType::Anthropic => "Anthropic",
        ProtocolType::Gemini => "Gemini",
        ProtocolType::Unknown => "Unknown",
    }
}

fn success_suggestion(detected: ProtocolType, preferred: Option<ProtocolType>) -> DetectionSuggestion {
    let should_switch = matches!(preferred, Some(p) if p != ProtocolType::Unknown && p != detected);
    if should_switch {
        DetectionSuggestion {
            suggestion_type: SuggestionType::SwitchPlatform,
            message: format!(
                "Detected {} protocol, but preferred was {}",
                protocol_display_name(detected),
                protocol_display_name(preferred.unwrap_or(ProtocolType::Unknown)),
            ),
            i18n_key: Some("settings.protocolMismatch".into()),
        }
    } else {
        DetectionSuggestion {
            suggestion_type: SuggestionType::None,
            message: format!("Detected {} protocol", protocol_display_name(detected)),
            i18n_key: Some("settings.protocolDetected".into()),
        }
    }
}

fn check_key_suggestion() -> DetectionSuggestion {
    DetectionSuggestion {
        suggestion_type: SuggestionType::CheckKey,
        message: "Could not detect protocol. Please check your API key and URL.".into(),
        i18n_key: Some("settings.protocolDetectionFailed".into()),
    }
}

/// Test a single key against the detected protocol endpoint.
async fn test_single_key(
    client: &reqwest::Client,
    protocol: ProtocolType,
    base: &str,
    api_key: &str,
    timeout: Duration,
) -> Result<(), AppError> {
    let (url, headers) = match protocol {
        ProtocolType::OpenAI => (
            format!("{base}/models"),
            vec![("Authorization", format!("Bearer {api_key}"))],
        ),
        ProtocolType::Anthropic => (
            format!("{base}/v1/models"),
            vec![
                ("x-api-key", api_key.to_owned()),
                ("anthropic-version", "2023-06-01".to_owned()),
            ],
        ),
        ProtocolType::Gemini => (format!("{base}/v1beta/models?key={api_key}"), vec![]),
        ProtocolType::Unknown => {
            return Err(AppError::Internal("Cannot test unknown protocol".into()));
        }
    };

    let mut req = client.get(&url).timeout(timeout);
    for (k, v) in &headers {
        req = req.header(*k, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| AppError::BadGateway(format!("Request failed: {e}")))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(AppError::BadGateway(format!("Status: {}", resp.status())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_keys --

    #[test]
    fn parse_keys_single() {
        let keys = parse_keys("sk-test-key");
        assert_eq!(keys, vec!["sk-test-key"]);
    }

    #[test]
    fn parse_keys_comma_separated() {
        let keys = parse_keys("key1,key2,key3");
        assert_eq!(keys, vec!["key1", "key2", "key3"]);
    }

    #[test]
    fn parse_keys_newline_separated() {
        let keys = parse_keys("key1\nkey2\nkey3");
        assert_eq!(keys, vec!["key1", "key2", "key3"]);
    }

    #[test]
    fn parse_keys_mixed_with_whitespace() {
        let keys = parse_keys(" key1 , key2 \n key3 ");
        assert_eq!(keys, vec!["key1", "key2", "key3"]);
    }

    #[test]
    fn parse_keys_filters_empty() {
        let keys = parse_keys("key1,,key2,");
        assert_eq!(keys, vec!["key1", "key2"]);
    }

    // -- infer_from_url --

    #[test]
    fn infer_url_anthropic() {
        assert_eq!(
            infer_from_url("https://api.anthropic.com"),
            Some(ProtocolType::Anthropic)
        );
    }

    #[test]
    fn infer_url_gemini() {
        assert_eq!(
            infer_from_url("https://generativelanguage.googleapis.com"),
            Some(ProtocolType::Gemini)
        );
    }

    #[test]
    fn infer_url_openai() {
        assert_eq!(infer_from_url("https://api.openai.com/v1"), Some(ProtocolType::OpenAI));
    }

    #[test]
    fn infer_url_unknown() {
        assert_eq!(infer_from_url("https://my-custom-api.com"), None);
    }

    #[test]
    fn infer_url_case_insensitive() {
        assert_eq!(
            infer_from_url("https://API.ANTHROPIC.COM"),
            Some(ProtocolType::Anthropic)
        );
    }

    // -- infer_from_key --

    #[test]
    fn infer_key_anthropic() {
        assert_eq!(infer_from_key("sk-ant-api03-test1234"), Some(ProtocolType::Anthropic));
    }

    #[test]
    fn infer_key_gemini() {
        assert_eq!(infer_from_key("AIzaSyBxxxxxx"), Some(ProtocolType::Gemini));
    }

    #[test]
    fn infer_key_generic() {
        assert_eq!(infer_from_key("sk-proj-abc123"), None);
    }

    // -- build_test_order --

    #[test]
    fn test_order_default() {
        let order = build_test_order(None, None, None);
        assert_eq!(
            order,
            vec![ProtocolType::OpenAI, ProtocolType::Anthropic, ProtocolType::Gemini]
        );
    }

    #[test]
    fn test_order_preferred_first() {
        let order = build_test_order(Some(ProtocolType::Gemini), None, None);
        assert_eq!(order[0], ProtocolType::Gemini);
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_order_url_inferred() {
        let order = build_test_order(None, Some(ProtocolType::Anthropic), None);
        assert_eq!(order[0], ProtocolType::Anthropic);
    }

    #[test]
    fn test_order_key_inferred() {
        let order = build_test_order(None, None, Some(ProtocolType::Gemini));
        assert_eq!(order[0], ProtocolType::Gemini);
        assert_eq!(order[1], ProtocolType::OpenAI);
        assert_eq!(order[2], ProtocolType::Anthropic);
    }

    #[test]
    fn test_order_preferred_overrides() {
        let order = build_test_order(
            Some(ProtocolType::Gemini),
            Some(ProtocolType::Anthropic),
            Some(ProtocolType::OpenAI),
        );
        assert_eq!(
            order,
            vec![ProtocolType::Gemini, ProtocolType::Anthropic, ProtocolType::OpenAI]
        );
    }

    #[test]
    fn test_order_no_duplicates() {
        let order = build_test_order(
            Some(ProtocolType::OpenAI),
            Some(ProtocolType::OpenAI),
            Some(ProtocolType::OpenAI),
        );
        assert_eq!(order.len(), 3);
        // Each protocol appears exactly once
        assert!(order.contains(&ProtocolType::OpenAI));
        assert!(order.contains(&ProtocolType::Anthropic));
        assert!(order.contains(&ProtocolType::Gemini));
    }

    #[test]
    fn test_order_unknown_preferred_ignored() {
        let order = build_test_order(Some(ProtocolType::Unknown), None, None);
        assert_eq!(
            order,
            vec![ProtocolType::OpenAI, ProtocolType::Anthropic, ProtocolType::Gemini]
        );
    }

    // -- validate_request --

    #[test]
    fn validate_empty_base_url() {
        let req = DetectProtocolRequest {
            base_url: "  ".into(),
            api_key: "sk-test".into(),
            timeout: None,
            test_all_keys: false,
            preferred_protocol: None,
        };
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validate_empty_api_key() {
        let req = DetectProtocolRequest {
            base_url: "https://api.example.com".into(),
            api_key: "  ".into(),
            timeout: None,
            test_all_keys: false,
            preferred_protocol: None,
        };
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validate_ok() {
        let req = DetectProtocolRequest {
            base_url: "https://api.example.com".into(),
            api_key: "sk-test".into(),
            timeout: None,
            test_all_keys: false,
            preferred_protocol: None,
        };
        assert!(validate_request(&req).is_ok());
    }

    // -- suggestion helpers --

    #[test]
    fn success_suggestion_no_preferred() {
        let s = success_suggestion(ProtocolType::Anthropic, None);
        assert_eq!(s.suggestion_type, SuggestionType::None);
        assert!(s.message.contains("Anthropic"));
    }

    #[test]
    fn success_suggestion_same_preferred() {
        let s = success_suggestion(ProtocolType::Anthropic, Some(ProtocolType::Anthropic));
        assert_eq!(s.suggestion_type, SuggestionType::None);
    }

    #[test]
    fn success_suggestion_different_preferred_returns_switch() {
        let s = success_suggestion(ProtocolType::OpenAI, Some(ProtocolType::Anthropic));
        assert_eq!(s.suggestion_type, SuggestionType::SwitchPlatform);
        assert!(s.message.contains("OpenAI"));
        assert!(s.message.contains("Anthropic"));
    }

    #[test]
    fn success_suggestion_unknown_preferred_is_ignored() {
        let s = success_suggestion(ProtocolType::Anthropic, Some(ProtocolType::Unknown));
        assert_eq!(s.suggestion_type, SuggestionType::None);
    }

    #[test]
    fn check_key_suggestion_has_check_key_type() {
        let s = check_key_suggestion();
        assert_eq!(s.suggestion_type, SuggestionType::CheckKey);
    }

    // -- is_auth_error --

    #[test]
    fn auth_error_detection() {
        assert!(is_auth_error(reqwest::StatusCode::UNAUTHORIZED));
        assert!(is_auth_error(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_auth_error(reqwest::StatusCode::OK));
        assert!(!is_auth_error(reqwest::StatusCode::NOT_FOUND));
    }
}
