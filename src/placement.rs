//! Where is our main window on screen?
//!
//! GPUI's X11 backend drops the origin when a window is moved and resized at
//! once, and under a reparenting window manager its coordinates are relative
//! to the frame. So we ask the X server directly: find the top-level window
//! with our PID and translate its origin to root coordinates. This runs only
//! when the placement is saved (resize, quit), never at startup.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Window};

/// Root-relative origin of this process's main window (X11 only).
pub fn main_window_origin() -> Option<(i32, i32)> {
    std::env::var_os("DISPLAY")?;
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let main = own_windows(&conn, root)?
        .into_iter()
        .filter(|(_, t)| t != tr!("Settings - rustpad++") && t.ends_with("rustpad++"))
        .filter_map(|(w, _)| {
            let g = conn.get_geometry(w).ok()?.reply().ok()?;
            Some((w, g.width as u32 * g.height as u32))
        })
        .max_by_key(|(_, area)| *area)?
        .0;
    let t = conn.translate_coordinates(main, root, 0, 0).ok()?.reply().ok()?;
    Some((t.dst_x as i32, t.dst_y as i32))
}

/// Top-level client windows of this process: (window, title).
pub(crate) fn own_windows(conn: &impl Connection, root: Window) -> Option<Vec<(Window, String)>> {
    let pid_atom = conn.intern_atom(true, b"_NET_WM_PID").ok()?.reply().ok()?.atom;
    let name_atom = conn.intern_atom(true, b"_NET_WM_NAME").ok()?.reply().ok()?.atom;
    let utf8 = conn.intern_atom(true, b"UTF8_STRING").ok()?.reply().ok()?.atom;
    let pid = std::process::id();
    let mut found = Vec::new();
    let mut level: Vec<Window> = vec![root];
    for _ in 0..3 {
        let mut next = Vec::new();
        for w in level {
            let Ok(Ok(tree)) = conn.query_tree(w).map(|c| c.reply()) else { continue };
            for child in tree.children {
                let owner = conn
                    .get_property(false, child, pid_atom, AtomEnum::CARDINAL, 0, 1)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().and_then(|mut v| v.next()));
                if owner == Some(pid) {
                    let title = conn
                        .get_property(false, child, name_atom, utf8, 0, 256)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                        .unwrap_or_default();
                    found.push((child, title));
                } else {
                    next.push(child);
                }
            }
        }
        level = next;
    }
    Some(found)
}
