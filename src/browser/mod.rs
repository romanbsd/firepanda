//! Browser handle: managed browser child (Lightpanda or Obscura), CDP
//! connection pool, idle shutdown. Process state stays behind one async mutex;
//! free connections are shared with page workers.

mod cdp;

use cdp::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, watch};

/// Either end of a CDP WebSocket: a Unix socket (Lightpanda) or loopback TCP
/// (Obscura, external endpoints).
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

/// A connected CDP WebSocket. Boxing the stream keeps the CDP client independent
/// of the transport; one allocation per connection is noise next to a browser.
pub type CdpStream = tokio_tungstenite::WebSocketStream<Box<dyn AsyncReadWrite>>;

/// Reconnect window for a managed browser that is still coming up; bounded by
/// the scrape's own deadline as well.
const CONNECT_WINDOW: Duration = Duration::from_secs(10);
/// One connect+handshake attempt.
const CONNECT_ATTEMPT: Duration = Duration::from_millis(100);
/// Pause between managed connect retries.
const CONNECT_RETRY: Duration = Duration::from_millis(20);

const STOPPING: &str = "Service is stopping";

/// Maximum page budget accepted by the tools (`timeout` in `src/tools.rs`).
/// Obscura caps in-page script execution per process, so raise that cap to the
/// largest page budget instead of silently truncating extraction.
pub const MAX_PAGE_TIMEOUT_MS: i64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Lightpanda,
    Obscura,
}

impl Backend {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "lightpanda" => Some(Self::Lightpanda),
            "obscura" => Some(Self::Obscura),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Lightpanda => "Lightpanda",
            Self::Obscura => "Obscura",
        }
    }

    pub const fn default_program(self) -> &'static str {
        match self {
            Self::Lightpanda => "lightpanda",
            Self::Obscura => "obscura",
        }
    }
}

