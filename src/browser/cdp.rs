//! One CDP connection: deadline-bounded reads, command round-trips, and the
//! per-page scrape pipeline. `tests/transport.py` pins the adversarial
//! behavior.

use super::{Format, PageRequest, Stop};
use crate::tools::MAX_LINKS;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// In-page extractor, embedded verbatim.
const EXTRACTION_JS: &str = include_str!("extract.js");

const STOPPING: &str = "Service is stopping";
const STOPPED: &str = "Lightpanda operation timed out or service is stopping";
const TIMED_OUT: &str = "Lightpanda operation timed out";
const DISCONNECTED: &str = "Lightpanda CDP disconnected or returned a non-text frame";
/// Readiness budget held back from the extractor for the reply round trip.
const READY_MARGIN: Duration = Duration::from_millis(250);

/// CDP rejection (peer answered with an `error` object) versus transport,
/// deadline, stop, and validation failures. Both surface as tool errors;
/// only `Network.enable` distinguishes them.
#[derive(Debug)]
pub enum ScrapeError {
    Cdp(String),
    Fatal(String),
}

impl ScrapeError {
    pub fn message(self) -> String {
        match self {
            Self::Cdp(message) | Self::Fatal(message) => message,
        }
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self::Fatal(message.into())
    }
}

/// CDP mixes replies and events on one socket; non-string members read as "".
fn string_member<'a>(object: &'a Value, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Isolated context and flattened session for one page fetch.
struct PageTarget {
    context: String,
    session: String,
}

pub struct Connection {
    ws: Option<tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>>,
    stop: Stop,
    next_id: u64,
    active_session: String,
    page_loaded: bool,
    main_frame: String,
    http_status: Option<i64>,
    early_document_statuses: Vec<(String, i64)>,
}

impl Connection {
    pub const fn new(stop: Stop) -> Self {
        Self {
            ws: None,
            stop,
            next_id: 0,
            active_session: String::new(),
            page_loaded: false,
            main_frame: String::new(),
            http_status: None,
            early_document_statuses: Vec::new(),
        }
    }

    pub const fn is_open(&self) -> bool {
        self.ws.is_some()
    }

    pub fn attach(
        &mut self,
        websocket: tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    ) {
        self.ws = Some(websocket);
    }

