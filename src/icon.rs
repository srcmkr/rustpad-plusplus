//! App icon on Linux: desktop entry, icon theme and X11 window icon.
//!
//! Panels and task switchers look the icon up through a `.desktop` file whose
//! `StartupWMClass` matches the window. Without one, Cinnamon/Muffin fall back
//! to `_NET_WM_ICON`, but read it at a fixed small size and scale it up, which
//! looks blurry. So on startup we keep `rustpad.desktop` and the hicolor icons
//! in `$XDG_DATA_HOME` up to date ourselves, and still set `_NET_WM_ICON` for
//! window managers that only use that. Everything runs on helper threads so it
//! stays off the startup path.

use std::path::{Path, PathBuf};
use std::process::Command;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
use x11rb::wrapper::ConnectionExt as _;

use crate::placement::own_windows;

/// (edge in px, PNG). 16 px has its own pixel-snapped artwork.
const PNGS: &[(u32, &[u8])] = &[
    (16, include_bytes!("../assets/logo/png/rustpad-16.png")),
    (24, include_bytes!("../assets/logo/png/rustpad-24.png")),
    (32, include_bytes!("../assets/logo/png/rustpad-32.png")),
    (48, include_bytes!("../assets/logo/png/rustpad-48.png")),
    (64, include_bytes!("../assets/logo/png/rustpad-64.png")),
    (128, include_bytes!("../assets/logo/png/rustpad-128.png")),
    (256, include_bytes!("../assets/logo/png/rustpad-256.png")),
    (512, include_bytes!("../assets/logo/png/rustpad-512.png")),
];
const SVG: &[u8] = include_bytes!("../assets/logo/rustpad.svg");
const DESKTOP: &str = include_str!("../assets/linux/rustpad.desktop");

/// Set the icon on all windows of this process that exist right now (X11 only).
pub fn apply() {
    if std::env::var_os("DISPLAY").is_none() {
        return;
    }
    std::thread::spawn(|| {
        let _ = set_window_icons();
    });
}

/// Create or refresh the desktop entry and theme icons for this binary.
pub fn register() {
    std::thread::spawn(|| {
        let _ = write_desktop_entry();
    });
}

fn set_window_icons() -> Option<()> {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let atom = conn.intern_atom(false, b"_NET_WM_ICON").ok()?.reply().ok()?.atom;
    let data = icon_data();
    for (w, _) in own_windows(&conn, root)? {
        conn.change_property32(PropMode::REPLACE, w, atom, AtomEnum::CARDINAL, &data).ok()?;
    }
    conn.flush().ok()
}

/// `_NET_WM_ICON` payload: per size width, height, then non-premultiplied ARGB rows.
fn icon_data() -> Vec<u32> {
    let mut out = Vec::new();
    for (_, png) in PNGS.iter().filter(|(px, _)| *px <= 256) {
        let Ok(img) = image::load_from_memory_with_format(png, image::ImageFormat::Png) else { continue };
        let img = img.into_rgba8();
        out.extend([img.width(), img.height()]);
        out.extend(img.pixels().map(|p| {
            let [r, g, b, a] = p.0;
            u32::from_be_bytes([a, r, g, b])
        }));
    }
    out
}

/// The desktop entry for the program at `exe`.
pub fn desktop_entry(exe: &str) -> String {
    let mut entry = DESKTOP.replace("Exec=rustpad ", &format!("Exec={} ", exec_quote(exe)));
    entry.push_str(&format!("MimeType={}\n", crate::file_types::mime_list()));
    entry
}

/// Write the icons into the `hicolor` theme at `hicolor`. Returns whether any changed.
pub fn install_icons(hicolor: &Path) -> bool {
    let mut changed = false;
    for (px, png) in PNGS {
        changed |= sync(&hicolor.join(format!("{px}x{px}/apps/rustpad.png")), png);
    }
    changed | sync(&hicolor.join("scalable/apps/rustpad.svg"), SVG)
}

/// Let the desktop notice new icons or entries; a stale icon cache hides new
/// icons until the next login.
pub fn refresh_caches(hicolor: &Path, icons_changed: bool, applications: &Path, entry_changed: bool) {
    if icons_changed && hicolor.join("icon-theme.cache").exists() {
        let _ = Command::new("gtk-update-icon-cache").args(["-f", "-t"]).arg(hicolor).output();
    }
    if entry_changed {
        let _ = Command::new("update-desktop-database").arg(applications).output();
    }
}

fn write_desktop_entry() -> Option<()> {
    use rustpad_update::layout::Install;
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let applications = data.join("applications");
    let user_entry = applications.join("rustpad.desktop");
    let exe = match Install::current() {
        // Installed for all users: the system's entry applies. Ours in the
        // user's folder would hide it, and may point to an old build.
        install if install.is_system() => {
            let ours = std::fs::read_to_string(&user_entry).is_ok_and(|e| e.contains("StartupWMClass=rustpad"));
            if ours && std::fs::remove_file(&user_entry).is_ok() {
                refresh_caches(&data.join("icons/hicolor"), false, &applications, true);
            }
            return Some(());
        }
        // The program runs from a temporary mount; the AppImage file stays.
        Install::AppImage(image) => image,
        Install::Build | Install::Deb | Install::Local => std::env::current_exe().ok()?,
    };
    // A rebuild replaces the running binary; its old path then ends in " (deleted)".
    if !exe.is_file() {
        return None;
    }
    let hicolor = data.join("icons/hicolor");
    let icons_changed = install_icons(&hicolor);
    let entry_changed = sync(&user_entry, desktop_entry(&exe.to_string_lossy()).as_bytes());
    refresh_caches(&hicolor, icons_changed, &applications, entry_changed);
    Some(())
}

/// Write `bytes` to `path` unless it already holds exactly that. Returns whether it wrote.
fn sync(path: &Path, bytes: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return false;
    }
    path.parent().is_some_and(|dir| std::fs::create_dir_all(dir).is_ok()) && std::fs::write(path, bytes).is_ok()
}

/// Quote an `Exec` program path per the Desktop Entry spec where needed.
fn exec_quote(path: &str) -> String {
    const RESERVED: &[char] = &[' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`'];
    if !path.contains(RESERVED) {
        return path.replace('%', "%%");
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::exec_quote;

    #[test]
    fn exec_paths() {
        assert_eq!(exec_quote("/home/a/rustpad++/target/release/rustpad"), "/home/a/rustpad++/target/release/rustpad");
        assert_eq!(exec_quote("/opt/my apps/rustpad"), "\"/opt/my apps/rustpad\"");
        assert_eq!(exec_quote("/opt/$x/100%/rustpad"), "\"/opt/\\$x/100%%/rustpad\"");
    }
}
