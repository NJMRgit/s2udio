//! A tiny localhost reverse proxy that injects the engine's auth header.
//!
//! The rqbit web UI (`/web/`) is an SPA: its `fetch()` calls do NOT carry
//! credentials, and browsers do not replay URL-userinfo basic auth on
//! `fetch()` (verified 2026-08-15 with headless Chromium — the page loads
//! via `http://user:pass@host/web/` but every API call fails with
//! "network error"; the UI shows 401 / "Error refreshing torrents"). So a
//! userinfo URL cannot authenticate the SPA.
//!
//! Instead of dropping the engine's random-token auth (defense in depth on
//! 127.0.0.1 against other local users and DNS-rebinding/CSRF from web
//! pages), this module exposes the same HTTP API behind a second loopback
//! listener that adds the `Authorization: Basic …` header to EVERY request
//! before forwarding it to the engine. s2udio opens
//! `http://127.0.0.1:<proxy port>/web/` (no credentials in the URL) and
//! the SPA's same-origin fetches work, while the engine port itself stays
//! auth-protected.
//!
//! Implementation: one std `TcpListener` on `127.0.0.1:0` (ephemeral
//! port), a thread per connection. The request head is read up to
//! `\r\n\r\n`, the `Authorization` header is injected, `Connection:
//! keep-alive` is rewritten to `Connection: close` (each request = one
//! engine connection; the browser is free to open as many as it likes and
//! the response is framed by the engine closing the socket — no body
//! parsing needed, so uploads and streaming responses pass through), and
//! the body (whatever followed the head) is streamed. Both directions run
//! concurrently; when the engine finishes the response it closes, the
//! client socket is closed, and the thread exits.

use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

/// Binds `127.0.0.1:0` and proxies the engine's HTTP API, injecting
/// `Authorization` into every request. Dropping the proxy stops the
/// accept loop; per-connection threads exit when their client sockets
/// close.
pub struct WebUiProxy {
    port: u16,
    shutdown: Arc<AtomicBool>,
}

impl std::fmt::Debug for WebUiProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebUiProxy").field("port", &self.port).finish_non_exhaustive()
    }
}

impl WebUiProxy {
    /// Spawn the proxy for `engine_port`. `auth_header` is the raw
    /// `Authorization: Basic …` header value. Fails only when the
    /// loopback bind fails.
    pub fn spawn(engine_port: u16, auth_header: String) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            // Nonblocking accept + shutdown flag polled with a short
            // sleep: Drop stops the loop deterministically (no reliance
            // on closing a socket the loop also owns).
            let _ = listener.set_nonblocking(true);
            let mut threads: Vec<JoinHandle<()>> = Vec::new();
            loop {
                if accept_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((client, _)) => {
                        let engine_addr = format!("127.0.0.1:{engine_port}");
                        let auth_header = auth_header.clone();
                        threads.push(std::thread::spawn(move || {
                            let _ = handle_connection(client, &engine_addr, &auth_header);
                        }));
                        // A runaway client must never accumulate threads
                        // forever (connections keep running until the
                        // clients close; we just stop tracking them).
                        if threads.len() > 64 {
                            threads.drain(0..threads.len() - 64);
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self { port, shutdown })
    }

    /// The proxy's bound port (`http://127.0.0.1:{port}/web/`).
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for WebUiProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Serve one client connection: rewrite the head, forward, relay both
/// directions until the engine closes (response done) or the client goes
/// away.
fn handle_connection(client: TcpStream, engine_addr: &str, auth_header: &str) -> io::Result<()> {
    let mut client = client;
    // 1. Read the request head (up to and including `\r\n\r\n`); any
    //    surplus bytes are the start of the body.
    let (head, surplus) = read_head(&mut client)?;
    let rewritten = rewrite_head(&head, auth_header);

    // 2. Connect to the engine and send the rewritten head + surplus.
    let mut engine = TcpStream::connect(engine_addr)?;
    engine.write_all(&rewritten)?;
    engine.write_all(&surplus)?;

    // 3. Relay both directions concurrently:
    //    - engine -> client: driven here; the engine closes after the
    //      response (we rewrote the request to `Connection: close`), so
    //      EOF = response complete -> close the client.
    //    - client -> engine: helper thread; when the client aborts, the
    //      engine socket is shut down so the response relay unblocks.
    let engine_write = engine.try_clone()?;
    let client_write = client.try_clone()?;
    let relay_client_to_engine = std::thread::spawn(move || {
        let mut client_r = client_write;
        let mut engine_w = engine_write;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match client_r.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if engine_w.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // The client is done sending (or gone): unblock the response
        // relay if it is still waiting on the engine.
        let _ = engine_w.shutdown(Shutdown::Both);
    });

    let mut engine_r = engine;
    let mut client_w = client;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match engine_r.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if client_w.write_all(&buffer[..n]).is_err() {
                    break;
                }
            }
        }
    }
    // The engine finished the response: close the client (its read side
    // was already done). This also unblocks the client->engine relay.
    let _ = client_w.shutdown(Shutdown::Both);
    let _ = relay_client_to_engine.join();
    Ok(())
}

/// Read bytes up to and including the `\r\n\r\n` head terminator.
/// Returns (head including terminator, surplus bytes after it). Caps the
/// head at 1 MiB (headers are small; bodies are streamed, not buffered).
fn read_head(stream: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed during head"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = find_head_end(&buf) {
            let head: Vec<u8> = buf.drain(..end).collect();
            return Ok((head, buf));
        }
        if buf.len() > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
    }
}

/// Index just past `\r\n\r\n`, or None.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Inject/force the `Authorization` header and `Connection: close`.
fn rewrite_head(head: &[u8], auth_header: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let lines: Vec<&str> = text.lines().collect();
    let mut has_auth = false;
    for line in lines.iter().skip(1) {
        // Header names are compared without the trailing ':' — the
        // prefix slices are exactly the name lengths ("Authorization" =
        // 13, "Proxy-Authorization" = 19, "Connection" = 10).
        if line.get(..13).map(|p| p.eq_ignore_ascii_case("authorization")).unwrap_or(false)
            || line
                .get(..19)
                .map(|p| p.eq_ignore_ascii_case("proxy-authorization"))
                .unwrap_or(false)
        {
            has_auth = true;
        }
    }
    let mut out = String::with_capacity(head.len() + 64);
    if let Some(first) = lines.first() {
        out.push_str(first);
        out.push_str("\r\n");
    }
    if !has_auth {
        out.push_str(&format!("Authorization: {auth_header}\r\n"));
    }
    for line in lines.iter().skip(1) {
        // Force one request per engine connection: the response is framed
        // by the engine closing the socket, so no body parsing is needed
        // (uploads and streaming/SSE responses pass through untouched).
        if line.get(..10).map(|p| p.eq_ignore_ascii_case("connection")).unwrap_or(false) {
            out.push_str("Connection: close\r\n");
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}
