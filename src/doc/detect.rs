//! Guess what a document is from its name and first bytes.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Json,
    Markdown,
    Csv,
    Binary,
}

pub fn detect(path: Option<&Path>, sample: &[u8]) -> Kind {
    let ext = path
        .and_then(|p| p.extension())
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "json" | "jsonl" | "ndjson" | "geojson" | "jsonc" | "har" | "ipynb" => return Kind::Json,
        "md" | "markdown" | "mdown" | "mkd" => return Kind::Markdown,
        "csv" | "tsv" => return Kind::Csv,
        _ => {}
    }
    if looks_binary(sample) {
        return Kind::Binary;
    }
    match sample.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'{') | Some(b'[') => Kind::Json,
        _ => Kind::Text,
    }
}

pub fn looks_binary(sample: &[u8]) -> bool {
    if sample.is_empty() {
        return false;
    }
    if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let suspicious = sample
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    suspicious * 20 > sample.len() || std::str::from_utf8(sample).is_err() && {
        // Allow a truncated multi-byte sequence at the end of the sample.
        let valid = sample.utf8_chunks().map(|c| c.invalid().len()).sum::<usize>();
        valid * 10 > sample.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(detect(None, b"  {\"a\":1}"), Kind::Json);
        assert_eq!(detect(Some(Path::new("x.MD")), b"{"), Kind::Markdown);
        assert_eq!(detect(Some(Path::new("x.csv")), b"a;b"), Kind::Csv);
        assert_eq!(detect(None, b"\x7fELF\x02\x01\x01\0\0"), Kind::Binary);
        assert_eq!(detect(None, "héllo".as_bytes()), Kind::Text);
    }
}
