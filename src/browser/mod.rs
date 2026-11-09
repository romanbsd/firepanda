//! Browser handle: managed Lightpanda child, CDP connection pool, idle
//! shutdown. Process state stays behind one async mutex; free connections are
//! shared with page workers.

mod cdp;

use cdp::{Connection, ScrapeError};
use serde::Serialize;
use serde_json::Value;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, watch};

/// A connected CDP WebSocket over the Unix socket.
type CdpStream = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;

/// Reconnect window for a managed browser that is still coming up; bounded by
/// the scrape's own deadline as well (C++ `checkout` argument).
const CONNECT_WINDOW: Duration = Duration::from_secs(10);
/// One connect+handshake attempt: C++ sets 100 ms connection/read timeouts.
const CONNECT_ATTEMPT: Duration = Duration::from_millis(100);
/// Pause between managed connect retries (C++ sleeps 20 ms).
const CONNECT_RETRY: Duration = Duration::from_millis(20);

const STOPPING: &str = "Service is stopping";

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

/// Closed set of extraction formats, serialized with the wire names the
/// in-page extractor expects. Keeping this typed prevents invalid formats from
/// reaching a page and makes the "links" checks plain enum comparisons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Format {
    #[serde(rename = "markdown")]
    Markdown,
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "html")]
    Html,
    #[serde(rename = "rawHtml")]
    RawHtml,
    #[serde(rename = "links")]
    Links,
}

impl Format {
    pub const ALL: [Self; 5] = [
        Self::Markdown,
        Self::Text,
        Self::Html,
        Self::RawHtml,
        Self::Links,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Html => "html",
            Self::RawHtml => "rawHtml",
            Self::Links => "links",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|format| format.as_str() == value)
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated once at the tool boundary, then reused for each page of a crawl.
#[derive(Debug, Clone)]
pub struct PageRequest {
    pub url: String,
    pub formats: Vec<Format>,
    pub only_main_content: bool,
    pub timeout: Duration,
    pub wait_for: Duration,
    pub wait_for_selector: String,
    pub wait_for_text: String,
}

/// Shutdown flag shared by the stdio loop, connect retries, deadline reads,
/// and waitFor sleeps. The C++ self-pipe + `stop_requested` atomic plays this
/// role; a watch channel wakes every waiter at once.
#[derive(Clone)]
pub struct Stop {
    sender: Arc<watch::Sender<bool>>,
    receiver: watch::Receiver<bool>,
}

impl Stop {
    pub fn channel() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self {
            sender: Arc::new(sender),
            receiver,
        }
    }

    /// Called from the SIGTERM/SIGINT task (C++ `interrupt`).
    pub fn notify(&self) {
        let _ = self.sender.send(true);
    }

    pub fn stopped(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolve once shutdown is requested. Each waiter uses its own receiver
    /// clone so one waiter consuming the notification cannot starve another.
    pub async fn wait(&self) {
        let mut receiver = self.receiver.clone();
        let _ = receiver.changed().await;
    }
}

/// Child process, socket directory, and idle bookkeeping. C++ touched this
/// only on the main thread between requests; the async mutex covers the same
/// exclusivity while launch/connect run concurrently in M4.
struct Process {
    options: Options,
    directory: String,
    child: Option<Child>,
    idle_since: Option<Instant>,
}

struct Inner {
    process: Mutex<Process>,
    /// Idle connections between page fetches; returned through `give_back`.
    free: Mutex<Vec<Connection>>,
    stop: Stop,
    /// False for `--socket`: an external peer is never spawned or killed.
    owns_process: bool,
}

pub struct Browser {
    inner: Arc<Inner>,
}

/// In-flight crawl page. `Failed` means the dispatch-thread checkout failed
/// and no worker ran; `Running` owns a pooled connection until it resolves.
pub enum PageTask {
    Failed(String),
    Running(tokio::task::JoinHandle<Result<Value, String>>),
}

impl Browser {
    pub fn new(options: Options, stop: Stop) -> Self {
        let owns_process = options.socket.is_empty();
        Self {
            inner: Arc::new(Inner {
                process: Mutex::new(Process {
                    options,
                    directory: String::new(),
                    child: None,
                    idle_since: None,
                }),
                free: Mutex::new(Vec::new()),
                stop,
                owns_process,
            }),
        }
    }