/// Where the CDP client connects. Lightpanda exposes a plain WebSocket over a
/// private Unix socket; Obscura only listens on loopback TCP and guards the
/// control plane with a bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix { path: String },
    Tcp { url: String, token: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Options {
    pub backend: Backend,
    /// Browser executable; `--browser` or the backend-specific alias.
    pub program: String,
    /// External Lightpanda Unix socket; empty for a managed browser.
    pub socket: String,
    /// External CDP WebSocket URL; empty for a managed browser.
    pub cdp_url: String,
    /// Zero keeps the managed browser warm indefinitely.
    pub idle_timeout: Duration,
    pub stealth: bool,
    pub obey_robots: bool,
    /// Obscura blocks loopback/RFC1918/link-local by default. Firepanda keeps
    /// Lightpanda's behaviour (local sites are scrapeable) unless disabled.
    pub allow_private_network: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            backend: Backend::Lightpanda,
            program: Backend::Lightpanda.default_program().to_string(),
            socket: String::new(),
            cdp_url: String::new(),
            idle_timeout: Duration::from_secs(60),
            stealth: false,
            obey_robots: false,
            allow_private_network: true,
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
    /// Per-format character cap; 0 means no limit.
    pub max_chars: usize,
    /// Markdown link targets: "full", "relative", or "none".
    pub markdown_links: &'static str,
}

/// Shutdown flag shared by the stdio loop, connect retries, deadline reads,
/// and waitFor sleeps; a watch channel wakes every waiter at once.
#[derive(Clone)]
pub struct Stop(Arc<watch::Sender<bool>>);

impl Stop {
    pub fn channel() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Called from the SIGTERM/SIGINT task. `send_replace` stores the flag
    /// even while no waiter is subscribed.
    pub fn notify(&self) {
        self.0.send_replace(true);
    }

    pub fn stopped(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolve once shutdown is requested; returns at once if it already was.
    pub async fn wait(&self) {
        let _ = self.0.subscribe().wait_for(|&stopped| stopped).await;
    }
}

/// Child process, endpoint, and idle bookkeeping, behind one async mutex so
/// concurrent crawl checkouts never race a launch or connect.
struct Process {
    options: Options,
    directory: String,
    child: Option<Child>,
    endpoint: Option<Endpoint>,
    /// Obscura control-plane bearer token; None for Lightpanda.
    token: Option<String>,
    idle_since: Option<Instant>,
}

struct Inner {
    process: Mutex<Process>,
    /// Idle connections between page fetches; returned through `give_back`.
    free: Mutex<Vec<Connection>>,
    stop: Stop,
    /// False for `--socket`/`--cdp-url`: an external peer is never spawned or
    /// killed.
    owns_process: bool,
}

pub struct Browser {
    inner: Arc<Inner>,
}

/// Page failure that still says whether the browser process is reusable.
#[derive(Debug)]
pub struct PageError {
    pub message: String,
    pub reap: bool,
}

/// In-flight crawl page. `Failed` means the dispatch-thread checkout failed
/// and no worker ran; `Running` owns a pooled connection until it resolves.
pub enum PageTask {
    Failed(String),
    Running(tokio::task::JoinHandle<Result<Value, PageError>>),
}

impl Browser {
    pub fn new(options: Options, stop: Stop) -> Self {
        let owns_process = options.socket.is_empty() && options.cdp_url.is_empty();
        let endpoint = if !options.cdp_url.is_empty() {
            Some(Endpoint::Tcp {
                url: options.cdp_url.clone(),
                token: None,
            })
        } else if !options.socket.is_empty() {
            Some(Endpoint::Unix {
                path: options.socket.clone(),
            })
        } else {
            None
        };
        Self {
            inner: Arc::new(Inner {
                process: Mutex::new(Process {
                    options,
                    directory: String::new(),
                    child: None,
                    endpoint,
                    token: None,
                    idle_since: None,
                }),
                free: Mutex::new(Vec::new()),
                stop,
                owns_process,
            }),
        }
    }

    /// One page fetch. A dead CDP connection reaps the pool and the owned
    /// child. A page error (readiness, validation, a rejected command) drops
    /// that connection and leaves the browser up.
    pub async fn scrape(&self, request: &PageRequest) -> Result<Value, String> {
        let (deadline, connect_deadline) = self.begin(request.timeout).await;
        match self.fetch(request, deadline, connect_deadline).await {
            Ok(value) => Ok(value),
            Err(error) => {
                if error.reap {
                    // disposeOnDetach releases this operation's context even if
                    // its CDP stream is unusable; sending another command on a
                    // failed stream can hang cleanup.
                    self.shutdown().await;
                }
                Err(error.message)
            }
        }
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
    ) -> Result<Value, PageError> {
        let mut connection =
            self.checkout(connect_deadline)
                .await
                .map_err(|message| PageError {
                    message,
                    reap: true,
                })?;
        let result = match cdp::run_scrape(&mut connection, request, deadline).await {
            Ok(result) => result,
            Err(error) => {
                return Err(PageError {
                    reap: error.reaps_browser(),
                    message: error.message(),
                });
            }
        };
        // A dispose failure has already dropped the socket. Pooling that
        // connection makes the next checkout reconnect instead of reusing it.
        self.inner.free.lock().await.push(connection);
        // Inactivity starts after work completes, not when it starts.
        self.inner.process.lock().await.idle_since = Some(Instant::now());
        Ok(result)
    }

    /// Fetch one page on a pool connection for the crawl loop. The dispatch task checks out inline — launching the
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
            let result = match cdp::run_scrape(&mut connection, &request, deadline).await {
                Ok(result) => Ok(result),
                Err(error) => Err(PageError {
                    reap: error.reaps_browser(),
                    message: error.message(),
                }),
            };
            // Only a finished page returns its connection. An error drops this
            // socket; the crawl loop reaps the browser only when `reap` is set.
            if result.is_ok() {
                inner.free.lock().await.push(connection);
            }
            result
        }))
    }

    /// Inactivity starts after work completes, not when it starts. Called by
    /// the crawl loop between dispatch and recording.
    pub async fn page_completed(&self) {
        self.inner.process.lock().await.idle_since = Some(Instant::now());
    }

    /// Stop flag for the crawl loop's dispatch condition.
    pub fn stopped(&self) -> bool {
        self.inner.stop.stopped()
    }

    /// Resolve once shutdown is requested.
    pub async fn wait_stopped(&self) {
        self.inner.stop.wait().await;
    }

    /// Pop a pooled connection or open a fresh one.
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
        loop {
            if self.inner.stop.stopped() {
                return Err(STOPPING.to_string());
            }
            // Obscura publishes its bound port in a ready file after V8 starts;
            // Lightpanda's endpoint exists as soon as it is spawned.
            resolve_endpoint(&mut process);
            if let Some(endpoint) = process.endpoint.clone()
                && let Ok(websocket) = attempt(&endpoint).await
            {
                connection.attach(websocket);
                return Ok(());
            }
            if !owns_process {
                break; // External peers get exactly one attempt.
            }
            let gone = process
                .child
                .as_mut()
                .is_none_or(|child| !matches!(child.try_wait(), Ok(None)));
            if gone {
                process.child = None;
                return Err(format!(
                    "{} exited before becoming ready",
                    process.options.backend.label()
                ));
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                () = self.inner.stop.wait() => {}
                () = tokio::time::sleep(CONNECT_RETRY) => {}
            }
        }
        Err(connect_error(&process))
    }

    /// Drop pooled connections, stop an owned child, and remove the private
    /// endpoint directory. External `--socket`/`--cdp-url` mode keeps its
    /// endpoint: only the pool is cleared. Called after failures, on idle
    /// expiry, and when the service exits.
    pub async fn shutdown(&self) {
        self.inner.free.lock().await.clear();
        let mut process = self.inner.process.lock().await;
        stop_child(&mut process).await;
        process.idle_since = None;
        if !process.directory.is_empty() {
            let _ = std::fs::remove_dir_all(&process.directory);
            process.directory.clear();
            process.endpoint = None;
            process.token = None;
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

/// One connect+handshake attempt against the CDP endpoint, capped at the
/// `CONNECT_ATTEMPT` so a stalled peer cannot outlive the loop.
async fn attempt(endpoint: &Endpoint) -> Result<CdpStream, ()> {
    let connect = async {
        let (stream, url, token): (Box<dyn AsyncReadWrite>, &str, Option<&str>) = match endpoint {
            Endpoint::Unix { path } => {
                let stream = UnixStream::connect(path).await.map_err(|_| ())?;
                // Lightpanda exposes a plain WebSocket at `/`; the Host header
                // stays localhost because this is HTTP/1.1 Upgrade over a UDS.
                (Box::new(stream), "ws://localhost/", None)
            }
            Endpoint::Tcp { url, token } => {
                let parsed = url::Url::parse(url).map_err(|_| ())?;
                let host = parsed.host_str().ok_or(())?;
                let port = parsed.port_or_known_default().ok_or(())?;
                let stream = TcpStream::connect((host, port)).await.map_err(|_| ())?;
                (Box::new(stream), url.as_str(), token.as_deref())
            }
        };
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_WS_MESSAGE))
            .max_frame_size(Some(MAX_WS_MESSAGE))
            .max_write_buffer_size(MAX_WS_MESSAGE);
        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
                .map_err(|_| ())?;
        if let Some(token) = token {
            let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
                "Bearer {token}"
            ))
            .map_err(|_| ())?;
            request.headers_mut().insert("authorization", value);
        }
        let (websocket, _) =
            tokio_tungstenite::client_async_with_config(request, stream, Some(config))
                .await
                .map_err(|_| ())?;
        Ok(websocket)
    };
    tokio::time::timeout(CONNECT_ATTEMPT, connect)
        .await
        .unwrap_or(Err(()))
}

