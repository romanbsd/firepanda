//! Browser handle. M1 parity slice: options + the failure path that
//! `tests/protocol.py` asserts ("Cannot start Lightpanda" as a tool error
//! when the binary is missing). Child management, CDP, and the pool land in
//! M2+ per docs/rust-migration-plan.md.

use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Options {
    pub lightpanda: String,
    pub socket: String,
    /// Zero keeps the managed browser warm indefinitely.
    pub idle_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lightpanda: "lightpanda".to_string(),
            socket: String::new(),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Validated once at the tool boundary, then reused for each page of a crawl.
/// Field reads arrive with the M2/M4 pipeline.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PageRequest {
    pub url: String,
    pub formats: Vec<String>,
    pub only_main_content: bool,
    pub timeout_ms: i64,
    pub wait_for_ms: i64,
    pub wait_for_selector: String,
    pub wait_for_text: String,
}

pub struct Browser {
    options: Options,
}

impl Browser {
    pub fn new(options: Options) -> Self {
        Self { options }
    }

    /// M1: no CDP yet. `tests/protocol.py` only exercises the failure path —
    /// a valid scrape against a missing Lightpanda must surface as a tool
    /// error containing "Cannot start Lightpanda", never as a JSON-RPC error.
    pub fn scrape(&self, _request: &PageRequest) -> Result<Value, String> {
        Err(format!(
            "Cannot start Lightpanda: browser control is not yet ported (milestone M2); configured binary: {}",
            self.options.lightpanda
        ))
    }

    /// Shut the browser down if the idle deadline passed. No-op until M2.
    pub fn stop_if_idle(&self) {}

    /// None while no managed browser is running. Wired into the main select
    /// loop once M2 brings the managed child up.
    #[allow(dead_code)]
    pub fn idle_deadline(&self) -> Option<Instant> {
        None
    }
}