    /// Read one CDP message before `deadline`, or fail. `StreamExt::next` is
    /// cancel-safe, so the stop/deadline arms of the select can drop a pending
    /// read without losing buffered bytes.
    async fn receive(&mut self, deadline: Instant) -> Result<Value, String> {
        loop {
            if self.stop.stopped() {
                return Err(STOPPING.to_string());
            }
            if Instant::now() >= deadline {
                return Err(TIMED_OUT.to_string());
            }
            let ws = self.ws.as_mut().ok_or_else(|| DISCONNECTED.to_string())?;
            let read = tokio::select! {
                () = self.stop.wait() => return Err(STOPPING.to_string()),
                () = tokio::time::sleep_until(deadline.into()) => return Err(TIMED_OUT.to_string()),
                message = ws.next() => message,
            };
            // A read finishing at/after the deadline failed like the C++
            // SocketDeadline shutdown, not like a clean timeout.
            if self.stop.stopped() || Instant::now() >= deadline {
                return Err(STOPPED.to_string());
            }
            match read {
                // The stream ending (None) is a disconnect like a close frame.
                None => return Err(DISCONNECTED.to_string()),
                Some(Ok(Message::Text(text))) => return parse_message(text.as_str()),
                Some(Ok(Message::Ping(payload))) => {
                    // Lightpanda may keepalive mid-scrape; answering inside the
                    // read loop keeps a control frame from failing the page.
                    ws.send(Message::Pong(payload))
                        .await
                        .map_err(|_| DISCONNECTED.to_string())?;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_) | Message::Binary(_)) | Err(_)) => {
                    return Err(DISCONNECTED.to_string());
                }
            }
        }
    }

    /// Fold the events that matter for one page (load flag, main-document
    /// status) without retaining multi-megabyte event bodies.
    fn observe(&mut self, message: &Value) {
        let method = string_member(message, "method");
        let session_id = string_member(message, "sessionId");
        if method == "Page.loadEventFired" && session_id == self.active_session {
            self.page_loaded = true;
        }
        if method != "Network.responseReceived" || session_id != self.active_session {
            return;
        }
        let Some(params) = message.get("params").filter(|value| value.is_object()) else {
            return;
        };
        if string_member(params, "type") != "Document" {
            return;
        }
        let frame = match params.get("frameId") {
            Some(Value::String(id)) if id.len() <= 256 => id,
            _ => return,
        };
        let Some(response) = params.get("response").filter(|value| value.is_object()) else {
            return;
        };
        // Status must be an integer 100..=599: 201.5 and 99/600 never become
        // an httpStatus (C++ compares get<int> back against the number).
        let number = match response.get("status").and_then(Value::as_i64) {
            Some(status) if (100..=599).contains(&status) => status,
            _ => return,
        };
        if !self.main_frame.is_empty() {
            // Subframe responses cannot overwrite the document.
            if *frame == self.main_frame {
                self.http_status = Some(number);
            }
        } else if let Some(entry) = self
            .early_document_statuses
            .iter_mut()
            .find(|(id, _)| id == frame)
        {
            entry.1 = number;
        } else if self.early_document_statuses.len() < 16 {
            self.early_document_statuses.push((frame.clone(), number));
        }
    }

    /// Write one command without waiting for its reply and return its id. A
    /// reply nobody waits for is skipped by the next `call` like any other
    /// non-matching id.
    async fn send(
        &mut self,
        method: &str,
        params: Value,
        session: &str,
        deadline: Instant,
    ) -> Result<u64, ScrapeError> {
        self.next_id += 1;
        let id = self.next_id;
        // Build the envelope by moving `params`: a large Runtime.evaluate
        // payload (the embedded extractor call) must not be cloned here.
        let mut envelope = serde_json::Map::with_capacity(4);
        envelope.insert("id".to_string(), Value::from(id));
        envelope.insert("method".to_string(), Value::String(method.to_string()));
        envelope.insert("params".to_string(), params);
        if !session.is_empty() {
            envelope.insert("sessionId".to_string(), Value::String(session.to_string()));
        }
        let Ok(payload) = serde_json::to_string(&Value::Object(envelope)) else {
            return Err(ScrapeError::fatal("Cannot write Lightpanda CDP command"));
        };
        let ws = self
            .ws
            .as_mut()
            .ok_or_else(|| ScrapeError::fatal(DISCONNECTED))?;
        // The deadline owns writes too: a peer that stops reading must not
        // outlive the operation (C++ SocketDeadline shuts the socket down).
        let sent = tokio::select! {
            () = self.stop.wait() => return Err(ScrapeError::fatal(STOPPING)),
            sent = tokio::time::timeout_at(deadline.into(), ws.send(Message::text(payload))) => sent,
        };
        if !matches!(sent, Ok(Ok(()))) {
            return Err(ScrapeError::fatal("Cannot write Lightpanda CDP command"));
        }
        Ok(id)
    }

    /// Send one command and wait for its reply; events observed along the way.
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: &str,
        deadline: Instant,
    ) -> Result<Value, ScrapeError> {
        let id = self.send(method, params, session, deadline).await?;
        loop {
            let mut response = self.receive(deadline).await.map_err(ScrapeError::fatal)?;
            let Some(object) = response.as_object() else {
                return Err(ScrapeError::fatal("CDP response must be an object"));
            };
            match object.get("id") {
                // No id (0 in C++) can never match; keep observing.
                None => {
                    self.observe(&response);
                    continue;
                }
                Some(value) => match crate::json::as_integer(value) {
                    None => return Err(ScrapeError::fatal("CDP response id must be an integer")),
                    Some(number) if number != i128::from(id) => {
                        self.observe(&response);
                        continue;
                    }
                    Some(_) => {}
                },
            }
            let has_result = object.get("result").is_some();
            let has_error = object.get("error").is_some();
            if (!session.is_empty() && string_member(&response, "sessionId") != session)
                || has_result == has_error
            {
                return Err(ScrapeError::fatal("Malformed CDP response envelope"));
            }
            if has_error {
                return Err(ScrapeError::Cdp(format!("{method}: {}", object["error"])));
            }
            if !object.get("result").is_some_and(Value::is_object) {
                return Err(ScrapeError::fatal("CDP result must be an object"));
            }
            // Move the result out of the envelope: extraction replies carry
            // the whole page payload, so cloning here would duplicate it.
            return Ok(response
                .get_mut("result")
                .map(Value::take)
                .expect("result presence checked above"));
        }
    }

    async fn loaded(&mut self, deadline: Instant) -> Result<(), ScrapeError> {
        while !self.page_loaded {
            let message = self.receive(deadline).await.map_err(ScrapeError::fatal)?;
            self.observe(&message);
        }
        Ok(())
    }
}

