//! One CDP connection: deadline-bounded reads, command round-trips, and the
//! per-page scrape pipeline. `tests/transport.py` pins the adversarial
//! behavior.

use super::{CdpStream, Format, PageRequest, Stop};
use crate::tools::MAX_LINKS;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// In-page extractor, embedded verbatim.
const EXTRACTION_JS: &str = include_str!("extract.js");

const STOPPING: &str = "Service is stopping";
const STOPPED: &str = "Browser operation timed out or service is stopping";
const TIMED_OUT: &str = "Browser operation timed out";
const DISCONNECTED: &str = "CDP disconnected or returned a non-text frame";
const TOO_LARGE: &str = "Browser reply exceeds 16 MiB; request fewer formats";
/// Readiness budget held back from the extractor for the reply round trip.
const READY_MARGIN: Duration = Duration::from_millis(250);

/// CDP rejection (peer answered with an `error` object), a page-level failure
/// (readiness, validation, extraction), or a transport failure that makes the
/// socket — and possibly the browser — unusable.
#[derive(Debug)]
pub enum ScrapeError {
    Cdp(String),
    Page(String),
    Transport(String),
}

impl ScrapeError {
    pub fn message(self) -> String {
        match self {
            Self::Cdp(message) | Self::Page(message) | Self::Transport(message) => message,
        }
    }

