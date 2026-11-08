//! 429 handling shared by search engines.
//!
//! Brave's HTML endpoint was observed on 2026-11-08 returning HTTP 429 with no
//! `Retry-After` or `X-RateLimit-*` headers, and it stayed 429 after 15 s and
//! 60 s from the same IP. With no declared policy, use a conservative
//! five-minute cooldown after exhausting the short in-call retries. A server
//! header always wins over that default.

use super::Engine;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// Short retries inside one tool call: initial request plus two retries.
pub const MAX_RETRIES: u32 = 2;
/// Cooldown when a 429 has no retry header.
pub const DEFAULT_429_COOLDOWN: Duration = Duration::from_secs(300);
/// Upper bound on a declared wait; longer values are clamped.
const MAX_DECLARED_WAIT: Duration = Duration::from_secs(3600);

pub fn retry_after(response: &primp::Response) -> Option<Duration> {
    let headers = response.headers();
    if let Some(value) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        && let Some(delay) = parse_retry_after(value)
    {
        return Some(delay);
    }
    // Some APIs use this as a delta or an epoch timestamp.
    headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_reset)
}

/// Parse `Retry-After` (delta seconds or HTTP-date) into a bounded wait.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_DECLARED_WAIT));
    }
    let when = httpdate::parse_http_date(value).ok()?;
    let wait = when.duration_since(SystemTime::now()).ok()?;
    Some(wait.min(MAX_DECLARED_WAIT))
}

fn parse_reset(value: &str) -> Option<Duration> {
    let value = value.trim().parse::<u64>().ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_secs());
    // Values near the current epoch are absolute; smaller ones are deltas.
    let seconds = if value > 1_000_000_000 {
        value.saturating_sub(now)
    } else {
        value
    };
    Some(Duration::from_secs(seconds).min(MAX_DECLARED_WAIT))
}

/// Short exponential backoff used only when the server sends no header.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(3))
}

pub const fn label(engine: Engine) -> &'static str {
    match engine {
        Engine::DuckDuckGo => "DuckDuckGo",
        Engine::Brave => "Brave",
    }
}

pub fn rate_limited_error(engine: Engine, wait: Duration) -> String {
    format!(
        "{} rate limited (HTTP 429); retry in {}s",
        label(engine),
        wait.as_secs().max(1)
    )
}

fn slots() -> &'static Mutex<[Option<Instant>; 2]> {
    static SLOTS: OnceLock<Mutex<[Option<Instant>; 2]>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new([None, None]))
}

const fn slot_index(engine: Engine) -> usize {
    match engine {
        Engine::DuckDuckGo => 0,
        Engine::Brave => 1,
    }
}

fn cooldown_until(engine: Engine) -> Option<Instant> {
    let slots = slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slots[slot_index(engine)]
}

pub fn set_cooldown(engine: Engine, until: Instant) {
    let mut slots = slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slots[slot_index(engine)] = Some(until);
}

pub fn clear_cooldown(engine: Engine) {
    let mut slots = slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slots[slot_index(engine)] = None;
}

/// Wait out an existing cooldown when the remaining tool budget can absorb it.
/// If it cannot, fail immediately with the retry hint instead of sleeping.
pub async fn wait_for_slot(engine: Engine, deadline: Instant) -> Result<(), String> {
    let Some(until) = cooldown_until(engine) else {
        return Ok(());
    };
    let now = Instant::now();
    if until <= now {
        return Ok(());
    }
    let wait = until - now;
    if wait >= deadline.saturating_duration_since(now) {
        return Err(rate_limited_error(engine, wait));
    }
    tokio::time::sleep_until(until).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after("999999"), Some(MAX_DECLARED_WAIT));
        assert_eq!(parse_retry_after("not-a-date"), None);
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(2));
        let parsed = parse_retry_after(&date).expect("valid HTTP date");
        assert!(
            parsed >= Duration::from_secs(1) && parsed <= Duration::from_secs(3),
            "{parsed:?}"
        );
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(9), Duration::from_secs(8));
    }

    #[test]
    fn reset_header_accepts_delta_or_epoch() {
        assert_eq!(parse_reset("2"), Some(Duration::from_secs(2)));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_secs();
        let parsed = parse_reset(&(now + 5).to_string()).expect("epoch reset");
        assert!(
            parsed <= Duration::from_secs(5) && parsed >= Duration::from_secs(4),
            "{parsed:?}"
        );
    }

    #[test]
    fn rate_limit_message_names_the_engine() {
        let message = rate_limited_error(Engine::Brave, Duration::from_secs(300));
        assert!(
            message.contains("Brave") && message.contains("300s"),
            "{message}"
        );
    }
}
