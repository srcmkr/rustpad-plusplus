//! Single instance: a second launch hands its files to the running window
//! over a Unix socket and exits within milliseconds, like notepad++ does.

use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user = std::env::var("USER").unwrap_or_default();
    let display = std::env::var("WAYLAND_DISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .unwrap_or_default()
        .replace(['/', ':'], "_");
    dir.join(format!("rustpad-{user}-{display}.sock"))
}

/// Try to pass `files` to a running instance. Returns true if it took them.
pub fn hand_off(files: &[PathBuf]) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path()) else {
        return false;
    };
    let mut msg = Vec::new();
    for f in files {
        let abs = std::path::absolute(f).unwrap_or_else(|_| f.clone());
        msg.extend_from_slice(abs.as_os_str().as_bytes());
        msg.push(0);
    }
    if stream.write_all(&msg).is_err() || stream.shutdown(std::net::Shutdown::Write).is_err() {
        return false;
    }
    // Wait for the acknowledgement so we never drop files silently.
    let mut ack = [0u8; 1];
    matches!(stream.read(&mut ack), Ok(1))
}

/// Listen for hand-offs. Each message is the list of files to open (may be
/// empty: just raise the window).
pub fn listen() -> Option<async_channel::Receiver<Vec<PathBuf>>> {
    let path = socket_path();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(_) => {
            // A stale socket from a crashed instance: nobody answered above.
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path).ok()?
        }
    };
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                if stream.read_to_end(&mut buf).is_err() {
                    continue;
                }
                let files = buf
                    .split(|b| *b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| PathBuf::from(std::ffi::OsStr::from_bytes(s)))
                    .collect();
                if tx.send_blocking(files).is_err() {
                    break;
                }
                let _ = stream.write_all(&[1]);
            }
        })
        .ok()?;
    Some(rx)
}

pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}