    /// One page fetch. Errors release owned resources (the connection is
    /// dropped by `fetch`, the pool and child by `shutdown`); external sockets
    /// survive so the next scrape reconnects fresh.
    pub async fn scrape(&self, request: &PageRequest) -> Result<Value, String> {
        let (deadline, connect_deadline) = self.begin(request.timeout).await;
        let outcome = self.fetch(request, deadline, connect_deadline).await;
        if outcome.is_err() {
            // disposeOnDetach releases this operation's context even if its
            // CDP stream is unusable; sending another command on a failed
            // stream can hang cleanup.
            self.shutdown().await;
        }
        outcome
    }

    /// Clear idle bookkeeping and derive the page and connect deadlines.
    /// Idle starts when work completes, not when it starts.
    async fn begin(&self, timeout: Duration) -> (Instant, Instant) {
        self.inner.process.lock().await.idle_since = None;
        let start = Instant::now();
        (start + timeout, start + timeout.min(CONNECT_WINDOW))
    }

    async fn fetch(
        &self,
        request: &PageRequest,
        deadline: Instant,
        connect_deadline: Instant,
    ) -> Result<Value, String> {
        let mut connection = self.checkout(connect_deadline).await?;
        let result = cdp::run_scrape(&mut connection, request, deadline)
            .await
            .map_err(ScrapeError::message)?;
        // Only healthy connections return to the pool; an error destroys this
        // one and `shutdown` reaps everything after it returns.
        self.inner.free.lock().await.push(connection);
        // Inactivity starts after work completes, not when it starts.
        self.inner.process.lock().await.idle_since = Some(Instant::now());
        Ok(result)
    }

    /// Fetch one page on a pool connection for the crawl loop (C++
    /// `scrape_async`). The dispatch task checks out inline — launching the
    /// managed browser if needed — so concurrent pages can never race a second
    /// spawn; a checkout failure surfaces as `Failed` without releasing the
    /// browser, which the crawl loop reaps after draining siblings.
    pub async fn start_page(&self, request: PageRequest) -> PageTask {
        let (deadline, connect_deadline) = self.begin(request.timeout).await;
        let connection = match self.checkout(connect_deadline).await {
            Ok(connection) => connection,
            Err(message) => return PageTask::Failed(message),
        };
        let inner = Arc::clone(&self.inner);
        PageTask::Running(tokio::spawn(async move {
            let mut connection = connection;
            let result = cdp::run_scrape(&mut connection, &request, deadline)
                .await
                .map_err(ScrapeError::message);
            // Only healthy connections return to the pool; an error destroys
            // this one and the crawl loop reaps the browser after draining.
            if result.is_ok() {
                inner.free.lock().await.push(connection);
            }
            result
        }))
    }

    /// Inactivity starts after work completes, not when it starts (C++
    /// `page_completed`; main thread only, between dispatch and recording).
    pub async fn page_completed(&self) {
        self.inner.process.lock().await.idle_since = Some(Instant::now());
    }

    /// Stop flag for the crawl loop's dispatch condition (C++
    /// `stop_requested`).
    pub fn stopped(&self) -> bool {
        self.inner.stop.stopped()
    }

    /// Resolve once shutdown is requested.
    pub async fn wait_stopped(&self) {
        self.inner.stop.wait().await;
    }

    /// Pop a pooled connection or open a fresh one (C++ `checkout`).
    async fn checkout(&self, deadline: Instant) -> Result<Connection, String> {
        if self.inner.owns_process {
            // Pooled sockets outlive a crashed child and still look open; drop
            // them so `connect` relaunches instead of failing the next page.
            let alive = self
                .inner
                .process
                .lock()
                .await
                .child
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
            if !alive {
                self.inner.free.lock().await.clear();
            }
        }
        let pooled = self.inner.free.lock().await.pop();
        let mut connection = pooled.unwrap_or_else(|| Connection::new(self.inner.stop.clone()));
        self.connect(&mut connection, deadline).await?;
        Ok(connection)
    }