/// CDP parse. serde_json's own 128-level recursion limit bounds the stack, so
/// multi-megabyte extraction replies skip the separate depth pre-scan that MCP
/// input needs for its distinct -32600 error.
fn parse_message(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|error| format!("Invalid CDP JSON: {error}"))
}

/// Isolated context + flattened session for this page. `disposeOnDetach`
/// releases the context even when the stream dies mid-scrape.
async fn open_page(
    connection: &mut Connection,
    deadline: Instant,
) -> Result<PageTarget, ScrapeError> {
    let context = connection
        .call(
            "Target.createBrowserContext",
            json!({"disposeOnDetach": true}),
            "",
            deadline,
        )
        .await?
        .get("browserContextId")
        .and_then(Value::as_str)
        .ok_or_else(|| ScrapeError::fatal("Missing browserContextId in CDP result"))?
        .to_string();
    let page = connection
        .call(
            "Target.createTarget",
            json!({"url": "about:blank", "browserContextId": context}),
            "",
            deadline,
        )
        .await?
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| ScrapeError::fatal("Missing targetId in CDP result"))?
        .to_string();
    let session = connection
        .call(
            "Target.attachToTarget",
            json!({"targetId": page, "flatten": true}),
            "",
            deadline,
        )
        .await?
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| ScrapeError::fatal("Missing sessionId in CDP result"))?
        .to_string();
    connection.active_session.clone_from(&session);
    connection.main_frame.clear();
    connection.http_status = None;
    connection.early_document_statuses.clear();
    Ok(PageTarget { context, session })
}

/// Enable the domains extraction needs, navigate, and wait for page load.
async fn navigate(
    connection: &mut Connection,
    request: &PageRequest,
    session: &str,
    deadline: Instant,
) -> Result<(), ScrapeError> {
    connection
        .call("Page.enable", json!({}), session, deadline)
        .await?;
    if let Err(ScrapeError::Cdp(_)) = connection
        .call("Network.enable", json!({}), session, deadline)
        .await
    {
        // Older CDP peers may lack Network; transport/timeouts still fail.
    }
    connection.page_loaded = false;
    let navigation = connection
        .call(
            "Page.navigate",
            json!({"url": request.url}),
            session,
            deadline,
        )
        .await?;
    connection.main_frame = string_member(&navigation, "frameId").to_string();
    // Responses observed before the frame id was known belong to the document
    // only if they match the main frame; subframes never overwrite it.
    let early = std::mem::take(&mut connection.early_document_statuses);
    for (frame, status) in early {
        if !connection.main_frame.is_empty() && frame == connection.main_frame {
            connection.http_status = Some(status);
        }
    }
    if navigation.get("errorText").is_some() {
        let text = navigation["errorText"]
            .as_str()
            .ok_or_else(|| ScrapeError::fatal("Page.navigate errorText must be a string"))?;
        return Err(ScrapeError::fatal(text));
    }
    connection.loaded(deadline).await
}

/// Sleep in short slices so SIGTERM still tears down the owned child promptly.
async fn wait_after_load(
    request: &PageRequest,
    deadline: Instant,
    stop: &Stop,
) -> Result<(), ScrapeError> {
    let now = Instant::now();
    if now + request.wait_for >= deadline {
        return Err(ScrapeError::fatal(
            "waitFor exceeds remaining operation timeout",
        ));
    }
    let wait_end = now + request.wait_for;
    while Instant::now() < wait_end {
        if stop.stopped() {
            return Err(ScrapeError::fatal(STOPPING));
        }
        let slice = std::cmp::min(wait_end, Instant::now() + Duration::from_millis(50));
        tokio::select! {
            () = stop.wait() => return Err(ScrapeError::fatal(STOPPING)),
            () = tokio::time::sleep_until(slice.into()) => {}
        }
    }
    Ok(())
}

