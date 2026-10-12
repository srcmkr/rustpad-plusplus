//! rustpad++ as the default application for file types (XDG MIME defaults).
//!
//! The current default comes from `xdg-mime query default`, which follows the
//! full XDG lookup (user, desktop and system `mimeapps.list`, caches).
//! Changes go into `$XDG_CONFIG_HOME/mimeapps.list` like `xdg-mime default`
//! does; switching off removes only our own entry, so the system default
//! applies again.

use std::path::PathBuf;
use std::process::Command;

pub const DESKTOP_ID: &str = "rustpad.desktop";

pub struct FileType {
    pub extensions: &'static str,
    pub label: &'static str,
    /// MIME types to claim; the first one is shown.
    pub mimes: &'static [&'static str],
}

pub const TYPES: &[FileType] = &[
    FileType { extensions: ".txt", label: n_!("Text files"), mimes: &["text/plain"] },
    FileType { extensions: ".log", label: n_!("Log files"), mimes: &["text/x-log"] },
    FileType { extensions: ".md", label: "Markdown", mimes: &["text/markdown"] },
    FileType { extensions: ".json", label: "JSON", mimes: &["application/json"] },
    FileType { extensions: ".xml", label: "XML", mimes: &["application/xml"] },
    FileType { extensions: ".yaml .yml", label: "YAML", mimes: &["application/yaml"] },
    FileType { extensions: ".toml", label: "TOML", mimes: &["application/toml"] },
    FileType { extensions: ".csv", label: "CSV", mimes: &["text/csv"] },
    FileType { extensions: ".sh", label: n_!("Shell scripts"), mimes: &["application/x-shellscript"] },
    FileType { extensions: ".py", label: "Python", mimes: &["text/x-python3", "text/x-python"] },
    FileType { extensions: ".rs", label: "Rust", mimes: &["text/rust"] },
];

/// `MimeType=` value for the desktop entry, so we show up under "Open with".
pub fn mime_list() -> String {
    TYPES.iter().flat_map(|t| t.mimes).map(|m| format!("{m};")).collect()
}

/// Desktop id of the current default application for each of [`TYPES`].
/// Spawns one `xdg-mime` per type; call it off the UI thread.
pub fn query_defaults() -> Vec<Option<String>> {
    TYPES
        .iter()
        .map(|t| {
            let out = Command::new("xdg-mime").args(["query", "default", t.mimes[0]]).output().ok()?;
            let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!id.is_empty()).then_some(id)
        })
        .collect()
}

/// Display name of an installed application, from its desktop file.
pub fn app_name(desktop_id: &str) -> Option<String> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let home = env("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = env("XDG_DATA_HOME").map(PathBuf::from).or(home.map(|h| h.join(".local/share"))).into_iter().collect();
    let system = env("XDG_DATA_DIRS").map(|d| d.to_string_lossy().into_owned()).unwrap_or("/usr/local/share:/usr/share".into());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    // Desktop ids map "-" in the id to subdirectories ("kde4-foo.desktop").
    let text = dirs.iter().find_map(|d| {
        let apps = d.join("applications");
        std::fs::read_to_string(apps.join(desktop_id))
            .or_else(|_| std::fs::read_to_string(apps.join(desktop_id.replacen('-', "/", 1))))
            .ok()
    })?;
    text.lines()
        .skip_while(|l| l.trim() != "[Desktop Entry]")
        .find_map(|l| l.strip_prefix("Name="))
        .map(|n| n.trim().to_string())
}

fn mimeapps_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("mimeapps.list"))
}

/// Make rustpad++ the default for `t` (`on`), or drop our default again.
pub fn set_default(t: &FileType, on: bool) -> std::io::Result<()> {
    let path = mimeapps_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let new = edit_mimeapps(&old, t.mimes, on);
    if new != old {
        let tmp = path.with_extension("list.rustpad-tmp");
        std::fs::write(&tmp, new)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Set or remove `mime=rustpad.desktop` lines in `[Default Applications]`,
/// leaving everything else as it was.
fn edit_mimeapps(text: &str, mimes: &[&str], on: bool) -> String {
    const SECTION: &str = "[Default Applications]";
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = match lines.iter().position(|l| l.trim() == SECTION) {
        Some(i) => i,
        None => {
            if !on {
                return text.to_string();
            }
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(SECTION.to_string());
            lines.len() - 1
        }
    };
    let end = lines[start + 1..].iter().position(|l| l.trim_start().starts_with('[')).map_or(lines.len(), |i| start + 1 + i);
    let mut section: Vec<String> = lines[start + 1..end].to_vec();
    for mime in mimes {
        let key = format!("{mime}=");
        let existing = section.iter().position(|l| l.trim_start().starts_with(&key));
        match (on, existing) {
            (true, Some(i)) => section[i] = format!("{key}{DESKTOP_ID}"),
            (true, None) => {
                // Keep a trailing blank line between sections.
                let at = section.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
                section.insert(at, format!("{key}{DESKTOP_ID}"));
            }
            (false, Some(i)) if section[i].trim_start()[key.len()..].split(';').next() == Some(DESKTOP_ID) => {
                section.remove(i);
            }
            (false, _) => {}
        }
    }
    lines.splice(start + 1..end, section);
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_only_our_entries() {
        let text = "[Default Applications]\ntext/html=tabby.desktop\ntext/markdown=viewer.desktop\n\n[Added Associations]\ntext/markdown=viewer.desktop;\n";
        let on = edit_mimeapps(text, &["text/markdown", "text/csv"], true);
        assert_eq!(
            on,
            "[Default Applications]\ntext/html=tabby.desktop\ntext/markdown=rustpad.desktop\ntext/csv=rustpad.desktop\n\n[Added Associations]\ntext/markdown=viewer.desktop;\n"
        );
        let off = edit_mimeapps(&on, &["text/markdown", "text/csv", "text/html"], false);
        assert_eq!(off, "[Default Applications]\ntext/html=tabby.desktop\n\n[Added Associations]\ntext/markdown=viewer.desktop;\n");
    }

    #[test]
    fn creates_the_section() {
        assert_eq!(edit_mimeapps("", &["text/plain"], true), "[Default Applications]\ntext/plain=rustpad.desktop\n");
        assert_eq!(edit_mimeapps("[Added Associations]\n", &["text/plain"], true), "[Added Associations]\n\n[Default Applications]\ntext/plain=rustpad.desktop\n");
        assert_eq!(edit_mimeapps("", &["text/plain"], false), "");
    }

    #[test]
    fn mime_list_for_desktop_entry() {
        assert!(mime_list().starts_with("text/plain;text/x-log;"));
    }
}
