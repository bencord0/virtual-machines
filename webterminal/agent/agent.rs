#!/usr/bin/env -S cargo +nightly -Zscript
---
[package]
edition = "2024"

[dependencies]
tungstenite = "0.21"
portable-pty = "0.8"
---

//! A guest agent / interface to this VM.
//!
//! Endpoints:
//!   GET /          -- an xterm.js page ("webssh") that opens a WebSocket to /ws
//!   GET /ws        -- upgrades to a WebSocket bridged to a login shell in a PTY
//!   GET /check     -- runs a check script, returns its "OK"/"NO"
//!   GET /health    -- "ok"
//!
//! Static assets are baked into the binary, so that this executable is standalone.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::{Role, WebSocket};
use tungstenite::{Error as WsError, Message};

const INDEX_HTML: &str = include_str!("web/index.html");
const XTERM_JS: &str = include_str!("web/xterm.js");
const XTERM_CSS: &str = include_str!("web/xterm.css");
const XTERM_FIT: &str = include_str!("web/addon-fit.js");

const LISTEN_ADDR: &str = "0.0.0.0:8080";

fn main() {
    let listener = TcpListener::bind(LISTEN_ADDR).expect("bind agent listener");
    eprintln!("agent listening on http://{LISTEN_ADDR}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(e) = handle(stream) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

fn handle(mut stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let head = read_head(&mut stream)?;
    if head.is_empty() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let mut sec_key: Option<String> = None;
    let mut is_upgrade = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "sec-websocket-key" => sec_key = Some(v.to_string()),
                "upgrade" => {
                    if v.to_ascii_lowercase().contains("websocket") {
                        is_upgrade = true;
                    }
                }
                _ => {}
            }
        }
    }

    if path == "/ws" && is_upgrade {
        return match sec_key {
            Some(key) => {
                let accept = derive_accept_key(key.as_bytes());
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\n\
                     Connection: Upgrade\r\n\
                     Upgrade: websocket\r\n\
                     Sec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                stream.write_all(resp.as_bytes())?;
                stream.flush()?;
                serve_ws(stream)
            }
            None => respond(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"missing Sec-WebSocket-Key",
            )
            .map_err(Into::into),
        };
    }

    match path {
        "/" => respond(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes())?,
        "/xterm.js" => respond(
            &mut stream,
            "200 OK",
            "application/javascript; charset=utf-8",
            XTERM_JS.as_bytes(),
        )?,
        "/xterm.css" => respond(&mut stream, "200 OK", "text/css; charset=utf-8", XTERM_CSS.as_bytes())?,
        "/addon-fit.js" => respond(
            &mut stream,
            "200 OK",
            "application/javascript; charset=utf-8",
            XTERM_FIT.as_bytes(),
        )?,
        "/health" => respond(&mut stream, "200 OK", "text/plain; charset=utf-8", b"ok")?,
        _ => respond(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"not found")?,
    }
    Ok(())
}

/// Parse a `{"type":"resize","cols":N,"rows":M}` control message without pulling
/// in a JSON dependency: just pluck the two integers when it's a resize.
fn parse_resize(s: &str) -> Option<(u16, u16)> {
    if !s.contains("resize") {
        return None;
    }
    Some((extract_u16(s, "cols")?, extract_u16(s, "rows")?))
}

/// Find `"<key>"` in `s` and read the first unsigned integer following it.
fn extract_u16(s: &str, key: &str) -> Option<u16> {
    let after = &s[s.find(key)? + key.len()..];
    let digits: String = after
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Bridge a WebSocket to a login shell running in a PTY.
fn serve_ws(stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    // A short read timeout turns read_message() into a poll: it returns WouldBlock
    // when there's no client input, letting us also service PTY output each loop.
    stream.set_read_timeout(Some(Duration::from_millis(20)))?;
    let mut ws = WebSocket::from_raw_socket(stream, Role::Server, None);

    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("bash");
    cmd.arg("-l");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", "/root");
    cmd.cwd("/root");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    // Keep the master open for as long as we're bridging; also used for resize.
    let master = pair.master;

    // PTY output is read on its own thread and funnelled back over a channel.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    loop {
        // Client -> PTY. Binary frames are keystrokes (stdin); text frames are
        // JSON control messages (currently just terminal resize).
        match ws.read() {
            Ok(Message::Binary(b)) => {
                writer.write_all(&b)?;
                writer.flush()?;
            }
            Ok(Message::Text(s)) => {
                if let Some((cols, rows)) = parse_resize(&s) {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            Ok(Message::Ping(p)) => {
                let _ = ws.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(WsError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }

        // PTY -> client.
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    if ws.send(Message::Binary(chunk)).is_err() {
                        let _ = child.kill();
                        return Ok(());
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = ws.send(Message::Close(None));
                    return Ok(());
                }
            }
        }

        // Shell exited: flush any tail, close, done.
        if let Ok(Some(_)) = child.try_wait() {
            while let Ok(chunk) = rx.try_recv() {
                let _ = ws.send(Message::Binary(chunk));
            }
            let _ = ws.send(Message::Close(None));
            break;
        }
    }

    let _ = child.kill();
    Ok(())
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Read an HTTP request head (up to the blank line terminating the headers).
fn read_head(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 65536 {
            break;
        }
    }
    Ok(buf)
}
