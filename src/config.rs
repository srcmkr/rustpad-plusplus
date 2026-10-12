//! Tiny `key=value` settings file in `$XDG_CONFIG_HOME/rustpad/settings.conf`.
//! Deliberately dependency-free: reading it costs microseconds at startup.

use std::path::PathBuf;

#[derive(Default)]
pub struct Config {
    entries: Vec<(String, String)>,
    pub recent: Vec<PathBuf>,
}

pub(crate) fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rustpad").join("settings.conf"))
}

impl Config {
    pub fn load() -> Config {
        let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Self::parse(&text)
    }

    fn parse(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if k == "recent" {
                    c.recent.push(PathBuf::from(v));
                } else {
                    c.set(k, v);
                }
            }
        }
        c
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, key: &str, value: &str) {
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value.to_string(),
            None => self.entries.push((key.to_string(), value.to_string())),
        }
    }

    fn serialize(&self) -> String {
        let mut out = String::from("# rustpad++ settings\n");
        for (k, v) in &self.entries {
            out.push_str(&format!("{k}={v}\n"));
        }
        for r in &self.recent {
            out.push_str(&format!("recent={}\n", r.display()));
        }
        out
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("conf.tmp");
        if std::fs::write(&tmp, self.serialize()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.set("theme", "Hell");
        c.set("font_size", "15");
        c.recent = vec![PathBuf::from("/a b/c.txt")];
        let back = Config::parse(&c.serialize());
        assert_eq!(back.get("theme"), Some("Hell"));
        assert_eq!(back.get("font_size"), Some("15"));
        assert_eq!(back.recent, c.recent);
    }
}
