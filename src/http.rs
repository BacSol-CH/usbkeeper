//! usbkeeper — tiny HTTP/1.1 + SSE server on std::net (no tokio/axum).
//! Routes: GET /api/state, GET /events, GET / and /ui, POST /api/rescan.

use crate::state;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

static UI_ROOT: OnceLock<String> = OnceLock::new();

pub struct Server {
    pub rescan: Arc<AtomicBool>,
}

pub fn serve(addr: &str, rescan: Arc<AtomicBool>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("[usbkeeper] http listening on {addr}");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let rescan = rescan.clone();
                thread::spawn(move || {
                    let _ = handle(s, rescan);
                });
            }
            Err(e) => eprintln!("[usbkeeper] accept: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, rescan: Arc<AtomicBool>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    // consume headers
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/").to_string();

    match (method, path.as_str()) {
        ("GET", "/api/state") => {
            write_response(&mut stream, 200, "application/json", state::snapshot_json().as_bytes())
        }
        ("GET", "/events") | ("GET", "/api/events") => sse(&stream),
        ("POST", "/api/rescan") => {
            rescan.store(true, Ordering::SeqCst);
            write_response(&mut stream, 200, "application/json", b"{\"ok\":true}")
        }
        ("GET", "/") | ("GET", "/ui") => {
            let idx = ui_path();
            match fs::read(&idx) {
                Ok(bytes) => write_response(&mut stream, 200, "text/html", &bytes),
                Err(_) => write_response(
                    &mut stream,
                    200,
                    "text/html",
                    b"<!doctype html><meta charset=utf-8><title>usbkeeper</title>\
                      <h1>usbkeeper</h1><p>waiting-for-ui</p>",
                ),
            }
        }
        ("HEAD", _) => write_response(&mut stream, 200, "text/plain", b""),
        _ => write_response(&mut stream, 404, "application/json", b"{\"error\":\"not found\"}"),
    }
}

/// Where the WebUI lives (single file under --ui-root). Set via --ui-root at startup,
/// env USBKEEPER_UI override for tests, default per frozen spec path.
fn ui_path() -> String {
    if let Ok(v) = std::env::var("USBKEEPER_UI") {
        return v;
    }
    let root = UI_ROOT.get().cloned().unwrap_or_else(|| "/var/lib/usbkeeper/web".to_string());
    let p = Path::new(&root);
    if p.is_file() {
        root
    } else {
        format!("{}/index.html", root.trim_end_matches('/'))
    }
}

pub fn set_ui_root(root: Option<String>) {
    if let Some(r) = root {
        let _ = UI_ROOT.set(r);
    }
}

fn sse(stream: &TcpStream) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    out.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Connection: keep-alive\r\n\
          X-Accel-Buffering: no\r\n\r\n",
    )?;
    // initial snapshot: one event per port (incl. empty ones we know about)
    let snap = state::reg().lock().map(|r| {
        r.ports
            .values()
            .map(|p| p.to_json())
            .collect::<Vec<_>>()
    });
    if let Ok(items) = snap {
        for j in items {
            out.write_all(format!("event: state\ndata: {j}\n\n").as_bytes())?;
        }
    }
    let rx = state::subscribe();
    stream.set_read_timeout(None).ok();
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(json) => {
                if out
                    .write_all(format!("event: state\ndata: {json}\n\n").as_bytes())
                    .is_err()
                {
                    return Ok(()); // client gone
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if out.write_all(b": ka\n\n").is_err() {
                    return Ok(());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn write_response(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        404 => "404 Not Found",
        _ => "500 Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
