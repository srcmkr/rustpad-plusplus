//! Text encodings of files on disk.
//!
//! Documents are UTF-8 inside rustpad++. UTF-8 files are mapped as they are
//! (a BOM is hidden and written back on save); files in other encodings are
//! decoded once when opened and encoded again when saved.

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
    Iso8859_1,
    Iso8859_15,
}

impl Encoding {
    pub const ALL: [Encoding; 6] = [
        Encoding::Utf8,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::Windows1252,
        Encoding::Iso8859_1,
        Encoding::Iso8859_15,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Windows1252 => "Windows-1252",
            Encoding::Iso8859_1 => "ISO-8859-1",
            Encoding::Iso8859_15 => "ISO-8859-15",
        }
    }

    /// Stable name for the session file.
    pub fn id(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Windows1252 => "windows-1252",
            Encoding::Iso8859_1 => "iso-8859-1",
            Encoding::Iso8859_15 => "iso-8859-15",
        }
    }

    pub fn from_id(id: &str) -> Option<Encoding> {
        Encoding::ALL.into_iter().find(|e| e.id() == id)
    }

    /// The byte order mark of this encoding, empty if it has none.
    pub fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8 => &[0xEF, 0xBB, 0xBF],
            Encoding::Utf16Le => &[0xFF, 0xFE],
            Encoding::Utf16Be => &[0xFE, 0xFF],
            _ => &[],
        }
    }

    pub fn has_bom(self) -> bool {
        !self.bom().is_empty()
    }

    /// Every character can be stored, so conversion never loses text.
    pub fn is_unicode(self) -> bool {
        matches!(self, Encoding::Utf8 | Encoding::Utf16Le | Encoding::Utf16Be)
    }

    fn legacy(self) -> Option<&'static encoding_rs::Encoding> {
        match self {
            Encoding::Windows1252 => Some(encoding_rs::WINDOWS_1252),
            Encoding::Iso8859_15 => Some(encoding_rs::ISO_8859_15),
            _ => None,
        }
    }
}

/// Guess the encoding of a file from its first bytes: (encoding, has BOM).
pub fn sniff(sample: &[u8]) -> (Encoding, bool) {
    for e in [Encoding::Utf8, Encoding::Utf16Le, Encoding::Utf16Be] {
        if sample.starts_with(e.bom()) {
            return (e, true);
        }
    }
    // Valid UTF-8, allowing a sequence cut off at the end of the sample.
    let mut chunks = sample.utf8_chunks().peekable();
    let mut valid = true;
    while let Some(c) = chunks.next() {
        if !c.invalid().is_empty() && (chunks.peek().is_some() || c.invalid().len() > 3) {
            valid = false;
        }
    }
    // Binary data stays "UTF-8": it is shown in hex anyway.
    if valid || sample.contains(&0) { (Encoding::Utf8, false) } else { (Encoding::Windows1252, false) }
}

/// Decode a whole file (BOM already removed) into UTF-8.
pub fn decode(bytes: &[u8], enc: Encoding) -> String {
    match enc {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Encoding::Utf16Le => encoding_rs::UTF_16LE.decode_without_bom_handling(bytes).0.into_owned(),
        Encoding::Utf16Be => encoding_rs::UTF_16BE.decode_without_bom_handling(bytes).0.into_owned(),
        Encoding::Iso8859_1 => bytes.iter().map(|&b| b as char).collect(),
        Encoding::Windows1252 | Encoding::Iso8859_15 => {
            enc.legacy().unwrap().decode_without_bom_handling(bytes).0.into_owned()
        }
    }
}

/// The first character of `text` that `enc` cannot store.
pub fn first_unmappable(text: &str, enc: Encoding) -> Option<char> {
    let mut sink = Vec::new();
    let mut w = Encoder::new(enc, &mut sink);
    match w.write_utf8(text.as_bytes()) {
        Err(Unmappable(c)) => Some(c),
        Ok(()) => None,
    }
}

/// A character the target encoding cannot represent.
#[derive(Debug)]
pub struct Unmappable(pub char);

/// Streams UTF-8 into `enc`. Input may be split anywhere, also inside a
/// character; invalid UTF-8 becomes U+FFFD.
pub struct Encoder<W: Write> {
    enc: Encoding,
    out: W,
    carry: Vec<u8>,
    legacy: Option<encoding_rs::Encoder>,
    buf: Vec<u8>,
    error: Option<io::Error>,
}

impl<W: Write> Encoder<W> {
    pub fn new(enc: Encoding, out: W) -> Self {
        Encoder { enc, out, carry: Vec::new(), legacy: enc.legacy().map(|e| e.new_encoder()), buf: Vec::new(), error: None }
    }

    pub fn write_utf8(&mut self, bytes: &[u8]) -> Result<(), Unmappable> {
        if self.enc == Encoding::Utf8 {
            return self.emit(bytes);
        }
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut chunks = data.utf8_chunks().peekable();
        while let Some(c) = chunks.next() {
            self.encode_str(c.valid())?;
            let invalid = c.invalid();
            if invalid.is_empty() {
                continue;
            }
            // An incomplete sequence at the very end may continue in the next call.
            if chunks.peek().is_none() && invalid.len() < 4 && starts_sequence(invalid) {
                self.carry = invalid.to_vec();
            } else {
                self.encode_str("\u{FFFD}")?;
            }
        }
        Ok(())
    }

