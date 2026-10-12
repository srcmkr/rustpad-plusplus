//! Currently only support utf8 mode

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use std::io::{self, Write};

const UTF8_START: &[u8] = &[0x1B, 0x25, 0x47];
const UTF8_END: &[u8] = &[0x1B, 0x25, 0x40];

/// Wrapper for reduce allocation
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CText<'s> {
    utf8: &'s str,
}

impl<'s> fmt::Debug for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> fmt::Display for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> CText<'s> {
    pub const fn new(utf8: &'s str) -> Self {
        Self { utf8 }
    }

    pub const fn len(self) -> usize {
        self.utf8.len() + UTF8_START.len() + UTF8_END.len()
    }

    #[cfg(feature = "std")]
    pub fn write(self, mut out: impl Write) -> io::Result<usize> {
        let mut writed = 0;
        writed += out.write(UTF8_START)?;
        writed += out.write(self.utf8.as_bytes())?;
        writed += out.write(UTF8_END)?;
        Ok(writed)
    }
}

/// Encoding utf8 to COMPOUND_TEXT with utf8 escape
pub fn utf8_to_compound_text(text: &str) -> Vec<u8> {
    let mut ret = Vec::with_capacity(text.len() + 6);
    ret.extend_from_slice(UTF8_START);
    ret.extend_from_slice(text.as_bytes());
    ret.extend_from_slice(UTF8_END);
    ret
}

#[derive(Debug, Clone)]
pub enum DecodeError {
    InvalidEncoding,
    UnsupportedEncoding,
    Utf8Error(alloc::string::FromUtf8Error),
}

impl From<alloc::string::FromUtf8Error> for DecodeError {
    fn from(err: alloc::string::FromUtf8Error) -> Self {
        DecodeError::Utf8Error(err)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "Invalid compound text"),
            Self::UnsupportedEncoding => write!(f, "This encoding is not supported yet"),
            Self::Utf8Error(e) => write!(f, "Not a valid utf8 {}", e),
        }
    }
}

macro_rules! decode {
    ($decoder:expr, $out:expr, $bytes:expr, $last:expr) => {
        loop {
            let (ret, _, _) = $decoder.decode_to_string($bytes, $out, $last);

            match ret {
                encoding_rs::CoderResult::InputEmpty => break,
                encoding_rs::CoderResult::OutputFull => {
                    $out.reserve(
                        $decoder
                            .max_utf8_buffer_length($bytes.len())
                            .unwrap_or_default(),
                    );
                }
            }
        }
    };
}

/// Decode COMPOUND_TEXT.
///
/// Patched for rustpad++: the original treated escape-free text as UTF-8,
/// but per the X11 COMPOUND_TEXT spec the initial state is ISO 8859-1
/// (ASCII in GL, Latin-1 right half in GR). ibus commits umlauts that way
/// (e.g. `ü` = 0xFC), which made the old decoder fail and zed-xim panic.
/// This decoder tracks the common designations and never fails on input it
/// does not understand: such bytes fall back to Latin-1.
pub fn compound_text_to_utf8(bytes: &[u8]) -> Result<String, DecodeError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Latin1,
        Utf8,
        Jp,
    }
    let mut out = String::with_capacity(bytes.len());
    let mut mode = Mode::Latin1;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i..] {
            // ESC % G ... ESC % @ : UTF-8 segment
            [0x1B, 0x25, 0x47, ..] => {
                mode = Mode::Utf8;
                i += 3;
            }
            [0x1B, 0x25, 0x40, ..] => {
                mode = Mode::Latin1;
                i += 3;
            }
            // ESC $ ( B : JIS X 0208 in GL
            [0x1B, 0x24, 0x28, 0x42, ..] => {
                mode = Mode::Jp;
                i += 4;
            }
            // ESC ( B (ASCII GL), ESC - A (Latin-1 GR): back to the default
            [0x1B, 0x28, 0x42, ..] | [0x1B, 0x2D, 0x41, ..] => {
                mode = Mode::Latin1;
                i += 3;
            }
            // Any other escape: skip it (intermediate bytes 0x20..0x2F + final byte).
            [0x1B, ..] => {
                i += 1;
                while i < bytes.len() && (0x20..0x30).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            _ => {
                let end = bytes[i..].iter().position(|&b| b == 0x1B).map(|p| i + p).unwrap_or(bytes.len());
                let seg = &bytes[i..end];
                match mode {
                    Mode::Latin1 => out.extend(seg.iter().map(|&b| b as char)),
                    Mode::Utf8 => out.push_str(&String::from_utf8_lossy(seg)),
                    Mode::Jp => {
                        let mut decoder = encoding_rs::ISO_2022_JP.new_decoder_without_bom_handling();
                        decode!(decoder, &mut out, &[0x1B, 0x24, 0x42], false);
                        decode!(decoder, &mut out, seg, true);
                    }
                }
                i = end;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn latin1_default() {
        // What ibus sends for "ü" and "Grüße".
        assert_eq!(crate::compound_text_to_utf8(&[252]).unwrap(), "ü");
        assert_eq!(crate::compound_text_to_utf8(&[71, 114, 252, 223, 101]).unwrap(), "Grüße");
        assert_eq!(crate::compound_text_to_utf8(b"abc").unwrap(), "abc");
        // Unknown escape is skipped, never an error.
        assert_eq!(crate::compound_text_to_utf8(&[0x1B, 0x24, 0x29, 0x41, 0x61]).unwrap(), "a");
    }

    #[test]
    fn korean() {
        const UTF8: &str = "가나다";
        const COMP: &[u8] = &[
            27, 37, 71, 234, 176, 128, 235, 130, 152, 235, 139, 164, 27, 37, 64,
        ];
        assert_eq!(crate::utf8_to_compound_text(UTF8), COMP);
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }

    #[test]
    fn iso_2011_jp() {
        const UTF8: &str = "東京";
        const COMP: &[u8] = &[27, 36, 40, 66, 69, 108, 53, 126];
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }
}