/// Evaluate the embedded extractor once in the page context.
async fn extract(
    connection: &mut Connection,
    request: &PageRequest,
    session: &str,
    deadline: Instant,
) -> Result<Value, ScrapeError> {
    // Leave headroom so an in-page readiness timeout reaches us as its own
    // error instead of losing the race to the CDP read deadline.
    let remaining = i64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(READY_MARGIN)
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    let extraction = json!({
        "maxLinks": MAX_LINKS,
        "formats": request.formats,
        "onlyMainContent": request.only_main_content,
        "waitForSelector": request.wait_for_selector,
        "waitForText": request.wait_for_text,
        "readyTimeout": remaining,
    });
    let expression = format!("{EXTRACTION_JS}({extraction})");
    // Move the expression string into the params map: `json!` would clone it,
    // and it carries the whole embedded extractor.
    let mut params = serde_json::Map::with_capacity(3);
    params.insert("expression".to_string(), Value::String(expression));
    params.insert("returnByValue".to_string(), Value::Bool(true));
    params.insert("awaitPromise".to_string(), Value::Bool(true));
    let mut value = connection
        .call("Runtime.evaluate", Value::Object(params), session, deadline)
        .await?;
    if value.get("exceptionDetails").is_some() {
        return Err(ScrapeError::fatal(format!(
            "Page extraction failed: {}",
            value["exceptionDetails"]
        )));
    }
    value
        .get_mut("result")
        .and_then(|inner| inner.get_mut("value"))
        .map(Value::take)
        .ok_or_else(|| ScrapeError::fatal("Page extraction failed: missing result value"))
}

/// Reject malformed page payloads before they enter a crawl result.
fn validate_extraction(result: &mut Value, request: &PageRequest) -> Result<(), ScrapeError> {
    let metadata_ok = result
        .get("metadata")
        .filter(|value| value.is_object())
        .and_then(|metadata| metadata.get("url"))
        .is_some_and(Value::is_string);
    if !metadata_ok {
        return Err(ScrapeError::fatal("Malformed page extraction metadata"));
    }
    for format in &request.formats {
        let valid = match result.get(format.as_str()) {
            None => false,
            Some(value) if *format == Format::Links => value.is_array(),
            Some(value) => value.is_string(),
        };
        if !valid {
            return Err(ScrapeError::fatal(format!(
                "Missing or invalid extraction format: {format}"
            )));
        }
    }
    if let Some(links) = result.get("links") {
        let array = links.as_array().filter(|links| links.len() <= MAX_LINKS);
        let Some(links) = array else {
            return Err(ScrapeError::fatal("Invalid extraction links"));
        };
        for link in links {
            let valid = link.is_object()
                && link.get("url").is_some_and(Value::is_string)
                && (link.get("title").is_none() || link["title"].is_string());
            if !valid {
                return Err(ScrapeError::fatal("Malformed extraction link"));
            }
        }
    }
    if let Some(flag) = result.get("linksTruncated")
        && !flag.is_boolean()
    {
        return Err(ScrapeError::fatal("Invalid extraction truncation flag"));
    }
    result["metadata"]["sourceURL"] = json!(request.url);
    if let Some(warnings) = result.get("warnings") {
        let array = warnings.as_array().filter(|warnings| warnings.len() <= 3);
        let Some(warnings) = array else {
            return Err(ScrapeError::fatal("Malformed extraction warnings"));
        };
        for warning in warnings {
            let code_ok = warning
                .get("code")
                .and_then(Value::as_str)
                .is_some_and(|code| code.len() <= 64);
            let message_ok = warning
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.len() <= 512);
            if !warning.is_object() || !code_ok || !message_ok {
                return Err(ScrapeError::fatal("Malformed extraction warning"));
            }
        }
    }
    Ok(())
}

