//! Browser handle: managed Lightpanda child, CDP connection pool, idle
//! shutdown. Ported from `src/browser.cpp`: process state stays behind one
//! async mutex (the C++ code restricted it to the main thread), free
//! connections are shared with page workers (M4).

mod cdp;

use cdp::{Connection, ScrapeError};
use serde_json::Value;
use std::os::fd::FromRawFd;
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

/// Validated once at the tool boundary, then reused for each page of a crawl.
#[derive(Debug, Clone)]
pub struct PageRequest {
    pub url: String,
    pub formats: Vec<String>,
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
    /// Idle connections between page fetches; returned through give_back.
    free: Mutex<Vec<Connection>>,
    stop: Stop,
    /// False for `--socket`: an external peer is never spawned or killed.
    owns_process: bool,
}

pub struct Browser {
    inner: Arc<Inner>,
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
        self.inner.process.lock().await.idle_since = None;
        let start = Instant::now();
        let deadline = start + request.timeout;
        let connect_deadline = std::cmp::min(deadline, start + CONNECT_WINDOW);
        let outcome = self.fetch(request, deadline, connect_deadline).await;
        if outcome.is_err() {
            // disposeOnDetach releases this operation's context even if its
            // CDP stream is unusable; sending another command on a failed
            // stream can hang cleanup.
            self.shutdown().await;
        }
        outcome
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
        if connection.is_open() {
            self.inner.free.lock().await.push(connection);
        }
        // Inactivity starts after work completes, not when it starts.
        self.inner.process.lock().await.idle_since = Some(Instant::now());
        Ok(result)
    }

    /// Pop a pooled connection or open a fresh one (C++ `checkout`).
    async fn checkout(&self, deadline: Instant) -> Result<Connection, String> {
        let pooled = self.inner.free.lock().await.pop();
        let mut connection = pooled.unwrap_or_else(|| Connection::new(self.inner.stop.clone()));
        self.connect(&mut connection, deadline).await?;
        Ok(connection)
    }

    /// Connect, launching the managed browser first. Serialized by the
    /// process mutex so concurrent checkouts can never race a second spawn.
    async fn connect(&self, connection: &mut Connection, deadline: Instant) -> Result<(), String> {
        if connection.is_open() {
            return Ok(());
        }
        let mut process = self.inner.process.lock().await;
        let owns_process = self.inner.owns_process;
        if owns_process {
            launch(&mut process).await?;
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
            let gone = match process.child.as_mut() {
                Some(child) => !matches!(child.try_wait(), Ok(None)),
                None => true,
            };
            if gone {
                process.child = None;
                return Err("Lightpanda exited before opening its Unix socket".to_string());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                _ = self.inner.stop.wait() => {}
                _ = tokio::time::sleep(CONNECT_RETRY) => {}
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

    /// None while no managed browser is running or idle timeout is disabled.
    pub async fn idle_deadline(&self) -> Option<Instant> {
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
    }

    pub async fn stop_if_idle(&self) {
        if let Some(deadline) = self.idle_deadline().await
            && Instant::now() >= deadline
        {
            // Called only between requests, never during active CDP work.
            self.shutdown().await;
        }
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

/// One connect+handshake attempt against the CDP Unix socket, capped at the
/// C++ connection/read timeout so a stalled peer cannot outlive the loop.
async fn attempt(socket: &str) -> Result<CdpStream, ()> {
    let connect = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|_| ())?;
        // Handshake over our own stream; Host stays localhost like the C++
        // direct hostname/address map.
        let (websocket, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .map_err(|_| ())?;
        Ok(websocket)
    };
    tokio::time::timeout(CONNECT_ATTEMPT, connect)
        .await
        .unwrap_or(Err(()))
}

/// Spawn the managed browser: `lightpanda serve --unix-socket … --log-level
/// error`, stdin /dev/null, stdout redirected to stderr so a browser log can
/// never corrupt the MCP byte stream (C++ posix_spawn file actions).
async fn launch(process: &mut Process) -> Result<(), String> {
    let running = match process.child.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    };
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
    let duplicate = unsafe { libc::dup(libc::STDERR_FILENO) };
    if duplicate < 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "Cannot start Lightpanda: {error}; use --lightpanda /path/to/lightpanda"
        ));
    }
    let stdout = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) };
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
        .map_err(|error| {
            format!("Cannot start Lightpanda: {error}; use --lightpanda /path/to/lightpanda")
        })?;
    process.child = Some(child);
    Ok(())
}

/// SIGTERM, 500 ms to exit, then SIGKILL — C++ `Browser::Impl::stop_child`.
async fn stop_child(process: &mut Process) {
    let Some(mut child) = process.child.take() else {
        return;
    };
    match child.try_wait() {
        Ok(Some(_)) => return, // Already exited; never signal a reused PID.
        Ok(None) => {}
        Err(_) => return,
    }
    // A reaped child has no pid; treat that like an already-exited process.
    let Some(pid) = child.id() else {
        return;
    };
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(_) => return,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
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
                formats: vec!["markdown".to_string()],
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
        assert!(process.directory.is_empty());
        assert!(process.child.is_none());
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
                formats: vec!["markdown".to_string()],
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
        assert!(process.directory.is_empty());
        assert!(process.child.is_none());
    }
}
