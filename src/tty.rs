//! Draws the pet by writing standard Kitty graphics escapes to a pane's pty.
//!
//! herdr 0.9.2 removed `pane.graphics.*`; panes now show whatever Kitty
//! graphics their terminal output contains. The daemon is not the pane's
//! process, but the pane's pty slave is an ordinary file owned by the same
//! user: a write to it is terminal *output* as far as herdr is concerned,
//! exactly as if the pane's program had printed it. herdr then handles
//! clipping, scrolling, visibility and the host terminal.
//!
//! Every draw is one `write` of: save cursor, absolute move, image, restore
//! cursor. Kernel ttys serialise `write` calls, so it never lands inside a
//! sequence emitted by a single write of the pane's own program.
use crate::socket;
use base64::Engine;
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

/// Kitty image id of the pet in every pane (`PET\0`). Re-transmitting the same
/// id replaces the previous frame and its placement.
const IMAGE_ID: u32 = 0x5045_5400;
/// Draws above the pane's text.
const Z_INDEX: i32 = 1;
/// Kitty limits one escape's payload to 4096 base64 bytes.
const CHUNK: usize = 4096;
/// A pane that does not drain its pty for this long loses the frame.
const WRITE_DEADLINE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u32,
    pub rows: u32,
    pub width_px: u32,
    pub height_px: u32,
}

impl WinSize {
    /// Integer cell size in pixels, when herdr set the pixel dimensions.
    pub fn cell_px(self) -> Option<(u32, u32)> {
        if self.cols == 0 || self.rows == 0 || self.width_px == 0 || self.height_px == 0 {
            return None;
        }
        Some((self.width_px / self.cols, self.height_px / self.rows))
    }
}

#[derive(Debug)]
pub enum OpenError {
    /// herdr no longer has this pane.
    Gone,
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Gone => f.write_str("pane no longer exists"),
            OpenError::Other(e) => f.write_str(e),
        }
    }
}

/// The pty of one herdr pane, opened for writing.
pub struct PaneTty {
    file: File,
}

/// `/dev/...` path of the controlling tty of `pid`, via `ps` (the same call on Linux and macOS).
fn tty_path(pid: i64) -> Result<String, String> {
    let out = Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if name.is_empty() || name.starts_with('?') || name == "-" {
        return Err(format!("pid {pid} has no controlling tty"));
    }
    Ok(format!("/dev/{name}"))
}

impl PaneTty {
    pub fn open(pane_id: &str) -> Result<PaneTty, OpenError> {
        let info = socket::request("pane.process_info", json!({ "pane_id": pane_id })).map_err(|e| {
            if matches!(e.code(), "pane_not_found" | "not_found") {
                OpenError::Gone
            } else {
                OpenError::Other(format!("pane.process_info: {e}"))
            }
        })?;
        let pid = info["process_info"]["shell_pid"]
            .as_i64()
            .ok_or_else(|| OpenError::Other("pane.process_info: no shell_pid".into()))?;
        let path = tty_path(pid).map_err(OpenError::Other)?;
        // O_NONBLOCK: a pane that stops reading (flow control) must not freeze the daemon.
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| OpenError::Other(format!("open {path}: {e}")))?;
        Ok(PaneTty { file })
    }

    pub fn winsize(&self) -> io::Result<WinSize> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ fills a `winsize` for a valid tty fd.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(WinSize { cols: ws.ws_col as u32, rows: ws.ws_row as u32, width_px: ws.ws_xpixel as u32, height_px: ws.ws_ypixel as u32 })
    }

    /// Show `png` with its top-left at 0-based (`col`, `row`), scaled into `cols`x`rows` cells.
    pub fn draw(&mut self, png: &[u8], col: u32, row: u32, cols: u32, rows: u32) -> io::Result<()> {
        self.write_all(&draw_sequence(png, col, row, cols, rows))
    }

    /// Remove the pet (and free its image data) from the pane. Best effort.
    pub fn clear(&mut self) {
        let _ = self.write_all(clear_sequence().as_bytes());
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + WRITE_DEADLINE;
        let mut off = 0;
        while off < buf.len() {
            match self.file.write(&buf[off..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() || !self.wait_writable(left) {
                        if off > 0 {
                            // Cancel the half-written escape so the pane's parser is not left inside it.
                            let _ = self.file.write(b"\x18");
                        }
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "pane is not reading its terminal"));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn wait_writable(&self, timeout: Duration) -> bool {
        let mut fd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut fd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) > 0 }
    }
}