    /// The browser process is reusable after a page error unless this socket
    /// can no longer speak CDP. Readiness, validation, and ordinary rejections
    /// drop the connection only.
    pub const fn reaps_browser(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    fn page(message: impl Into<String>) -> Self {
        Self::Page(message.into())
    }

    fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
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

/// Page-scoped CDP observations. Reset for every browser context so a new page
/// can never inherit flags or statuses from the previous one.
#[derive(Debug, Default)]
struct PageState {
    loaded: bool,
    main_frame: String,
    http_status: Option<i64>,
    early_document_statuses: Vec<(String, i64)>,
    /// Frame ids whose `load` lifecycle event arrived before `Page.navigate`
    /// returned. Obscura flushes command side-effect events before the reply
    /// (Chromium semantics), so the document load can precede `main_frame`.
    early_loads: Vec<String>,
}

pub struct Connection {
    ws: Option<CdpStream>,
    stop: Stop,
    next_id: u64,
    active_session: String,
    page: PageState,
}

impl Connection {
    pub fn new(stop: Stop) -> Self {
        Self {
            ws: None,
            stop,
            next_id: 0,
            active_session: String::new(),
            page: PageState::default(),
        }
    }

    pub const fn is_open(&self) -> bool {
        self.ws.is_some()
    }

    pub fn attach(&mut self, websocket: CdpStream) {
        self.ws = Some(websocket);
    }

    /// Read one CDP message before `deadline`, or fail. `StreamExt::next` is
    /// cancel-safe, so the stop/deadline arms of the select can drop a pending
    /// read without losing buffered bytes.
    async fn receive(&mut self, deadline: Instant) -> Result<Value, ScrapeError> {
        loop {
            if self.stop.stopped() {
                return Err(ScrapeError::page(STOPPING));
            }
            if Instant::now() >= deadline {
                return Err(ScrapeError::page(TIMED_OUT));
            }
            let ws = self
                .ws
                .as_mut()
                .ok_or_else(|| ScrapeError::transport(DISCONNECTED))?;
            let read = tokio::select! {
                () = self.stop.wait() => return Err(ScrapeError::page(STOPPING)),
                () = tokio::time::sleep_until(deadline.into()) => return Err(ScrapeError::page(TIMED_OUT)),
                message = ws.next() => message,
            };
            // A read finishing at/after the deadline counts as a failed read,
            // not a clean timeout.
            if self.stop.stopped() || Instant::now() >= deadline {
                return Err(ScrapeError::page(STOPPED));
            }
            match read {
                // The stream ending (None) is a disconnect like a close frame.
                None => return Err(ScrapeError::transport(DISCONNECTED)),
                Some(Ok(Message::Text(text))) => {
                    return parse_message(text.as_str()).map_err(ScrapeError::transport);
                }
                Some(Ok(Message::Ping(payload))) => {
                    // A peer may keepalive mid-scrape; answering inside the
                    // read loop keeps a control frame from failing the page.
                    ws.send(Message::Pong(payload))
                        .await
                        .map_err(|_| ScrapeError::transport(DISCONNECTED))?;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                // The WebSocket size cap, not a crash: say what to change.
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                    return Err(ScrapeError::page(TOO_LARGE));
                }
                Some(Ok(Message::Close(_) | Message::Binary(_)) | Err(_)) => {
                    return Err(ScrapeError::transport(DISCONNECTED));
                }
            }
        }
    }

    /// Fold the events that matter for one page (load flag, main-document
    /// status) without retaining multi-megabyte event bodies.
    fn observe(&mut self, message: &Value) {
        if string_member(message, "sessionId") != self.active_session {
            return;
        }
        self.observe_load(message);
        self.observe_status(message);
    }

    /// `Page.loadEventFired` has no frame id. Ignore it until navigate has
    /// stored the document frame, so an about:blank load during `Page.enable`
    /// cannot finish the wait. A child frame's `loadEventFired` still has no id
    /// and can satisfy the wait after that. `Page.lifecycleEvent` `"load"` is
    /// accepted only for the main frame.
    fn observe_load(&mut self, message: &Value) {
        let method = string_member(message, "method");
        let params = message.get("params");
        let load = method == "Page.lifecycleEvent"
            && params.is_some_and(|params| string_member(params, "name") == "load");
        if self.page.main_frame.is_empty() {
            // Obscura flushes navigation events before the `Page.navigate`
            // reply, so the document load can arrive before its frame id is
            // known. Buffer frame ids; `Page.loadEventFired` stays ignored
            // because it carries no frame id.
            if load
                && let Some(frame) = params.map(|params| string_member(params, "frameId"))
                && !frame.is_empty()
                && self.page.early_loads.len() < 16
                && !self.page.early_loads.iter().any(|seen| seen == frame)
            {
                self.page.early_loads.push(frame.to_string());
            }
        } else if method == "Page.loadEventFired"
            || load
                && params
                    .is_some_and(|params| string_member(params, "frameId") == self.page.main_frame)
        {
            self.page.loaded = true;
        }
    }

    fn observe_status(&mut self, message: &Value) {
        if string_member(message, "method") != "Network.responseReceived" {
            return;
        }
        let Some(params) = message.get("params").filter(|value| value.is_object()) else {
            return;
        };
        if string_member(params, "type") != "Document" {
            return;
        }
        // A main-document response shares the navigation loader's request id.
        // Obscura reports iframe documents with the page's frame id, so the
        // frame alone cannot tell them apart; the request id can.
        let request = string_member(params, "requestId");
        let loader = string_member(params, "loaderId");
        if !request.is_empty() && !loader.is_empty() && request != loader {
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
        // a statusCode.
        let number = match response.get("status").and_then(Value::as_i64) {
            Some(status) if (100..=599).contains(&status) => status,
            _ => return,
        };
        if !self.page.main_frame.is_empty() {
            // Subframe responses cannot overwrite the document.
            if *frame == self.page.main_frame {
                self.page.http_status = Some(number);
            }
        } else if let Some(entry) = self
            .page
            .early_document_statuses
            .iter_mut()
            .find(|(id, _)| id == frame)
        {
            entry.1 = number;
        } else if self.page.early_document_statuses.len() < 16 {
            self.page
                .early_document_statuses
                .push((frame.clone(), number));
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
            return Err(ScrapeError::transport("Cannot write CDP command"));
        };
        let ws = self
            .ws
            .as_mut()
            .ok_or_else(|| ScrapeError::transport(DISCONNECTED))?;
        // The deadline owns writes too: a peer that stops reading must not
        // outlive the operation.
        let sent = tokio::select! {
            () = self.stop.wait() => return Err(ScrapeError::page(STOPPING)),
            sent = tokio::time::timeout_at(deadline.into(), ws.send(Message::text(payload))) => sent,
        };
        if !matches!(sent, Ok(Ok(()))) {
            return Err(ScrapeError::transport("Cannot write CDP command"));
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
            let mut response = self.receive(deadline).await?;
            let Some(object) = response.as_object() else {
                return Err(ScrapeError::transport("CDP response must be an object"));
            };
            match object.get("id") {
                // No id can never match; keep observing.
                None => {
                    self.observe(&response);
                    continue;
                }
                Some(value) => match crate::json::as_integer(value) {
                    None => {
                        return Err(ScrapeError::transport("CDP response id must be an integer"));
                    }
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
                return Err(ScrapeError::transport("Malformed CDP response envelope"));
            }
            if has_error {
                return Err(ScrapeError::Cdp(format!("{method}: {}", object["error"])));
            }
            if !object.get("result").is_some_and(Value::is_object) {
                return Err(ScrapeError::transport("CDP result must be an object"));
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
        while !self.page.loaded {
            let message = self.receive(deadline).await?;
            self.observe(&message);
        }
        Ok(())
    }
}

/// CDP parse. `serde_json`'s own 128-level recursion limit bounds the stack, so
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
        .ok_or_else(|| ScrapeError::transport("Missing browserContextId in CDP result"))?
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
        .ok_or_else(|| ScrapeError::transport("Missing targetId in CDP result"))?
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
        .ok_or_else(|| ScrapeError::transport("Missing sessionId in CDP result"))?
        .to_string();
    connection.active_session.clone_from(&session);
    connection.page = PageState::default();
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
    connection.page.loaded = false;
    let navigation = connection
        .call(
            "Page.navigate",
            json!({"url": request.url}),
            session,
            deadline,
        )
        .await?;
    connection.page.main_frame = string_member(&navigation, "frameId").to_string();
    // Responses observed before the frame id was known belong to the document
    // only if they match the main frame; subframes never overwrite it.
    let early = std::mem::take(&mut connection.page.early_document_statuses);
    for (frame, status) in early {
        if !connection.page.main_frame.is_empty() && frame == connection.page.main_frame {
            connection.page.http_status = Some(status);
        }
    }
    // A load event delivered before the navigate reply counts once its frame
    // matches the document frame.
    let early_load = connection
        .page
        .early_loads
        .iter()
        .any(|frame| frame == &connection.page.main_frame);
    connection.page.early_loads.clear();
    if early_load {
        connection.page.loaded = true;
    }
    if navigation.get("errorText").is_some() {
        let text = navigation["errorText"]
            .as_str()
            .ok_or_else(|| ScrapeError::transport("Page.navigate errorText must be a string"))?;
        return Err(ScrapeError::page(text));
    }
    connection.loaded(deadline).await
}

/// Sleep in short slices so SIGTERM still tears down the owned child promptly.
/// `waitFor` is shortened to the time still inside this page's deadline.
async fn wait_after_load(
    request: &PageRequest,
    deadline: Instant,
    stop: &Stop,
) -> Result<(), ScrapeError> {
    let now = Instant::now();
    let usable = deadline
        .saturating_duration_since(now)
        .saturating_sub(READY_MARGIN);
    let wait = request.wait_for.min(usable);
    if wait.is_zero() {
        return Ok(());
    }
    let wait_end = now + wait;
    while Instant::now() < wait_end {
        if stop.stopped() {
            return Err(ScrapeError::page(STOPPING));
        }
        let slice = std::cmp::min(wait_end, Instant::now() + Duration::from_millis(50));
        tokio::select! {
            () = stop.wait() => return Err(ScrapeError::page(STOPPING)),
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
        "markdownLinks": request.markdown_links,
    });
    let expression = format!("{EXTRACTION_JS}({extraction})");
    // Move the expression string into the params map: `json!` would clone it,
    // and it carries the whole embedded extractor.
    let mut params = serde_json::Map::with_capacity(4);
    params.insert("expression".to_string(), Value::String(expression));
    params.insert("returnByValue".to_string(), Value::Bool(true));
    params.insert("awaitPromise".to_string(), Value::Bool(true));
    // Obscura defaults an evaluation to 30 s regardless of the page budget.
    // Keep the in-page readiness wait inside a longer `timeout`.
    params.insert("timeout".to_string(), Value::from(remaining.max(1)));
    let mut value = connection
        .call("Runtime.evaluate", Value::Object(params), session, deadline)
        .await?;
    if value.get("exceptionDetails").is_some() {
        return Err(ScrapeError::page(format!(
            "Page extraction failed: {}",
            value["exceptionDetails"]
        )));
    }
    value
        .get_mut("result")
        .and_then(|inner| inner.get_mut("value"))
        .map(Value::take)
        .ok_or_else(|| ScrapeError::transport("Page extraction failed: missing result value"))
}

/// Reject malformed page payloads before they enter a crawl result.
fn validate_extraction(result: &mut Value, request: &PageRequest) -> Result<(), ScrapeError> {
    let metadata_ok = result
        .get("metadata")
        .filter(|value| value.is_object())
        .and_then(|metadata| metadata.get("url"))
        .is_some_and(Value::is_string);
    if !metadata_ok {
        return Err(ScrapeError::page("Malformed page extraction metadata"));
    }
    for format in &request.formats {
        let valid = match result.get(format.as_str()) {
            None => false,
            Some(value) if *format == Format::Links => value.is_array(),
            Some(value) => value.is_string(),
        };
        if !valid {
            return Err(ScrapeError::page(format!(
                "Missing or invalid extraction format: {format}"
            )));
        }
    }
    if let Some(links) = result.get("links") {
        let array = links.as_array().filter(|links| links.len() <= MAX_LINKS);
        let Some(links) = array else {
            return Err(ScrapeError::page("Invalid extraction links"));
        };
        for link in links {
            let valid = link.is_object()
                && link.get("url").is_some_and(Value::is_string)
                && (link.get("title").is_none() || link["title"].is_string());
            if !valid {
                return Err(ScrapeError::page("Malformed extraction link"));
            }
        }
    }
    if let Some(flag) = result.get("linksTruncated")
        && !flag.is_boolean()
    {
        return Err(ScrapeError::page("Invalid extraction truncation flag"));
    }
    result["metadata"]["sourceURL"] = json!(request.url);
    if let Some(warnings) = result.get("warnings") {
        let array = warnings.as_array().filter(|warnings| warnings.len() <= 3);
        let Some(warnings) = array else {
            return Err(ScrapeError::page("Malformed extraction warnings"));
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
                return Err(ScrapeError::page("Malformed extraction warning"));
            }
        }
    }
    Ok(())
}

fn push_warning(result: &mut Value, code: &str, message: String) {
    if result.get("warnings").is_none() {
        result["warnings"] = json!([]);
    }
    if let Some(array) = result.get_mut("warnings").and_then(Value::as_array_mut) {
        array.push(crate::json::object([
            ("code", Value::String(code.to_string())),
            ("message", Value::String(message)),
        ]));
    }
}

fn apply_http_status(result: &mut Value, status: i64) {
    // Firecrawl's metadata name; one field, as Firecrawl and its MCP send.
    result["metadata"]["statusCode"] = json!(status);
    if status >= 400 {
        push_warning(
            result,
            "http_error",
            format!("Document returned HTTP {status}"),
        );
    }
}

/// Cut each content format to `max_chars` characters (0 = no limit) at a
/// character boundary, and say so in a warning.
fn truncate_content(result: &mut Value, max_chars: usize) {
    if max_chars == 0 {
        return;
    }
    let mut cut = false;
    for format in [
        Format::Markdown,
        Format::Text,
        Format::Html,
        Format::RawHtml,
    ] {
        if let Some(Value::String(text)) = result.get_mut(format.as_str())
            && let Some((index, _)) = text.char_indices().nth(max_chars)
        {
            text.truncate(index);
            cut = true;
        }
    }
    if cut {
        push_warning(
            result,
            "content_truncated",
            format!("Content truncated to {max_chars} characters (maxChars)"),
        );
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
    if let Some(status) = connection.page.http_status {
        apply_http_status(&mut result, status);
    }
    truncate_content(&mut result, request.max_chars);
    // Best effort. The page is already extracted; a failed dispose must not
    // drop it. Closing the socket still runs disposeOnDetach. Leaving the
    // socket out of the pool forces the next checkout to reconnect.
    if connection
        .send(
            "Target.disposeBrowserContext",
            json!({"browserContextId": target.context}),
            "",
            deadline,
        )
        .await
        .is_err()
    {
        connection.ws = None;
    }
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
        assert!(connection.page.http_status.is_none());
        assert_eq!(
            connection.page.early_document_statuses,
            [("child".to_string(), 503)]
        );
        // Fractional statuses never become statusCode; integers under 100 or
        // over 599 are ignored too.
        connection.observe(&event("main", json!(99)));
        connection.observe(&event("main", json!(600)));
        connection.observe(&event("main", json!(201)));
        assert!(connection.page.http_status.is_none());
        connection.page.main_frame = "child".to_string();
        connection.observe(&event("child", json!(404)));
        assert_eq!(connection.page.http_status, Some(404));
        // Subframe responses never overwrite the document.
        connection.observe(&event("main", json!(500)));
        assert_eq!(connection.page.http_status, Some(404));
        // Obscura reports iframe documents with the page's frame id; the
        // navigation loader's request id is what separates them.
        let subframe = json!({
            "method": "Network.responseReceived",
            "sessionId": "session",
            "params": {
                "type": "Document", "frameId": "child",
                "requestId": "fetch-1", "loaderId": "loader-1",
                "response": {"status": 503},
            },
        });
        connection.observe(&subframe);
        assert_eq!(connection.page.http_status, Some(404));
        let document = json!({
            "method": "Network.responseReceived",
            "sessionId": "session",
            "params": {
                "type": "Document", "frameId": "child",
                "requestId": "loader-1", "loaderId": "loader-1",
                "response": {"status": 200},
            },
        });
        connection.observe(&document);
        assert_eq!(connection.page.http_status, Some(200));
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
            max_chars: 0,
            markdown_links: "full",
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
        assert_eq!(good["metadata"]["statusCode"], 503);
        assert!(good["metadata"].get("httpStatus").is_none());
        assert_eq!(good["warnings"][0]["code"], "http_error");
        let mut long = json!({"markdown": "héllo wörld", "text": "ok"});
        truncate_content(&mut long, 4);
        assert_eq!(long["markdown"], "héll");
        assert_eq!(long["text"], "ok");
        assert_eq!(long["warnings"][0]["code"], "content_truncated");
        let mut short = json!({"markdown": "hi"});
        truncate_content(&mut short, 4);
        assert!(short.get("warnings").is_none());
    }

    #[tokio::test]
    async fn wait_for_clamps_inside_the_remaining_budget() {
        let request = PageRequest {
            url: "https://example.com".to_string(),
            formats: vec![Format::Markdown],
            only_main_content: true,
            timeout: Duration::from_secs(30),
            wait_for: Duration::from_secs(30),
            wait_for_selector: String::new(),
            wait_for_text: String::new(),
            max_chars: 0,
            markdown_links: "full",
        };
        let started = Instant::now();
        let deadline = started + Duration::from_millis(20);
        wait_after_load(&request, deadline, &Stop::channel())
            .await
            .expect("waitFor is clamped to the remaining budget");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn load_events_require_the_navigation_main_frame() {
        let mut connection = Connection::new(Stop::channel());
        connection.active_session = "session".to_string();
        let load = json!({
            "method": "Page.loadEventFired",
            "sessionId": "session",
        });
        connection.observe(&load);
        assert!(
            !connection.page.loaded,
            "about:blank load before navigate must not finish the wait"
        );
        connection.observe(&json!({
            "method": "Page.lifecycleEvent",
            "sessionId": "session",
            "params": {"name": "load", "frameId": "main"},
        }));
        assert!(!connection.page.loaded);
        connection.page.main_frame = "main".to_string();
        connection.observe(&json!({
            "method": "Page.lifecycleEvent",
            "sessionId": "session",
            "params": {"name": "load", "frameId": "child"},
        }));
        assert!(!connection.page.loaded);
        connection.observe(&json!({
            "method": "Page.lifecycleEvent",
            "sessionId": "session",
            "params": {"name": "load", "frameId": "main"},
        }));
        assert!(connection.page.loaded);
        connection.page.loaded = false;
        connection.observe(&load);
        assert!(connection.page.loaded);
    }

    #[test]
    fn loads_before_the_navigate_reply_apply_to_the_document_frame() {
        let mut connection = Connection::new(Stop::channel());
        connection.active_session = "session".to_string();
        for frame in ["child", "main"] {
            connection.observe(&json!({
                "method": "Page.lifecycleEvent",
                "sessionId": "session",
                "params": {"name": "load", "frameId": frame},
            }));
        }
        // Obscura's bare load event cannot be attributed to a frame.
        connection.observe(&json!({
            "method": "Page.loadEventFired",
            "sessionId": "session",
        }));
        assert!(!connection.page.loaded, "no frame id is known yet");
        assert_eq!(connection.page.early_loads, ["child", "main"]);
        connection.page.main_frame = "main".to_string();
        let early = connection
            .page
            .early_loads
            .iter()
            .any(|frame| frame == &connection.page.main_frame);
        connection.page.early_loads.clear();
        assert!(early, "the document frame's early load must be applied");
        assert!(
            !connection.page.loaded,
            "the parser applies the flag; observe only buffers"
        );
    }

    #[test]
    fn only_a_transport_failure_reaps_the_browser() {
        assert!(ScrapeError::Transport(DISCONNECTED.to_string()).reaps_browser());
        assert!(ScrapeError::Transport("Cannot write CDP command".to_string()).reaps_browser());
        assert!(
            ScrapeError::Transport("Malformed CDP response envelope".to_string()).reaps_browser()
        );
        assert!(
            ScrapeError::Transport("Invalid CDP JSON: recursion limit exceeded".to_string())
                .reaps_browser()
        );
        assert!(
            ScrapeError::Transport("CDP response must be an object".to_string()).reaps_browser()
        );
        assert!(!ScrapeError::Page("Content readiness timed out".to_string()).reaps_browser());
        assert!(
            !ScrapeError::Page("waitFor exceeds remaining operation timeout".to_string())
                .reaps_browser()
        );
        assert!(!ScrapeError::Page(TIMED_OUT.to_string()).reaps_browser());
        assert!(!ScrapeError::Page(STOPPING.to_string()).reaps_browser());
        assert!(!ScrapeError::Page(STOPPED.to_string()).reaps_browser());
        assert!(!ScrapeError::Page(TOO_LARGE.to_string()).reaps_browser());
        assert!(!ScrapeError::Cdp("Runtime.evaluate: broken".to_string()).reaps_browser());
    }
}