/// One spawn-failure message for both stdout-clone and `spawn` failures.
fn start_error(process: &Process, error: &std::io::Error) -> String {
    format!(
        "Cannot start {}: {error}; use --browser /path/to/{}",
        process.options.backend.label(),
        process.options.backend.default_program()
    )
}

/// What the managed browser must do before the pool can connect.
fn connect_error(process: &Process) -> String {
    match &process.endpoint {
        Some(Endpoint::Unix { path }) => format!(
            "Cannot connect to {} Unix socket: {path}",
            process.options.backend.label()
        ),
        Some(Endpoint::Tcp { url, .. }) => format!(
            "Cannot connect to {} CDP endpoint: {url}",
            process.options.backend.label()
        ),
        None => format!(
            "{} did not publish a ready file",
            process.options.backend.label()
        ),
    }
}

/// Read and parse a small JSON record from the private directory.
fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Read Obscura's atomically published `{pid, host, port, websocket_path}`.
fn read_ready(path: &std::path::Path) -> Option<ReadyRecord> {
    let ready: ReadyRecord = read_json(path)?;
    let host_ok = matches!(ready.host.as_str(), "127.0.0.1" | "::1" | "localhost");
    let path_ok = ready.websocket_path.starts_with('/') && ready.websocket_path.len() <= 256;
    (ready.port != 0 && host_ok && path_ok).then_some(ready)
}