    /// Connect, launching the managed browser first. Serialized by the
    /// process mutex so concurrent checkouts can never race a second spawn.
    ///
    /// The guard is intentionally held across the connect attempts: releasing
    /// it would let a second dispatch spawn another child or see a socket that
    /// is still being created.
    #[allow(clippy::significant_drop_tightening)]
    async fn connect(&self, connection: &mut Connection, deadline: Instant) -> Result<(), String> {
        if connection.is_open() {
            return Ok(());
        }
        let mut process = self.inner.process.lock().await;
        let owns_process = self.inner.owns_process;
        if owns_process {
            launch(&mut process)?;
        }
        let socket = process.options.socket.clone();
        loop {
            if self.inner.stop.stopped() {
                return Err(STOPPING.to_string());
            }
            // Host header stays localhost; only the connection target is a UDS.
            if let Ok(websocket) = attempt(&socket).await {
                connection.attach(websocket);
                return Ok(());
            }
            if !owns_process {
                break; // External peers get exactly one attempt (C++ `own` check).
            }
            let gone = process
                .child
                .as_mut()
                .is_none_or(|child| !matches!(child.try_wait(), Ok(None)));
            if gone {
                process.child = None;
                return Err("Lightpanda exited before opening its Unix socket".to_string());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                () = self.inner.stop.wait() => {}
                () = tokio::time::sleep(CONNECT_RETRY) => {}
            }
        }
        Err(format!(
            "Cannot connect to Lightpanda Unix socket: {socket}"
        ))
    }

    /// Drop pooled connections, stop an owned child, and remove the private
    /// socket directory. External `--socket` mode keeps its socket path: only
    /// the pool is cleared. Called after failures, on idle expiry, and when
    /// the service exits (C++ `release()`).
    pub async fn shutdown(&self) {
        self.inner.free.lock().await.clear();
        let mut process = self.inner.process.lock().await;
        stop_child(&mut process).await;
        process.idle_since = None;
        if !process.directory.is_empty() {
            let _ = std::fs::remove_dir_all(&process.directory);
            process.directory.clear();
            process.options.socket.clear();
        }
    }

    /// Stop an expired idle browser and return the next deadline. Expiry and
    /// deadline are read under one lock; `shutdown` only re-locks when it runs.
    pub async fn expire_if_idle(&self) -> Option<Instant> {
        let deadline = {
            let process = self.inner.process.lock().await;
            if !self.inner.owns_process
                || process.child.is_none()
                || process.options.idle_timeout.is_zero()
            {
                return None;
            }
            process
                .idle_since
                .map(|since| since + process.options.idle_timeout)
        };
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Called only between requests, never during active CDP work.
            self.shutdown().await;
            return None;
        }
        deadline
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Safety net for skipped teardown; shutdown() already cleared both.
        if let Ok(mut process) = self.inner.process.try_lock() {
            if let Some(mut child) = process.child.take() {
                let _ = child.start_kill();
            }
            if !process.directory.is_empty() {
                let _ = std::fs::remove_dir_all(&process.directory);
            }
        }
    }
}

/// CDP WebSocket message and frame cap. Everything Lightpanda sends is far
/// smaller; bounding the frame/message and write buffer keeps a misbehaving
/// peer from growing memory.
const MAX_WS_MESSAGE: usize = 16 * 1024 * 1024;

/// One connect+handshake attempt against the CDP Unix socket, capped at the
/// C++ connection/read timeout so a stalled peer cannot outlive the loop.
async fn attempt(socket: &str) -> Result<CdpStream, ()> {
    let connect = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|_| ())?;
        // Handshake over our own stream; Host stays localhost like the C++
        // direct hostname/address map. Lightpanda exposes a plain WebSocket,
        // so this is HTTP/1.1 Upgrade over the UDS (HTTP/2 extended CONNECT
        // is not used).
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_WS_MESSAGE))
            .max_frame_size(Some(MAX_WS_MESSAGE))
            .max_write_buffer_size(MAX_WS_MESSAGE);
        let (websocket, _) =
            tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config))
                .await
                .map_err(|_| ())?;
        Ok(websocket)
    };
    tokio::time::timeout(CONNECT_ATTEMPT, connect)
        .await
        .unwrap_or(Err(()))
}

/// One spawn-failure message for both stdout-clone and `spawn` failures.
fn start_error(error: &std::io::Error) -> String {
    format!("Cannot start Lightpanda: {error}; use --lightpanda /path/to/lightpanda")
}

/// Spawn the managed browser: `lightpanda serve --unix-socket … --log-level
/// error`, stdin /dev/null, stdout redirected to stderr so a browser log can
/// never corrupt the MCP byte stream (C++ `posix_spawn` file actions).
fn launch(process: &mut Process) -> Result<(), String> {
    let running = process
        .child
        .as_mut()
        .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
    if running {
        return Ok(()); // Still running; reuse it.
    }
    process.child = None;
    if process.directory.is_empty() {
        let directory = make_socket_directory()?;
        process.options.socket = format!("{directory}/cdp.sock");
        process.directory = directory;
    }
    let socket = process.options.socket.clone();
    let _ = std::fs::remove_file(&socket); // A stale socket would fail the bind.
    let stdout = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| start_error(&error))?;
    let child = Command::new(&process.options.lightpanda)
        .arg("serve")
        .arg("--unix-socket")
        .arg(&socket)
        .arg("--log-level")
        .arg("error")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(false)
        .spawn()
        .map_err(|error| start_error(&error))?;
    process.child = Some(child);
    Ok(())
}

