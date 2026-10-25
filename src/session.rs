//! Session persistence, like notepad++: open tabs and their unsaved changes
//! survive closing the app (and crashes, thanks to periodic writes).
//!
//! Layout in `$XDG_STATE_HOME/rustpad` (default `~/.local/state/rustpad`):
//! `session.conf` lists the tabs, `backup/b<id>.bak` holds unsaved content
//! (see `Buffer::write_backup`). Only one process owns the session, guarded
//! by a lock file.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::doc::Buffer;
use crate::doc::buffer::LineEnding;
use crate::doc::encoding::Encoding;

static LOCK: OnceLock<File> = OnceLock::new();
static WRITE: Mutex<()> = Mutex::new(());

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("rustpad"))
}

fn backup_dir() -> Option<PathBuf> {
    dir().map(|d| d.join("backup"))
}

/// Become the session owner. Fails if another instance already is.
pub fn acquire() -> bool {
    let Some(d) = dir() else { return false };
    if std::fs::create_dir_all(d.join("backup")).is_err() {
        return false;
    }
    let Ok(file) = File::options().create(true).truncate(false).write(true).open(d.join("session.lock")) else {
        return false;
    };
    if file.try_lock().is_err() {
        return false;
    }
    LOCK.set(file).is_ok()
}

pub fn is_owner() -> bool {
    LOCK.get().is_some()
}

pub struct TabState {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub title: Option<String>,
    pub mode: &'static str,
    pub cursor: u64,
    pub top: u64,
    /// Snapshot of a dirty buffer whose backup must be (re)written.
    pub write_backup: Option<Buffer>,
    /// The tab has unsaved changes (an existing backup file stays valid).
    pub dirty: bool,
    pub encoding: Encoding,
    pub bom: bool,
    /// Line endings that saving will convert to.
    pub convert_eol: Option<LineEnding>,
}

pub struct Session {
    pub tabs: Vec<TabState>,
    pub active: usize,
}

pub fn write(session: &Session) -> io::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(dir), Some(bdir)) = (dir(), backup_dir()) else { return Ok(()) };
    std::fs::create_dir_all(&bdir)?;

    for t in &session.tabs {
        if let Some(buf) = &t.write_backup {
            let path = bdir.join(format!("b{}.bak", t.id));
            let tmp = bdir.join(format!("b{}.tmp", t.id));
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
            buf.write_backup(&mut w)?;
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            std::fs::rename(&tmp, &path)?;
        }
    }

    let mut out = format!("# rustpad++ session\nactive={}\n", session.active);
    for t in &session.tabs {
        out.push_str("[tab]\n");
        out.push_str(&format!("id={}\nmode={}\ncursor={}\ntop={}\n", t.id, t.mode, t.cursor, t.top));
        if let Some(p) = &t.path {
            out.push_str(&format!("path={}\n", p.display()));
        }
        if let Some(n) = &t.title {
            out.push_str(&format!("title={n}\n"));
        }
        if t.dirty {
            out.push_str(&format!("backup=b{}.bak\n", t.id));
        }
        out.push_str(&format!("encoding={}\nbom={}\n", t.encoding.id(), t.bom));
        if let Some(eol) = t.convert_eol {
            out.push_str(&format!("convert_eol={}\n", eol.label()));
        }
    }
    let tmp = dir.join("session.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, dir.join("session.conf"))?;

    // Drop backups no longer referenced (saved or closed tabs).
    let keep: Vec<String> = session.tabs.iter().filter(|t| t.dirty).map(|t| format!("b{}.bak", t.id)).collect();
    if let Ok(entries) = std::fs::read_dir(&bdir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".bak") && !keep.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

pub struct LoadedTab {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub title: Option<String>,
    pub mode: String,
    pub cursor: u64,
    pub top: u64,
    pub backup: Option<PathBuf>,
    /// Missing in sessions from before encodings were selectable.
    pub encoding: Option<Encoding>,
    pub bom: Option<bool>,
    pub convert_eol: Option<LineEnding>,
}

pub fn load() -> (Vec<LoadedTab>, usize) {
    let (Some(dir), Some(bdir)) = (dir(), backup_dir()) else { return (Vec::new(), 0) };
    let Ok(text) = std::fs::read_to_string(dir.join("session.conf")) else { return (Vec::new(), 0) };
    let mut tabs: Vec<LoadedTab> = Vec::new();
    let mut active = 0;
    for line in text.lines() {
        if line == "[tab]" {
            tabs.push(LoadedTab {
                id: 0,
                path: None,
                title: None,
                mode: String::new(),
                cursor: 0,
                top: 0,
                backup: None,
                encoding: None,
                bom: None,
                convert_eol: None,
            });
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let Some(t) = tabs.last_mut() else {
            if k == "active" {
                active = v.parse().unwrap_or(0);
            }
            continue;
        };
        match k {
            "id" => t.id = v.parse().unwrap_or(0),
            "mode" => t.mode = v.to_string(),
            "cursor" => t.cursor = v.parse().unwrap_or(0),
            "top" => t.top = v.parse().unwrap_or(0),
            "path" => t.path = Some(PathBuf::from(v)),
            "title" => t.title = Some(v.to_string()),
            "backup" => t.backup = Some(bdir.join(v)),
            "encoding" => t.encoding = Encoding::from_id(v),
            "bom" => t.bom = Some(v == "true"),
            "convert_eol" => t.convert_eol = [LineEnding::Lf, LineEnding::CrLf].into_iter().find(|e| e.label() == v),
            _ => {}
        }
    }
    (tabs, active)
}

/// Keep a backup that could not be restored instead of deleting it.
pub fn reject_backup(path: &std::path::Path) -> Option<PathBuf> {
    let target = path.with_extension(format!(
        "rejected-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    ));
    std::fs::rename(path, &target).ok()?;
    Some(target)
}

/// Write `session` and swallow errors after reporting them on stderr.
pub fn write_logged(session: &Session) {
    if let Err(e) = write(session) {
        let _ = writeln!(io::stderr(), "rustpad: could not write session: {e}");
    }
}