/// `host:port`, bracketed for IPv6 literals.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Resolve a managed Obscura endpoint once its ready file appears. The record
/// must name the child this process spawned, so a stale file cannot redirect
/// the pool.
fn resolve_endpoint(process: &mut Process) {
    if process.endpoint.is_some() {
        return;
    }
    let path = std::path::Path::new(&process.directory).join(READY_FILE);
    let Some(ready) = read_ready(&path) else {
        return;
    };
    if process.child.as_ref().and_then(Child::id) != Some(ready.pid) {
        return;
    }
    let url = format!("ws://{}", ready.url());
    process.endpoint = Some(Endpoint::Tcp {
        url,
        token: process.token.clone(),
    });
}

/// 32 random bytes, hex encoded. `OBSCURA_CDP_TOKEN` requires at least 32.
fn random_token() -> Result<String, String> {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| "Cannot generate an Obscura CDP token".to_string())?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

/// Spawn the managed browser and publish its endpoint. Lightpanda gets a
/// private Unix socket; Obscura gets an ephemeral loopback port, a bearer
/// token, and a ready file. stdin /dev/null and stdout redirected to stderr so
/// a browser log can never corrupt the MCP byte stream.
fn launch(process: &mut Process) -> Result<(), String> {
    let running = process
        .child
        .as_mut()
        .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
    if running {
        return Ok(()); // Still running; reuse it.
    }
    process.child = None;
    process.endpoint = None;
    process.token = None;
    if process.directory.is_empty() {
        sweep_orphans(std::path::Path::new(SOCKET_ROOT));
        process.directory = make_socket_directory()?;
    }
    let stdout = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| start_error(process, &error))?;
    let mut command = Command::new(&process.options.program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(false);
    match process.options.backend {
        Backend::Lightpanda => {
            let socket = format!("{}/{SOCKET_FILE}", process.directory);
            let _ = std::fs::remove_file(&socket); // A stale socket would fail the bind.
            command
                .arg("serve")
                .arg("--unix-socket")
                .arg(&socket)
                .arg("--log-level")
                .arg("error");
            process.endpoint = Some(Endpoint::Unix { path: socket });
        }
        Backend::Obscura => {
            let ready = format!("{}/{READY_FILE}", process.directory);
            let _ = std::fs::remove_file(&ready);
            let token = random_token()?;
            command
                .arg("serve")
                .arg("--host")
                .arg("127.0.0.1")
                .arg("--port")
                .arg("0")
                .arg("--ready-file")
                .arg(&ready)
                .arg("--workers")
                .arg("1");
            if process.options.stealth {
                command.arg("--stealth");
            }
            if process.options.obey_robots {
                command.arg("--obey-robots");
            }
            if process.options.allow_private_network {
                command.arg("--allow-private-network");
            }
            command.env("OBSCURA_CDP_TOKEN", &token).env(
                "OBSCURA_SCRIPT_DEADLINE_MS",
                MAX_PAGE_TIMEOUT_MS.to_string(),
            );
            process.token = Some(token);
        }
    }
    let child = command
        .spawn()
        .map_err(|error| start_error(process, &error))?;
    if let Some(pid) = child.id() {
        write_owner(process, pid);
    }
    process.child = Some(child);
    Ok(())
}

