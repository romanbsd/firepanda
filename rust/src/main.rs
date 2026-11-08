//! firepanda: MCP over stdio with a managed Lightpanda browser.
//! Ported from `src/main.cpp`; milestone gates in docs/rust-migration-plan.md.

mod browser;
mod cli;
mod json;
mod mcp;
#[cfg(feature = "search")]
mod search;
mod tools;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const READ_CHUNK: usize = 8192;

/// Idle expiry as a select arm: no deadline (idle timeout disabled, external
/// socket, or no browser yet) waits forever, like C++ `wait_for_input` with a
/// null timeout.
async fn idle_wait(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Stdio loop with the same structure as C++ `main`: buffered chunked reads,
/// bounded line assembly, per-line idle shutdown, and prompt exits on EOF (0),
/// shutdown signals (0), or a closed stdout (1).
async fn serve(options: browser::Options, stop: browser::Stop) -> i32 {
    let mut server = mcp::Server::new(options, stop.clone());
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buffer = [0u8; READ_CHUNK];
    let mut line: Vec<u8> = Vec::new();
    let mut oversized = false;
    let mut exit_code = 0;
    'service: loop {
        if stop.stopped() {
            break;
        }
        server.stop_if_idle().await;
        let idle = server.idle_deadline().await.map(tokio::time::Instant::from);
        let count = tokio::select! {
            _ = stop.wait() => break,
            _ = idle_wait(idle) => continue,
            read = stdin.read(&mut buffer) => match read {
                Ok(0) => break,
                Ok(count) => count,
                // Retry only interrupts; every other failure ends the loop
                // exactly like a closed stdin (C++ `count <= 0`).
                Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            },
        };
        // Buffered reads keep stdio cheap while bounding memory before JSON parsing.
        let mut offset = 0;
        while offset < count {
            if stop.stopped() {
                break;
            }
            let chunk = &buffer[offset..count];
            let newline = chunk.iter().position(|&byte| byte == b'\n');
            let length = newline.unwrap_or(chunk.len());
            if !oversized {
                if line.len() + length <= json::MAX_LINE_BYTES {
                    line.extend_from_slice(&chunk[..length]);
                } else {
                    oversized = true;
                    line.clear();
                }
            }
            let Some(index) = newline else {
                break;
            };
            offset += index + 1;
            // Also expire during a stream of buffered pings/notifications; MCP
            // traffic alone must not keep the browser alive.
            server.stop_if_idle().await;
            let response = if oversized {
                Some(mcp::rpc_error(Value::Null, -32600, "Request exceeds 1 MiB"))
            } else {
                match json::parse_line(&line) {
                    Ok(request) => server.handle(&request).await,
                    Err(json::ParseError::Nesting(message)) => {
                        Some(mcp::rpc_error(Value::Null, -32600, &message))
                    }
                    Err(json::ParseError::Syntax) => {
                        Some(mcp::rpc_error(Value::Null, -32700, "Parse error"))
                    }
                }
            };
            line.clear();
            oversized = false;
            if let Some(response) = response {
                let mut bytes =
                    serde_json::to_vec(&response).expect("in-memory JSON always serializes");
                bytes.push(b'\n');
                // Once the client closes its reader, further tool work cannot
                // be delivered; returning still tears down the browser.
                let written =
                    stdout.write_all(&bytes).await.is_ok() && stdout.flush().await.is_ok();
                if !written {
                    exit_code = 1;
                    break 'service;
                }
            }
        }
    }
    // C++ `Server` destructor: stop the managed child and remove its socket
    // directory before the process exits.
    server.shutdown().await;
    exit_code
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let options = cli::parse();
    let stop = browser::Stop::channel();
    let mut terminate =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                eprintln!("firepanda: cannot install shutdown signal handler: {error}");
                std::process::exit(1);
            }
        };
    let mut interrupt =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(error) => {
                eprintln!("firepanda: cannot install shutdown signal handler: {error}");
                std::process::exit(1);
            }
        };
    {
        // Like C++ `interrupt`: one flag wakes deadline reads, connect
        // retries, waitFor sleeps, and the stdio loop at once.
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            stop.notify();
        });
    }
    std::process::exit(serve(options, stop).await);
}
