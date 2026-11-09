//! Shared browser-impersonated HTTP client for non-browser requests: the `fetch`
//! tool and search-engine page loads. Requests run under an absolute deadline
//! and a caller-supplied body cap.
//!
//! primp negotiates HTTP/2 over TLS via ALPN (Chrome's HTTP/2 window settings)
//! and falls back to HTTP/1.1; cleartext requests use HTTP/1.1. One process-wide
//! client keeps pooled connections (90 s idle), `TCP_NODELAY`, and compression
//! support, so repeated fetches reuse connections instead of reconnecting.

use std::sync::OnceLock;
use std::time::Duration;

/// Client backstop. Per-request deadlines are always stricter.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// One process-wide client: connection pooling and the browser fingerprint are
/// shared by every request.
pub fn client() -> Result<&'static primp::Client, String> {
    static CLIENT: OnceLock<Result<primp::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            primp::Client::builder()
                .impersonate(primp::Impersonate::ChromeV146)
                .timeout(CLIENT_TIMEOUT)
                .build()
                .map_err(|error| format!("HTTP client failed: {}", error.without_url()))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Read a body up to `max_bytes`, failing as soon as the deadline expires.
/// `label` prefixes error messages ("HTTP", "Search").
pub async fn read_limited(
    mut response: primp::Response,
    max_bytes: usize,
    deadline: tokio::time::Instant,
    label: &str,
) -> Result<Vec<u8>, String> {
    let declared = response.content_length();
    if declared.is_some_and(|length| length > u64::try_from(max_bytes).unwrap_or(u64::MAX)) {
        return Err(too_large(label, max_bytes));
    }
    let expected = usize::try_from(declared.unwrap_or(0))
        .unwrap_or(max_bytes)
        .min(max_bytes);
    let mut body = Vec::with_capacity(expected);
    loop {
        let chunk = tokio::time::timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| format!("{label} request timed out"))?
            .map_err(|error| format!("{label} response failed: {}", error.without_url()))?;
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.len() > max_bytes - body.len() {
            return Err(too_large(label, max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn too_large(label: &str, max_bytes: usize) -> String {
    format!("{label} response exceeds {} MiB", max_bytes / (1024 * 1024))
}

/// UTF-8 is the normal case; a lossy decode only matters for malformed bodies.
pub fn text_or_lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}