/// SIGTERM, 500 ms to exit, then SIGKILL — C++ `Browser::Impl::stop_child`.
async fn stop_child(process: &mut Process) {
    let Some(mut child) = process.child.take() else {
        return;
    };
    match child.try_wait() {
        Ok(Some(_)) | Err(_) => return, // Already exited (or unreaped); never signal a reused PID.
        Ok(None) => {}
    }
    // A reaped child has no pid; treat that like an already-exited process.
    let Some(pid) = child.id() else {
        return;
    };
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if tokio::time::timeout(Duration::from_millis(500), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// mkdtemp("/tmp/firepanda-XXXXXX"): a private directory only this process
/// puts its socket in (C++ `launch`).
fn make_socket_directory() -> Result<String, String> {
    let mut template = b"/tmp/firepanda-XXXXXX\0".to_vec();
    let created = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if created.is_null() {
        return Err("Cannot create private socket directory".to_string());
    }
    let directory = unsafe { std::ffi::CStr::from_ptr(created) };
    Ok(directory.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_directories_are_unique_and_removable() {
        let first = make_socket_directory().unwrap();
        let second = make_socket_directory().unwrap();
        assert!(first.starts_with("/tmp/firepanda-"));
        assert_ne!(first, second);
        std::fs::remove_dir_all(&first).unwrap();
        std::fs::remove_dir_all(&second).unwrap();
    }

    #[tokio::test]
    async fn stop_wakes_every_waiter_once_notified() {
        let stop = Stop::channel();
        assert!(!stop.stopped());
        let waiter = {
            let stop = stop.clone();
            tokio::spawn(async move { stop.wait().await })
        };
        stop.notify();
        stop.wait().await; // A second waiter must not be starved.
        waiter.await.unwrap();
        assert!(stop.stopped());
        stop.wait().await; // Already stopped: resolves immediately.
    }

    #[test]
    fn format_wire_names_round_trip() {
        for format in Format::ALL {
            assert_eq!(Format::parse(format.as_str()), Some(format));
            assert_eq!(format.to_string(), format.as_str());
            assert_eq!(
                serde_json::to_value(format).expect("format serializes"),
                serde_json::Value::String(format.as_str().to_string())
            );
        }
        assert_eq!(Format::parse("pdf"), None);
    }

    #[tokio::test]
    async fn external_socket_never_spawns_or_cleans_the_socket() {
        let browser = Browser::new(
            Options {
                socket: "/tmp/unused-cdp.sock".to_string(),
                ..Options::default()
            },
            Stop::channel(),
        );
        // Fail fast: external peers get one attempt (connection refused).
        let error = browser
            .scrape(&PageRequest {
                url: "https://example.com".to_string(),
                formats: vec![Format::Markdown],
                only_main_content: true,
                timeout: Duration::from_millis(500),
                wait_for: Duration::ZERO,
                wait_for_selector: String::new(),
                wait_for_text: String::new(),
            })
            .await
            .unwrap_err();
        assert!(
            error.contains("Cannot connect to Lightpanda Unix socket"),
            "{error}"
        );
        let process = browser.inner.process.lock().await;
        assert_eq!(process.options.socket, "/tmp/unused-cdp.sock");
        assert_eq!(process.directory, "");
        assert!(process.child.is_none());
        drop(process);
    }

    #[tokio::test]
    async fn missing_managed_binary_reports_spawn_failure() {
        let browser = Browser::new(
            Options {
                lightpanda: "/missing/lightpanda".to_string(),
                ..Options::default()
            },
            Stop::channel(),
        );
        let error = browser
            .scrape(&PageRequest {
                url: "https://example.com".to_string(),
                formats: vec![Format::Markdown],
                only_main_content: true,
                timeout: Duration::from_secs(5),
                wait_for: Duration::ZERO,
                wait_for_selector: String::new(),
                wait_for_text: String::new(),
            })
            .await
            .unwrap_err();
        assert!(error.contains("Cannot start Lightpanda"), "{error}");
        // The failed launch's private directory is removed by shutdown.
        let process = browser.inner.process.lock().await;
        assert_eq!(process.directory, "");
        assert!(process.child.is_none());
        drop(process);
    }
}