/// Delete the pet image everywhere in the pane.
fn clear_sequence() -> String {
    format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\")
}

/// Cursor-neutral transmit-and-display of one PNG frame.
///
/// `ESC 7`/`ESC 8` bracket the move so the program's cursor, attributes and
/// pending-wrap state survive; `?6l` makes the move absolute even when the
/// program enabled origin mode (restored by `ESC 8`). `C=1` keeps the image
/// from moving the cursor and `q=2` stops herdr's reply from being typed into
/// the pane's input.
fn draw_sequence(png: &[u8], col: u32, row: u32, cols: u32, rows: u32) -> Vec<u8> {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = Vec::with_capacity(data.len() + data.len() / CHUNK * 16 + 96);
    out.extend_from_slice(format!("\x1b7\x1b[?6l\x1b[{};{}H", row + 1, col + 1).as_bytes());
    let mut chunks = data.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            out.extend_from_slice(
                format!("\x1b_Ga=T,f=100,i={IMAGE_ID},p=1,c={cols},r={rows},C=1,z={Z_INDEX},q=2,m={more};").as_bytes(),
            );
            first = false;
        } else {
            out.extend_from_slice(format!("\x1b_Gq=2,m={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out.extend_from_slice(b"\x1b8");
    out
}

/// Inner size of a pane in cells, as its program sees it.
pub fn pane_size(pane_id: &str) -> Option<WinSize> {
    PaneTty::open(pane_id).ok()?.winsize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_frame_is_one_cursor_neutral_escape() {
        let s = String::from_utf8(draw_sequence(b"png", 4, 9, 6, 3)).unwrap();
        assert_eq!(
            s,
            format!("\x1b7\x1b[?6l\x1b[10;5H\x1b_Ga=T,f=100,i={IMAGE_ID},p=1,c=6,r=3,C=1,z=1,q=2,m=0;cG5n\x1b\\\x1b8")
        );
    }

    #[test]
    fn large_frames_split_at_the_kitty_chunk_limit() {
        // 9000 bytes -> 12000 base64 chars -> 3 chunks of at most 4096.
        let png = vec![0xA5u8; 9000];
        let s = String::from_utf8(draw_sequence(&png, 0, 0, 2, 2)).unwrap();
        let escapes: Vec<&str> = s.split("\x1b_G").skip(1).collect();
        assert_eq!(escapes.len(), 3);
        assert!(escapes[0].starts_with("a=T,") && escapes[0].contains("m=1;"));
        assert!(escapes[1].starts_with("q=2,m=1;"));
        assert!(escapes[2].starts_with("q=2,m=0;"));
        let payload: usize = escapes
            .iter()
            .map(|e| {
                let body = e.split_once(';').unwrap().1;
                let body = body.strip_suffix("\x1b8").unwrap_or(body);
                body.strip_suffix("\x1b\\").unwrap().len()
            })
            .sum();
        assert_eq!(payload, 12000);
        assert!(s.ends_with("\x1b\\\x1b8"));
    }

    #[test]
    fn cell_size_needs_pixel_dimensions() {
        let ws = WinSize { cols: 152, rows: 36, width_px: 1216, height_px: 720 };
        assert_eq!(ws.cell_px(), Some((8, 20)));
        assert_eq!(WinSize { width_px: 0, ..ws }.cell_px(), None);
        assert_eq!(WinSize { cols: 0, ..ws }.cell_px(), None);
    }
}