/// Record ownership so a later firepanda can reap this browser if we die
/// without cleanup. Best effort: a missing file only skips that sweep.
fn write_owner(process: &Process, pid: u32) {
    let backend = match process.options.backend {
        Backend::Lightpanda => "lightpanda",
        Backend::Obscura => "obscura",
    };
    let record = OwnerRecord {
        parent: i32::try_from(std::process::id()).unwrap_or(i32::MAX),
        browser: i32::try_from(pid).unwrap_or(i32::MAX),
        backend: backend.to_string(),
        token: process.token.clone(),
    };
    if let Ok(text) = serde_json::to_string(&record) {
        let _ = std::fs::write(format!("{}/{OWNER_FILE}", process.directory), text);
    }
}

const SOCKET_ROOT: &str = "/tmp";
const SOCKET_FILE: &str = "cdp.sock";
const READY_FILE: &str = "cdp.json";
const OWNER_FILE: &str = "owner.json";

#[derive(Debug, Deserialize)]
struct ReadyRecord {
    pid: u32,
    host: String,
    port: u16,
    websocket_path: String,
}

impl ReadyRecord {
    fn url(&self) -> String {
        format!(
            "{}{}",
            authority(&self.host, self.port),
            self.websocket_path
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct OwnerRecord {
    parent: i32,
    browser: i32,
    backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

fn read_owner(path: &std::path::Path) -> Option<OwnerRecord> {
    read_json(path)
}

/// Reap browsers left by a firepanda that died without cleanup (SIGKILL,
/// abort): each private directory names its owner, the browser PID, and the
/// backend. Only dead owners are swept, and the browser is signalled only
/// after proving the recorded endpoint is still live, which shows the PID has
/// not been reused.
/// ponytail: runs only when a browser launches; Linux `PR_SET_PDEATHSIG` would
/// reap immediately but does not exist on macOS.
fn sweep_orphans(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("firepanda-")
        {
            continue;
        }
        let path = entry.path();
        let Some(owner) = read_owner(&path.join(OWNER_FILE)) else {
            continue; // Not ours, still starting, or from an older version.
        };
        if owner.parent <= 0 || owner.browser <= 0 || process_exists(owner.parent) {
            continue;
        }
        if browser_is_live(&path, &owner) {
            unsafe {
                libc::kill(owner.browser, libc::SIGTERM);
            }
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Prove the recorded browser still owns the recorded endpoint.
fn browser_is_live(directory: &std::path::Path, owner: &OwnerRecord) -> bool {
    match owner.backend.as_str() {
        "lightpanda" => {
            std::os::unix::net::UnixStream::connect(directory.join(SOCKET_FILE)).is_ok()
        }
        "obscura" => {
            let Some(ready) = read_ready(&directory.join(READY_FILE)) else {
                return false;
            };
            u32::try_from(owner.browser).is_ok_and(|pid| pid == ready.pid)
                && probe_json_version(&ready.host, ready.port, owner.token.as_deref())
        }
        _ => false,
    }
}

/// Authenticated `GET /json/version` against the recorded loopback port.
fn probe_json_version(host: &str, port: u16, token: Option<&str>) -> bool {
    use std::io::Write as _;
    use std::net::{SocketAddr, TcpStream as StdTcpStream};

    let authority = authority(host, port);
    let Ok(address) = authority.parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = StdTcpStream::connect_timeout(&address, Duration::from_millis(200)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET /json/version HTTP/1.1\r\nHost: {authority}\r\n{auth}Connection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0u8; 4096];
    let Ok(read) = stream.read(&mut response) else {
        return false;
    };
    String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.")
}

/// `kill(pid, 0)`: only ESRCH proves the process is gone (EPERM means it
/// exists under another user).
fn process_exists(pid: libc::pid_t) -> bool {
    let status = unsafe { libc::kill(pid, 0) };
    status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// SIGTERM, 500 ms to exit, then SIGKILL.
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

/// mkdtemp("/tmp/firepanda-XXXXXX") under `SOCKET_ROOT`: a private directory only this process
/// puts its socket in.
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

    #[test]
    fn sweep_removes_only_directories_of_dead_owners() {
        let root = make_socket_directory().unwrap();
        let root = std::path::Path::new(&root);
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let dead = exited.id();
        exited.wait().unwrap();
        let live = std::process::id();
        let record = |parent, browser| {
            serde_json::to_string(&OwnerRecord {
                parent,
                browser,
                backend: "lightpanda".to_string(),
                token: None,
            })
            .unwrap()
        };
        for (name, owner) in [
            (
                "firepanda-dead",
                Some(record(dead.cast_signed(), dead.cast_signed())),
            ),
            (
                "firepanda-live",
                Some(record(live.cast_signed(), dead.cast_signed())),
            ),
            ("firepanda-unowned", None),
        ] {
            std::fs::create_dir(root.join(name)).unwrap();
            if let Some(owner) = owner {
                std::fs::write(root.join(name).join(OWNER_FILE), owner).unwrap();
            }
        }
        sweep_orphans(root);
        assert!(!root.join("firepanda-dead").exists());
        assert!(root.join("firepanda-live").exists());
        assert!(root.join("firepanda-unowned").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backend_names_round_trip() {
        assert_eq!(Backend::parse("lightpanda"), Some(Backend::Lightpanda));
        assert_eq!(Backend::parse("obscura"), Some(Backend::Obscura));
        assert_eq!(Backend::parse("auto"), None);
        assert_eq!(Backend::parse("chrome"), None);
        assert_eq!(Backend::Obscura.label(), "Obscura");
        assert_eq!(Backend::Obscura.default_program(), "obscura");
    }

    #[test]
    fn ready_record_requires_loopback_and_a_valid_path() {
        let directory = make_socket_directory().unwrap();
        let path = std::path::Path::new(&directory).join(READY_FILE);
        for invalid in [
            r#"{"pid":1,"host":"10.0.0.5","port":9222,"websocket_path":"/devtools/browser"}"#,
            r#"{"pid":1,"host":"127.0.0.1","port":0,"websocket_path":"/devtools/browser"}"#,
            r#"{"pid":1,"host":"127.0.0.1","port":9222,"websocket_path":"devtools"}"#,
            r#"{"host":"127.0.0.1","port":9222,"websocket_path":"/devtools/browser"}"#,
            "not json",
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(read_ready(&path).is_none(), "{invalid}");
        }
        std::fs::write(
            &path,
            r#"{"pid":1,"host":"127.0.0.1","port":9222,"websocket_path":"/devtools/browser"}"#,
        )
        .unwrap();
        let ready = read_ready(&path).expect("valid ready record");
        assert_eq!((ready.port, ready.pid), (9222, 1));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn obscura_tokens_are_32_bytes_of_hex() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(token, random_token().unwrap());
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
                max_chars: 0,
                markdown_links: "full",
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
                program: "/missing/lightpanda".to_string(),
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
                max_chars: 0,
                markdown_links: "full",
            })
            .await
            .unwrap_err();
        assert!(error.contains("Cannot start Lightpanda"), "{error}"); // The failed launch's private directory is removed by shutdown.
        let process = browser.inner.process.lock().await;
        assert_eq!(process.directory, "");
        assert!(process.child.is_none());
        drop(process);
    }
}
