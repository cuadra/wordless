//! Plain-text clipboard.
//!
//! Primary path: pipe into `wl-copy` with an explicit `text/plain` MIME type.
//! Why a subprocess instead of an in-process Wayland client: on Wayland the
//! clipboard is *owned by a live process*. If Wordless served the selection
//! itself, the copied text would vanish the moment you hit `q`. `wl-copy`
//! forks a tiny daemon that keeps serving it, which is exactly the semantics
//! a pager needs — and it costs zero extra crates.
//!
//! Fallback: OSC 52, so yanking still works over SSH or under X11 terminals.

use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy(text: &str) -> Result<&'static str, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match wl_copy(text) {
            Ok(()) => return Ok("wl-copy"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // fall through
            Err(e) => return Err(format!("wl-copy: {e}")),
        }
    }
    osc52(text)
        .map(|_| "osc52")
        .map_err(|e| format!("osc52: {e}"))
}

fn wl_copy(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["--type", "text/plain;charset=utf-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null()) // never let a child scribble over the TUI
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(text.as_bytes())?;
    // stdin is dropped above → EOF; wl-copy forks and the parent exits.
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("exited with {status}")))
    }
}

fn osc52(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        assert_eq!(super::base64(b"Man"), "TWFu");
        assert_eq!(super::base64(b"Ma"), "TWE=");
        assert_eq!(super::base64(b"M"), "TQ==");
    }
}