    /// Flush the rest; returns the writer, or the first I/O error.
    pub fn finish(mut self) -> Result<W, FinishError> {
        if !self.carry.is_empty() {
            self.carry.clear();
            self.encode_str("\u{FFFD}").map_err(FinishError::Unmappable)?;
        }
        match self.error {
            Some(e) => Err(FinishError::Io(e)),
            None => Ok(self.out),
        }
    }

    fn encode_str(&mut self, s: &str) -> Result<(), Unmappable> {
        if s.is_empty() {
            return Ok(());
        }
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        match self.enc {
            Encoding::Utf8 => buf.extend_from_slice(s.as_bytes()),
            Encoding::Utf16Le => s.encode_utf16().for_each(|u| buf.extend_from_slice(&u.to_le_bytes())),
            Encoding::Utf16Be => s.encode_utf16().for_each(|u| buf.extend_from_slice(&u.to_be_bytes())),
            Encoding::Iso8859_1 => {
                for c in s.chars() {
                    buf.push(u8::try_from(c as u32).map_err(|_| Unmappable(c))?);
                }
            }
            Encoding::Windows1252 | Encoding::Iso8859_15 => {
                let encoder = self.legacy.as_mut().unwrap();
                let mut src = s;
                loop {
                    let start = buf.len();
                    buf.resize(start + src.len() + 16, 0);
                    let (result, read, written) =
                        encoder.encode_from_utf8_without_replacement(src, &mut buf[start..], false);
                    buf.truncate(start + written);
                    src = &src[read..];
                    match result {
                        encoding_rs::EncoderResult::InputEmpty => break,
                        encoding_rs::EncoderResult::OutputFull => continue,
                        encoding_rs::EncoderResult::Unmappable(c) => return Err(Unmappable(c)),
                    }
                }
            }
        }
        let r = self.emit(&buf);
        self.buf = buf;
        r
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), Unmappable> {
        if self.error.is_none()
            && let Err(e) = self.out.write_all(bytes)
        {
            self.error = Some(e);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum FinishError {
    Io(io::Error),
    Unmappable(Unmappable),
}

fn starts_sequence(bytes: &[u8]) -> bool {
    let need = match bytes[0] {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return false,
    };
    bytes.len() < need && bytes[1..].iter().all(|b| b & 0xC0 == 0x80)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(text: &[u8], enc: Encoding, split: usize) -> Result<Vec<u8>, Unmappable> {
        let mut w = Encoder::new(enc, Vec::new());
        for part in text.chunks(split) {
            w.write_utf8(part)?;
        }
        w.finish().map_err(|e| match e {
            FinishError::Unmappable(u) => u,
            FinishError::Io(e) => panic!("{e}"),
        })
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"\xEF\xBB\xBFhi"), (Encoding::Utf8, true));
        assert_eq!(sniff(b"\xFF\xFEh\0"), (Encoding::Utf16Le, true));
        assert_eq!(sniff(b"\xFE\xFF\0h"), (Encoding::Utf16Be, true));
        assert_eq!(sniff("Grüße".as_bytes()), (Encoding::Utf8, false));
        // Cut inside "ü" at the end of the sample.
        assert_eq!(sniff(&"Grü".as_bytes()[..3]), (Encoding::Utf8, false));
        assert_eq!(sniff(b"Gr\xFC\xDFe"), (Encoding::Windows1252, false));
        assert_eq!(sniff(b"\x7fELF\0\0"), (Encoding::Utf8, false));
    }

    #[test]
    fn round_trips() {
        // Split every 3 bytes, so also inside characters.
        let text = "Grüße, naïve Fußgänger";
        for enc in Encoding::ALL {
            let bytes = encode(text.as_bytes(), enc, 3).unwrap();
            assert_eq!(decode(&bytes, enc), text, "{enc:?}");
        }
        assert_eq!(encode("Grüße".as_bytes(), Encoding::Windows1252, 1).unwrap(), b"Gr\xFC\xDFe");
        assert_eq!(encode("€".as_bytes(), Encoding::Iso8859_15, 2).unwrap(), b"\xA4");
        assert_eq!(encode("hi".as_bytes(), Encoding::Utf16Be, 1).unwrap(), b"\0h\0i");
    }

    #[test]
    fn unmappable_characters() {
        assert_eq!(first_unmappable("ok €", Encoding::Iso8859_1), Some('€'));
        assert_eq!(first_unmappable("😀", Encoding::Windows1252), Some('😀'));
        assert!(first_unmappable("äöü", Encoding::Iso8859_15).is_none());
        assert!(matches!(encode("a😀".as_bytes(), Encoding::Windows1252, 2), Err(Unmappable('😀'))));
    }
}
