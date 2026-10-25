//! NDJSON client for the herdr socket API over a Unix domain socket.
//! One request per line; `events.subscribe` keeps the connection open.
//! Push-style connections get a reader thread that forwards lines through
//! caller-supplied closures.
use serde::Serialize;
use serde_json::{json, Value};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    match std::env::var("HERDR_SESSION") {
        Ok(session) if !session.is_empty() => {
            home.join(format!(".config/herdr/sessions/{session}/herdr.sock"))
        }
        _ => home.join(".config/herdr/herdr.sock"),
    }
}

#[derive(Debug, Clone)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Herdr(HerdrError),
    Json(serde_json::Error),
    Closed,
}

impl Error {
    pub fn code(&self) -> &str {
        match self {
            Error::Herdr(e) => &e.code,
            _ => "",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Herdr(e) => write!(f, "{}: {}", e.code, e.message),
            Error::Json(e) => write!(f, "invalid json: {e}"),
            Error::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

fn next_id(prefix: &str) -> String {
    format!("pet_{prefix}_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

fn connect() -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path())
}

fn write_line(stream: &mut UnixStream, value: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// Parse one response line: `{"id":..,"result":{..}}` or `{"id":..,"error":{code,message}}`.
fn parse_response(line: &str) -> Result<Value, Error> {
    let msg: Value = serde_json::from_str(line)?;
    if let Some(err) = msg.get("error") {
        return Err(Error::Herdr(HerdrError {
            code: err["code"].as_str().unwrap_or("error").to_owned(),
            message: err["message"].as_str().unwrap_or("").to_owned(),
        }));
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

/// One-shot request on a fresh connection; returns the `result` object.
/// Requests run on the daemon's event-loop thread, so they must never hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

pub fn request(method: &str, params: Value) -> Result<Value, Error> {
    let mut stream = connect()?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    write_line(&mut stream, &json!({ "id": next_id("req"), "method": method, "params": params }))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(Error::Closed);
    }
    parse_response(&line)
}

/// A push connection (subscription). Dropping/closing it ends the reader thread.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    pub fn close(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reads lines until EOF/error, then reports the close reason (an error line
/// herdr wrote, if any).
fn spawn_reader(
    mut reader: BufReader<UnixStream>,
    on_line: impl Fn(Value) + Send + 'static,
    on_close: impl FnOnce(Option<String>) + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut reason = None;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(v) if v.get("error").is_some() => {
                            reason = Some(
                                v["error"]["message"].as_str().unwrap_or("error").to_owned(),
                            );
                        }
                        Ok(v) => on_line(v),
                        Err(_) => {}
                    }
                }
                Err(e) => {
                    reason = Some(e.to_string());
                    break;
                }
            }
        }
        on_close(reason);
    });
}

/// Long-lived `events.subscribe`. Returns once herdr acknowledges the
/// subscription; pushed events go to `on_event` as the full envelope
/// (`{"event":"pane_focused","data":{"type":"pane_focused",...}}`).
pub fn subscribe(
    subscriptions: Vec<Value>,
    on_event: impl Fn(Value) + Send + 'static,
    on_close: impl FnOnce(Option<String>) + Send + 'static,
) -> Result<Connection, Error> {
    let mut stream = connect()?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    write_line(
        &mut stream,
        &json!({ "id": next_id("sub"), "method": "events.subscribe", "params": { "subscriptions": subscriptions } }),
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    parse_response(&read_ack(&mut reader)?)?;
    spawn_reader(reader, on_event, on_close);
    Ok(Connection { stream })
}

/// Reads the acknowledgement line. The same `BufReader` must then be handed to
/// the reader thread: herdr may push the first event in the same read as the
/// ack, and a throwaway buffer would silently drop it.
fn read_ack(reader: &mut BufReader<UnixStream>) -> Result<String, Error> {
    reader.get_ref().set_read_timeout(Some(REQUEST_TIMEOUT))?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    reader.get_ref().set_read_timeout(None)?;
    if line.is_empty() {
        return Err(Error::Closed);
    }
    Ok(line)
}