fn apply_http_status(result: &mut Value, status: i64) {
    result["metadata"]["httpStatus"] = json!(status);
    // Firecrawl names the same value `statusCode`; emit both so clients can
    // read either without a translation layer.
    result["metadata"]["statusCode"] = json!(status);
    if status < 400 {
        return;
    }
    if result.get("warnings").is_none() {
        result["warnings"] = json!([]);
    }
    if let Some(array) = result.get_mut("warnings").and_then(Value::as_array_mut) {
        array.push(json!({
            "code": "http_error",
            "message": format!("Document returned HTTP {status}"),
        }));
    }
}

/// One page fetch on an already connected connection. Errors propagate to the
/// caller; only `Browser::scrape` decides whether to reap the browser.
pub async fn run_scrape(
    connection: &mut Connection,
    request: &PageRequest,
    deadline: Instant,
) -> Result<Value, ScrapeError> {
    let target = open_page(connection, deadline).await?;
    navigate(connection, request, &target.session, deadline).await?;
    wait_after_load(request, deadline, &connection.stop).await?;
    let mut result = extract(connection, request, &target.session, deadline).await?;
    validate_extraction(&mut result, request)?;
    if let Some(status) = connection.http_status {
        apply_http_status(&mut result, status);
    }
    // Fire and forget: the reply would only cost a round trip per page, and
    // the next command on this connection skips it.
    connection
        .send(
            "Target.disposeBrowserContext",
            json!({"browserContextId": target.context}),
            "",
            deadline,
        )
        .await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp_parse_enforces_nesting_bound() {
        let deep = format!("{}0{}", "[".repeat(5000), "]".repeat(5000));
        let error = parse_message(&deep).unwrap_err();
        assert!(
            error.contains("Invalid CDP JSON: recursion limit exceeded"),
            "{error}"
        );
        assert!(parse_message("[]").is_ok());
        assert!(parse_message("{").is_err());
    }

    #[test]
    fn status_observation_is_main_document_only() {
        let mut connection = Connection::new(Stop::channel());
        connection.active_session = "session".to_string();
        let event = |frame: &str, status: Value| {
            json!({
                "method": "Network.responseReceived",
                "sessionId": "session",
                "params": {"type": "Document", "frameId": frame, "response": {"status": status}},
            })
        };
        connection.observe(&event("main", json!(201.5)));
        connection.observe(&event("child", json!(503)));
        assert!(connection.http_status.is_none());
        assert_eq!(
            connection.early_document_statuses,
            [("child".to_string(), 503)]
        );
        // Fractional statuses never become httpStatus; integers under 100 or
        // over 599 are ignored too.
        connection.observe(&event("main", json!(99)));
        connection.observe(&event("main", json!(600)));
        connection.observe(&event("main", json!(201)));
        assert!(connection.http_status.is_none());
        connection.main_frame = "child".to_string();
        connection.observe(&event("child", json!(404)));
        assert_eq!(connection.http_status, Some(404));
        // Subframe responses never overwrite the document.
        connection.observe(&event("main", json!(500)));
        assert_eq!(connection.http_status, Some(404));
    }

    #[test]
    fn extraction_validation_catches_malformed_payloads() {
        let request = PageRequest {
            url: "https://example.com".to_string(),
            formats: vec![Format::Markdown],
            only_main_content: true,
            timeout: Duration::from_secs(1),
            wait_for: Duration::ZERO,
            wait_for_selector: String::new(),
            wait_for_text: String::new(),
        };
        let mut missing = json!({"metadata": {"url": "https://example.com/"}});
        assert!(validate_extraction(&mut missing, &request).is_err());

        let mut good = json!({
            "metadata": {"url": "https://example.com/"},
            "markdown": "hello",
            "warnings": [false],
        });
        let error = validate_extraction(&mut good, &request).unwrap_err();
        assert_eq!(error.message(), "Malformed extraction warning");

        let mut good = json!({
            "metadata": {"url": "https://example.com/"},
            "markdown": "hello",
        });
        validate_extraction(&mut good, &request).unwrap();
        assert_eq!(good["metadata"]["sourceURL"], "https://example.com");
        apply_http_status(&mut good, 503);
        assert_eq!(good["metadata"]["httpStatus"], 503);
        assert_eq!(good["warnings"][0]["code"], "http_error");
    }
}
