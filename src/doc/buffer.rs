//! Piece table over a memory-mapped file.
//!
//! The original file is never read into memory: it is mapped read-only and
//! only the pages that are actually displayed or scanned get faulted in.
//! Edits go into an append-only `add` buffer; the document is the sequence of
//! pieces referencing either buffer.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use memmap2::Mmap;

use super::encoding::{self, Encoding, Encoder, FinishError, Unmappable};
use super::line_index::LineIndex;

pub enum Source {
    Empty,
    Mapped(Mmap),
    /// Content restored from a session backup.
    Owned(Vec<u8>),
}

impl Source {
    pub fn open(path: &Path) -> io::Result<Source> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(Source::Empty);
        }
        // SAFETY: the map is read-only. If another process truncates the file
        // while it is open, accesses past the new end fault, which is the same
        // trade-off every mmap-based viewer makes.
        let map = unsafe { Mmap::map(&file)? };
        Ok(Source::Mapped(map))
    }

    #[inline]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Source::Empty => &[],
            Source::Mapped(m) => m,
            Source::Owned(v) => v,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Buf {
    Orig,
    Add,
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    buf: Buf,
    start: u64,
    len: u64,
}

#[derive(Clone, Debug)]
enum Op {
    /// `len` bytes were inserted at `at`.
    Insert { at: u64, len: u64, pieces: Vec<Piece> },
    /// The pieces were removed from `at`.
    Delete { at: u64, pieces: Vec<Piece> },
}

#[derive(Clone, Debug)]
struct UndoGroup {
    ops: Vec<Op>,
    cursor_before: u64,
    cursor_after: u64,
    time: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// Rewrites every line ending of a stream to one kind, chunk by chunk; a
/// "\r\n" split between two chunks is still one line ending.
struct EolConverter {
    to: LineEnding,
    /// The previous chunk ended in "\r" (not yet written for `Lf`).
    cr: bool,
}

impl EolConverter {
    fn new(to: LineEnding) -> Self {
        EolConverter { to, cr: false }
    }

    fn convert(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.reserve(chunk.len() + chunk.len() / 16);
        for &b in chunk {
            match self.to {
                LineEnding::Lf => {
                    if self.cr && b != b'\n' {
                        out.push(b'\r');
                    }
                    self.cr = b == b'\r';
                    if !self.cr {
                        out.push(b);
                    }
                }
                LineEnding::CrLf => {
                    if b == b'\n' && !self.cr {
                        out.push(b'\r');
                    }
                    out.push(b);
                    self.cr = b == b'\r';
                }
            }
        }
    }

    /// What is still held back at the end of the stream.
    fn finish(&self) -> &'static [u8] {
        if self.to == LineEnding::Lf && self.cr { b"\r" } else { b"" }
    }
}

pub struct Buffer {
    orig: Arc<Source>,
    add: Vec<u8>,
    pieces: Vec<Piece>,
    /// `starts[i]` is the document offset of `pieces[i]`.
    starts: Vec<u64>,
    len: u64,
    undo: Vec<UndoGroup>,
    redo: Vec<UndoGroup>,
    version: u64,
    saved_version: u64,
    pub path: Option<PathBuf>,
    /// What Enter inserts; with `convert_eol`, also what saving writes.
    pub line_ending: LineEnding,
    /// Rewrite every line ending to `line_ending` when saving.
    pub convert_eol: bool,
    index: Arc<LineIndex>,
    /// Name shown for untitled documents ("new 3").
    pub title: Option<String>,
    /// Size and mtime of the original file when it was opened; a session
    /// backup that stores only the edits is valid only against this file.
    stamp: Option<(u64, u128)>,
    /// Last known state of the file on disk (size, mtime, inode), to notice
    /// external changes.
    disk: Option<DiskState>,
    /// Encoding of the file on disk; the document itself is UTF-8.
    pub encoding: Encoding,
    /// Write a byte order mark when saving.
    pub bom: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DiskState {
    len: u64,
    mtime: u128,
    ino: u64,
}

fn disk_state(path: &Path) -> Option<DiskState> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some(DiskState { len: m.len(), mtime, ino: m.ino() })
}

/// How the file on disk differs from what the buffer was loaded from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiskChange {
    /// Same file, larger: typically a log being appended to.
    Grown,
    /// Same file, smaller: the mapping is no longer fully readable.
    Truncated,
    /// Same file and size, rewritten in place.
    Modified,
    /// Replaced by a new file (editors usually save this way).
    Replaced,
    /// Deleted or moved away.
    Missing,
}

/// Originals up to this size are backed up in full; larger ones only as a
/// diff against the file on disk.
const FULL_BACKUP_MAX: u64 = 64 << 20;
const BACKUP_MAGIC: &[u8; 4] = b"RPB1";
/// Files in other encodings than UTF-8 are decoded into memory; larger ones
/// are refused (or, when only guessed, shown as UTF-8).
const TRANSCODE_MAX: u64 = 256 << 20;

fn file_stamp(path: &Path) -> Option<(u64, u128)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some((m.len(), mtime))
}

fn put_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn get_u64(data: &[u8], pos: &mut usize) -> io::Result<u64> {
    let bytes = data
        .get(*pos..*pos + 8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated backup"))?;
    *pos += 8;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

impl Buffer {
    pub fn empty() -> Buffer {
        Self::from_source(Arc::new(Source::Empty), None)
    }

    pub fn open(path: &Path) -> io::Result<Buffer> {
        Self::open_with(path, None)
    }

    /// Open as `forced` encoding, or guess it. UTF-8 stays memory-mapped;
    /// other encodings are decoded into memory.
    pub fn open_with(path: &Path, forced: Option<Encoding>) -> io::Result<Buffer> {
        let stamp = file_stamp(path);
        let source = Source::open(path)?;
        let bytes = source.bytes();
        let len = bytes.len() as u64;
        let head = &bytes[..bytes.len().min(64 * 1024)];
        let (enc, bom) = match forced {
            Some(e) => (e, e.has_bom() && head.starts_with(e.bom())),
            None => encoding::sniff(head),
        };
        let skip = if bom { enc.bom().len() as u64 } else { 0 };
        if enc == Encoding::Utf8 || (forced.is_none() && len > TRANSCODE_MAX) {
            let mut b = Self::from_source(Arc::new(source), Some(path.to_path_buf()));
            if skip > 0 {
                b.pieces = if len > skip { vec![Piece { buf: Buf::Orig, start: skip, len: len - skip }] } else { Vec::new() };
                b.reindex();
            }
            b.bom = bom && enc == Encoding::Utf8;
            b.stamp = stamp;
            b.disk = disk_state(path);
            return Ok(b);
        }
        if len > TRANSCODE_MAX {
            return Err(io::Error::other(tr!(
                "too large to open as {encoding} (at most {max})",
                encoding = enc.label(),
                max = "256 MiB"
            )));
        }
        let text = encoding::decode(&bytes[skip as usize..], enc);
        let mut b = Self::from_source(Arc::new(Source::Owned(text.into_bytes())), Some(path.to_path_buf()));
        b.encoding = enc;
        b.bom = bom;
        // No stamp: the pieces refer to the decoded text, not to the file, so
        // a session backup must hold the full content.
        b.disk = disk_state(path);
        Ok(b)
    }

    /// Save in another encoding from now on (marks the document modified).
    pub fn convert(&mut self, enc: Encoding, bom: bool) {
        self.encoding = enc;
        self.bom = bom && enc.has_bom();
        self.version += 1;
    }

    /// Save with only `to` line endings from now on (marks the document
    /// modified). The text keeps its line endings until then, so this is
    /// instant for files of any size.
    pub fn convert_line_endings(&mut self, to: LineEnding) {
        self.line_ending = to;
        self.convert_eol = true;
        self.version += 1;
    }

    /// Test helper and "new from text".
    pub fn from_text(text: &str) -> Buffer {
        let mut b = Self::empty();
        b.add.extend_from_slice(text.as_bytes());
        if !text.is_empty() {
            b.pieces.push(Piece { buf: Buf::Add, start: 0, len: text.len() as u64 });
        }
        b.reindex();
        b
    }

    fn from_source(orig: Arc<Source>, path: Option<PathBuf>) -> Buffer {
        let len = orig.bytes().len() as u64;
        let pieces = if len > 0 {
            vec![Piece { buf: Buf::Orig, start: 0, len }]
        } else {
            Vec::new()
        };
        let line_ending = detect_line_ending(&orig.bytes()[..orig.bytes().len().min(64 * 1024)]);
        let index = LineIndex::spawn(orig.clone());
        let mut b = Buffer {
            orig,
            add: Vec::new(),
            pieces,
            starts: Vec::new(),
            len,
            undo: Vec::new(),
            redo: Vec::new(),
            version: 0,
            saved_version: 0,
            path,
            line_ending,
            convert_eol: false,
            index,
            title: None,
            stamp: None,
            disk: None,
            encoding: Encoding::Utf8,
            bom: false,
        };
        b.reindex();
        b
    }

    fn reindex(&mut self) {
        self.starts.clear();
        let mut acc = 0;
        for p in &self.pieces {
            self.starts.push(acc);
            acc += p.len;
        }
        self.len = acc;
    }

    #[inline]
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.index
    }

    pub fn file_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| "new".to_string())
    }

    #[inline]
    fn piece_bytes(&self, p: &Piece) -> &[u8] {
        let src = match p.buf {
            Buf::Orig => self.orig.bytes(),
            Buf::Add => &self.add,
        };
        &src[p.start as usize..(p.start + p.len) as usize]
    }

    /// Index of the piece containing `offset` (offset < len).
    fn piece_at(&self, offset: u64) -> usize {
        self.starts.partition_point(|&s| s <= offset) - 1
    }

    /// The contiguous bytes starting at `offset` up to the end of its piece.
    pub fn chunk_at(&self, offset: u64) -> &[u8] {
        if offset >= self.len {
            return &[];
        }
        let i = self.piece_at(offset);
        let bytes = self.piece_bytes(&self.pieces[i]);
        &bytes[(offset - self.starts[i]) as usize..]
    }

    /// The contiguous bytes ending at `offset`, starting at its piece start.
    pub fn chunk_before(&self, offset: u64) -> &[u8] {
        if offset == 0 || self.len == 0 {
            return &[];
        }
        let i = self.piece_at(offset - 1);
        let bytes = self.piece_bytes(&self.pieces[i]);
        &bytes[..(offset - self.starts[i]) as usize]
    }

    #[inline]
    pub fn byte_at(&self, offset: u64) -> Option<u8> {
        self.chunk_at(offset).first().copied()
    }

    /// Bytes of `range`; borrowed when it lies within a single piece.
    pub fn slice(&self, range: Range<u64>) -> Cow<'_, [u8]> {
        let end = range.end.min(self.len);
        let start = range.start.min(end);
        if start == end {
            return Cow::Borrowed(&[]);
        }
        let first = self.chunk_at(start);
        let want = (end - start) as usize;
        if first.len() >= want {
            return Cow::Borrowed(&first[..want]);
        }
        let mut out = Vec::with_capacity(want);
        let mut pos = start;
        while pos < end {
            let c = self.chunk_at(pos);
            let take = c.len().min((end - pos) as usize);
            out.extend_from_slice(&c[..take]);
            pos += take as u64;
        }
        Cow::Owned(out)
    }

    pub fn text(&self, range: Range<u64>) -> String {
        String::from_utf8_lossy(&self.slice(range)).into_owned()
    }

    /// Find the next occurrence of `byte` at or after `from`, scanning at most `limit` bytes.
    pub fn find_byte(&self, from: u64, byte: u8, limit: u64) -> Option<u64> {
        let end = from.saturating_add(limit).min(self.len);
        let mut pos = from;
        while pos < end {
            let c = self.chunk_at(pos);
            let c = &c[..c.len().min((end - pos) as usize)];
            if let Some(i) = memchr::memchr(byte, c) {
                return Some(pos + i as u64);
            }
            pos += c.len() as u64;
        }
        None
    }

    /// Find the last occurrence of `byte` strictly before `before`, scanning at most `limit` bytes.
    pub fn rfind_byte(&self, before: u64, byte: u8, limit: u64) -> Option<u64> {
        let stop = before.saturating_sub(limit);
        let mut pos = before.min(self.len);
        while pos > stop {
            let c = self.chunk_before(pos);
            let skip = c.len().saturating_sub((pos - stop) as usize);
            let c = &c[skip..];
            if let Some(i) = memchr::memrchr(byte, c) {
                return Some(pos - c.len() as u64 + i as u64);
            }
            pos -= c.len() as u64;
        }
        None
    }

    // ---------------------------------------------------------------- lines

    /// Number of '\n' bytes in `range`, if the line index covers it.
    pub fn count_newlines(&self, range: Range<u64>) -> Option<u64> {
        let end = range.end.min(self.len);
        let mut pos = range.start.min(end);
        let mut total = 0;
        while pos < end {
            let i = self.piece_at(pos);
            let p = self.pieces[i];
            let rel = pos - self.starts[i];
            let take = (p.len - rel).min(end - pos);
            match p.buf {
                Buf::Add => {
                    let b = &self.add[(p.start + rel) as usize..(p.start + rel + take) as usize];
                    total += memchr::memchr_iter(b'\n', b).count() as u64;
                }
                Buf::Orig => {
                    total += self.index.count(self.orig.bytes(), p.start + rel..p.start + rel + take)?;
                }
            }
            pos += take;
        }
        Some(total)
    }

    /// Zero-based line number of `offset`, if known.
    pub fn line_of_offset(&self, offset: u64) -> Option<u64> {
        self.count_newlines(0..offset)
    }

    /// Start offset of zero-based `line`, if the index covers it.
    pub fn offset_of_line(&self, line: u64) -> Option<u64> {
        if line == 0 {
            return Some(0);
        }
        // We need the position after the line-th newline.
        let mut remaining = line;
        for (i, p) in self.pieces.iter().enumerate() {
            let n = match p.buf {
                Buf::Add => memchr::memchr_iter(b'\n', self.piece_bytes(p)).count() as u64,
                Buf::Orig => self.index.count(self.orig.bytes(), p.start..p.start + p.len)?,
            };
            if n >= remaining {
                let rel = match p.buf {
                    Buf::Add => memchr::memchr_iter(b'\n', self.piece_bytes(p))
                        .nth((remaining - 1) as usize)
                        .map(|x| x as u64)?,
                    Buf::Orig => {
                        self.index.nth_newline(self.orig.bytes(), p.start, remaining - 1)? - p.start
                    }
                };
                return Some(self.starts[i] + rel + 1);
            }
            remaining -= n;
        }
        None
    }

    /// Total line count, once the index has finished.
    pub fn line_count(&self) -> Option<u64> {
        self.count_newlines(0..self.len).map(|n| n + 1)
    }

    // ---------------------------------------------------------------- edits

    fn split_at(&mut self, offset: u64) -> usize {
        // Returns index of first piece starting at `offset`.
        if offset >= self.len {
            return self.pieces.len();
        }
        let i = self.piece_at(offset);
        let rel = offset - self.starts[i];
        if rel == 0 {
            return i;
        }
        let p = self.pieces[i];
        self.pieces[i].len = rel;
        self.pieces.insert(i + 1, Piece { buf: p.buf, start: p.start + rel, len: p.len - rel });
        self.reindex();
        i + 1
    }

    fn raw_remove(&mut self, range: Range<u64>) -> Vec<Piece> {
        if range.start >= range.end {
            return Vec::new();
        }
        let a = self.split_at(range.start);
        let b = self.split_at(range.end);
        let removed: Vec<Piece> = self.pieces.drain(a..b).collect();
        self.reindex();
        removed
    }

    fn raw_insert(&mut self, at: u64, pieces: &[Piece]) {
        let i = self.split_at(at);
        // Merge with the previous piece if it is contiguous in the add buffer.
        if pieces.len() == 1 && i > 0 {
            let prev = &mut self.pieces[i - 1];
            let p = pieces[0];
            if prev.buf == p.buf && prev.start + prev.len == p.start {
                prev.len += p.len;
                self.reindex();
                return;
            }
        }
        self.pieces.splice(i..i, pieces.iter().copied());
        self.reindex();
    }

    /// Replace `range` with `text`. `cursor_before` is stored for undo.
    pub fn replace(&mut self, range: Range<u64>, text: &str, cursor_before: u64) -> u64 {
        let range = range.start.min(self.len)..range.end.min(self.len);
        let mut ops = Vec::new();
        if !range.is_empty() {
            let removed = self.raw_remove(range.clone());
            ops.push(Op::Delete { at: range.start, pieces: removed });
        }
        if !text.is_empty() {
            let start = self.add.len() as u64;
            self.add.extend_from_slice(text.as_bytes());
            let piece = Piece { buf: Buf::Add, start, len: text.len() as u64 };
            self.raw_insert(range.start, &[piece]);
            ops.push(Op::Insert { at: range.start, len: piece.len, pieces: vec![piece] });
        }
        let cursor_after = range.start + text.len() as u64;
        if ops.is_empty() {
            return cursor_after;
        }
        self.version += 1;
        self.redo.clear();

        // Coalesce plain typing into one undo step.
        let now = Instant::now();
        if let (Some(last), [Op::Insert { at, len, .. }]) = (self.undo.last_mut(), ops.as_slice())
            && text.len() <= 4
            && !text.contains('\n')
            && now.duration_since(last.time) < Duration::from_millis(1500)
            && last.cursor_after == *at
            && last.ops.iter().all(|o| matches!(o, Op::Insert { .. }))
            && let Some(Op::Insert { len: l, pieces, .. }) = last.ops.last_mut() {
                *l += *len;
                pieces.extend(ops.into_iter().flat_map(|o| match o {
                    Op::Insert { pieces, .. } => pieces,
                    Op::Delete { .. } => Vec::new(),
                }));
                last.cursor_after = cursor_after;
                last.time = now;
                return cursor_after;
            }
        self.undo.push(UndoGroup { ops, cursor_before, cursor_after, time: now });
        cursor_after
    }

    /// Replace every match in `hits` (sorted, non-overlapping) with `text`,
    /// as a single undo step. Rebuilds the affected pieces in one pass, so it
    /// stays linear in the number of hits. Returns the cursor position (start
    /// of the first replacement).
    pub fn replace_all(&mut self, hits: &[Range<u64>], text: &str, cursor_before: u64) -> u64 {
        let (Some(first), Some(last)) = (hits.first(), hits.last()) else { return cursor_before };
        let span = first.start..last.end.min(self.len);
        let removed = self.raw_remove(span.clone());
        // All replacements share one copy of `text` in the add buffer.
        let repl = (!text.is_empty()).then(|| {
            let start = self.add.len() as u64;
            self.add.extend_from_slice(text.as_bytes());
            Piece { buf: Buf::Add, start, len: text.len() as u64 }
        });

        // Walk the removed pieces once, copying the gaps between hits.
        let mut pieces = Vec::with_capacity(hits.len() * 2 + 1);
        let (mut i, mut piece_start) = (0, span.start);
        let mut copy = |from: u64, to: u64, out: &mut Vec<Piece>| {
            while from < to && i < removed.len() {
                let p = removed[i];
                let piece_end = piece_start + p.len;
                let (a, b) = (from.max(piece_start), to.min(piece_end));
                if a < b {
                    out.push(Piece { buf: p.buf, start: p.start + (a - piece_start), len: b - a });
                }
                if piece_end > to {
                    break;
                }
                piece_start = piece_end;
                i += 1;
            }
        };
        let mut prev = span.start;
        for h in hits {
            copy(prev, h.start, &mut pieces);
            pieces.extend(repl);
            prev = h.end;
        }
        copy(prev, span.end, &mut pieces);

        let inserted: u64 = pieces.iter().map(|p| p.len).sum();
        let mut ops = vec![Op::Delete { at: span.start, pieces: removed }];
        if !pieces.is_empty() {
            self.raw_insert(span.start, &pieces);
            ops.push(Op::Insert { at: span.start, len: inserted, pieces });
        }
        self.version += 1;
        self.redo.clear();
        let cursor_after = span.start;
        // Backdated so that typing afterwards never merges into this step.
        let time = Instant::now() - Duration::from_secs(60);
        self.undo.push(UndoGroup { ops, cursor_before, cursor_after, time });
        cursor_after
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Returns the cursor position to restore.
    pub fn undo(&mut self) -> Option<u64> {
        let group = self.undo.pop()?;
        for op in group.ops.iter().rev() {
            match op {
                Op::Insert { at, len, .. } => {
                    self.raw_remove(*at..at + len);
                }
                Op::Delete { at, pieces } => self.raw_insert(*at, pieces),
            }
        }
        self.version += 1;
        let c = group.cursor_before;
        self.redo.push(group);
        Some(c)
    }

    pub fn redo(&mut self) -> Option<u64> {
        let mut group = self.redo.pop()?;
        for op in &group.ops {
            match op {
                Op::Insert { at, pieces, .. } => self.raw_insert(*at, pieces),
                Op::Delete { at, pieces } => {
                    let len: u64 = pieces.iter().map(|p| p.len).sum();
                    self.raw_remove(*at..at + len);
                }
            }
        }
        self.version += 1;
        let c = group.cursor_after;
        group.time = Instant::now() - Duration::from_secs(60);
        self.undo.push(group);
        Some(c)
    }

    // ---------------------------------------------------------------- save

    /// A read-only copy sharing the mapped file, for background work
    /// (search, save). Edits made afterwards do not affect it.
    pub fn snapshot(&self) -> Buffer {
        Buffer {
            orig: self.orig.clone(),
            add: self.add.clone(),
            pieces: self.pieces.clone(),
            starts: self.starts.clone(),
            len: self.len,
            undo: Vec::new(),
            redo: Vec::new(),
            version: self.version,
            saved_version: self.saved_version,
            path: self.path.clone(),
            line_ending: self.line_ending,
            convert_eol: self.convert_eol,
            index: self.index.clone(),
            title: self.title.clone(),
            stamp: self.stamp,
            disk: self.disk,
            encoding: self.encoding,
            bom: self.bom,
        }
    }

    // ---------------------------------------------------------------- backup

    /// Serialize the unsaved state for the session. Small documents are
    /// stored in full; edits to large files only as pieces referring to the
    /// original plus the inserted bytes.
    pub fn write_backup(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(BACKUP_MAGIC)?;
        let orig_len = self.orig.bytes().len() as u64;
        let diff = orig_len > FULL_BACKUP_MAX && self.path.is_some() && self.stamp.is_some();
        if !diff {
            w.write_all(&[0])?;
            put_u64(w, self.len)?;
            for p in &self.pieces {
                w.write_all(self.piece_bytes(p))?;
            }
            return Ok(());
        }
        let (len, mtime) = self.stamp.unwrap();
        w.write_all(&[1])?;
        put_u64(w, len)?;
        w.write_all(&mtime.to_le_bytes())?;
        put_u64(w, self.pieces.len() as u64)?;
        for p in &self.pieces {
            match p.buf {
                Buf::Orig => {
                    w.write_all(&[0])?;
                    put_u64(w, p.start)?;
                    put_u64(w, p.len)?;
                }
                Buf::Add => {
                    w.write_all(&[1])?;
                    put_u64(w, p.len)?;
                    w.write_all(self.piece_bytes(p))?;
                }
            }
        }
        Ok(())
    }

    /// Rebuild a dirty buffer from `write_backup` output. Fails if a diff
    /// backup no longer matches the file on disk.
    pub fn read_backup(path: Option<PathBuf>, data: &[u8]) -> io::Result<Buffer> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        if data.get(..4) != Some(BACKUP_MAGIC) {
            return Err(bad("not a rustpad backup"));
        }
        let mut pos = 5;
        let mut b = match data.get(4) {
            Some(0) => {
                let len = get_u64(data, &mut pos)? as usize;
                let content = data.get(pos..pos + len).ok_or_else(|| bad("truncated backup"))?.to_vec();
                Buffer::from_source(Arc::new(Source::Owned(content)), path.clone())
            }
            Some(1) => {
                let path = path.clone().ok_or_else(|| bad("diff backup without file"))?;
                let len = get_u64(data, &mut pos)?;
                let mtime = u128::from_le_bytes(
                    data.get(pos..pos + 16).ok_or_else(|| bad("truncated backup"))?.try_into().unwrap(),
                );
                pos += 16;
                if file_stamp(&path) != Some((len, mtime)) {
                    return Err(bad("the file was changed on disk since the backup"));
                }
                let mut b = Buffer::open(&path)?;
                let n = get_u64(data, &mut pos)?;
                let mut pieces = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let tag = *data.get(pos).ok_or_else(|| bad("truncated backup"))?;
                    pos += 1;
                    if tag == 0 {
                        let start = get_u64(data, &mut pos)?;
                        let plen = get_u64(data, &mut pos)?;
                        if start + plen > len {
                            return Err(bad("corrupt backup"));
                        }
                        pieces.push(Piece { buf: Buf::Orig, start, len: plen });
                    } else {
                        let plen = get_u64(data, &mut pos)? as usize;
                        let bytes = data.get(pos..pos + plen).ok_or_else(|| bad("truncated backup"))?;
                        pos += plen;
                        pieces.push(Piece { buf: Buf::Add, start: b.add.len() as u64, len: plen as u64 });
                        b.add.extend_from_slice(bytes);
                    }
                }
                b.pieces = pieces;
                b.reindex();
                b
            }
            _ => return Err(bad("unknown backup format")),
        };
        b.path = path;
        b.version = 1;
        b.saved_version = 0;
        Ok(b)
    }

    /// Write the document to `path` via a temp file + rename.
    /// `progress` receives bytes written and may return false to cancel.
    pub fn write_to(&self, path: &Path, mut progress: impl FnMut(u64) -> bool) -> io::Result<()> {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let tmp = dir.join(format!(
            ".{}.rustpad-tmp-{}",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            std::process::id()
        ));
        let result = (|| {
            let file = File::create(&tmp)?;
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = file.set_permissions(meta.permissions());
            }
            let mut w = BufWriter::with_capacity(1 << 20, file);
            if self.bom {
                w.write_all(self.encoding.bom())?;
            }
            let unmappable = |Unmappable(c): Unmappable| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!("the character “{char}” cannot be saved as {encoding}", char = c, encoding = self.encoding.label()),
                )
            };
            let mut enc = Encoder::new(self.encoding, w);
            let mut eol = self.convert_eol.then(|| (EolConverter::new(self.line_ending), Vec::new()));
            let mut written = 0u64;
            for p in &self.pieces {
                for chunk in self.piece_bytes(p).chunks(8 << 20) {
                    match &mut eol {
                        Some((conv, buf)) => {
                            conv.convert(chunk, buf);
                            enc.write_utf8(buf).map_err(unmappable)?;
                        }
                        None => enc.write_utf8(chunk).map_err(unmappable)?,
                    }
                    written += chunk.len() as u64;
                    if !progress(written) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "save cancelled"));
                    }
                }
            }
            if let Some((conv, _)) = &eol {
                enc.write_utf8(conv.finish()).map_err(unmappable)?;
            }
            let w = enc.finish().map_err(|e| match e {
                FinishError::Io(e) => e,
                FinishError::Unmappable(u) => unmappable(u),
            })?;
            let file = w.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Called on the live buffer after a snapshot taken at `version` was
    /// written to `path`. If nothing changed meanwhile, the buffer is
    /// re-based onto the new file so the add buffer can be dropped.
    pub fn finish_save(&mut self, path: &Path, version: u64) {
        self.path = Some(path.to_path_buf());
        if self.version == version
            && let Ok(fresh) = Buffer::open_with(path, Some(self.encoding))
        {
            let line_ending = self.line_ending;
            *self = Buffer { version, saved_version: version, line_ending, ..fresh };
            return;
        }
        // Edited during the save: keep the pieces (the old mapping stays
        // valid even though the file was replaced), mark what was saved.
        // The pieces no longer match the file on disk, so a session backup
        // must not be a diff against it.
        self.stamp = None;
        self.disk = disk_state(path);
        self.saved_version = version;
    }
}

impl Buffer {
    // ---------------------------------------------------------------- disk watch

    /// Compare the file on disk with the state the buffer knows about.
    pub fn disk_change(&self) -> Option<DiskChange> {
        let path = self.path.as_ref()?;
        let known = self.disk?;
        let Some(now) = disk_state(path) else { return Some(DiskChange::Missing) };
        if now == known {
            return None;
        }
        Some(if now.ino != known.ino {
            DiskChange::Replaced
        } else if now.len > known.len {
            DiskChange::Grown
        } else if now.len < known.len {
            DiskChange::Truncated
        } else {
            DiskChange::Modified
        })
    }

    /// Keep working with the current content and stop reporting this change.
    pub fn accept_disk_change(&mut self, change: DiskChange) {
        let Some(path) = self.path.clone() else { return };
        match change {
            // Appended bytes do not affect what we mapped: a diff backup
            // against the grown file stays valid.
            DiskChange::Grown if self.stamp.is_some() => self.stamp = file_stamp(&path),
            _ => self.stamp = None,
        }
        self.disk = disk_state(&path);
    }

    /// Reload from disk, discarding unsaved changes.
    pub fn reload(&mut self) -> io::Result<()> {
        let path = self.path.clone().ok_or_else(|| io::Error::other("no file"))?;
        let fresh = Buffer::open_with(&path, Some(self.encoding))?;
        let version = self.version + 1;
        let title = self.title.take();
        *self = Buffer { version, saved_version: version, title, ..fresh };
        Ok(())
    }

    /// The file grew and the buffer is unmodified: map the new size and
    /// index only the appended part. Falls back to a full reload.
    pub fn reload_appended(&mut self) -> io::Result<()> {
        // Only a plain mapped UTF-8 file can simply be extended.
        let pristine = !self.is_dirty()
            && self.encoding == Encoding::Utf8
            && !self.bom
            && self.add.is_empty()
            && self.pieces.iter().all(|p| p.buf == Buf::Orig)
            && self.pieces.len() <= 1;
        let Some(path) = self.path.clone() else { return Ok(()) };
        if !pristine {
            return self.reload();
        }
        let source = Arc::new(Source::open(&path)?);
        let len = source.bytes().len() as u64;
        self.index = LineIndex::spawn_continuing(source.clone(), &self.index);
        self.orig = source;
        self.pieces = if len > 0 { vec![Piece { buf: Buf::Orig, start: 0, len }] } else { Vec::new() };
        self.reindex();
        self.stamp = file_stamp(&path);
        self.disk = disk_state(&path);
        self.version += 1;
        self.saved_version = self.version;
        Ok(())
    }

    /// The file was truncated under a dirty buffer. Reading the mapping past
    /// the new end would crash, so copy what is still readable (plus all
    /// edits) into memory and detach from the file.
    pub fn detach_truncated(&mut self) {
        let Some(path) = self.path.clone() else { return };
        // Decoded content lives in memory already.
        if !matches!(*self.orig, Source::Mapped(_)) {
            self.stamp = None;
            self.disk = disk_state(&path);
            return;
        }
        self.index.cancel();
        let safe = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut content = Vec::new();
        for p in &self.pieces {
            match p.buf {
                Buf::Add => content.extend_from_slice(self.piece_bytes(p)),
                Buf::Orig => {
                    let end = (p.start + p.len).min(safe);
                    if p.start < end {
                        content.extend_from_slice(&self.orig.bytes()[p.start as usize..end as usize]);
                    }
                }
            }
        }
        let (version, saved, title, ending) = (self.version + 1, self.saved_version, self.title.take(), self.line_ending);
        let (enc, bom) = (self.encoding, self.bom);
        let mut b = Buffer::from_source(Arc::new(Source::Owned(content)), Some(path.clone()));
        b.encoding = enc;
        b.bom = bom;
        b.version = version;
        b.saved_version = saved;
        b.title = title;
        b.line_ending = ending;
        b.disk = disk_state(&path);
        *self = b;
    }
}

fn detect_line_ending(sample: &[u8]) -> LineEnding {
    match memchr::memchr(b'\n', sample) {
        Some(i) if i > 0 && sample[i - 1] == b'\r' => LineEnding::CrLf,
        _ => LineEnding::Lf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(b: &Buffer) -> String {
        b.text(0..b.len())
    }

    #[test]
    fn edit_undo_redo() {
        let mut b = Buffer::from_text("hello world");
        b.replace(5..5, ",", 5);
        assert_eq!(all(&b), "hello, world");
        b.replace(0..5, "HELLO", 0);
        assert_eq!(all(&b), "HELLO, world");
        b.replace(7..12, "", 7);
        assert_eq!(all(&b), "HELLO, ");
        b.undo();
        assert_eq!(all(&b), "HELLO, world");
        b.undo();
        assert_eq!(all(&b), "hello, world");
        b.redo();
        assert_eq!(all(&b), "HELLO, world");
        assert!(b.is_dirty());
    }

    #[test]
    fn typing_coalesces() {
        let mut b = Buffer::from_text("");
        let mut c = 0;
        for ch in ["a", "b", "c"] {
            c = b.replace(c..c, ch, c);
        }
        assert_eq!(all(&b), "abc");
        b.undo();
        assert_eq!(all(&b), "");
    }

    #[test]
    fn replace_all_one_undo_step() {
        let mut b = Buffer::from_text("a-b-c--d");
        // Split the original into several pieces first.
        b.replace(2..2, "X", 0);
        b.replace(0..0, "-", 0);
        assert_eq!(all(&b), "-a-Xb-c--d");
        let hits = [0..1, 2..3, 5..6, 7..8, 8..9];
        b.replace_all(&hits, "+=", 3);
        assert_eq!(all(&b), "+=a+=Xb+=c+=+=d");
        b.undo();
        assert_eq!(all(&b), "-a-Xb-c--d");
        b.redo();
        assert_eq!(all(&b), "+=a+=Xb+=c+=+=d");
        // Typing right after is a separate undo step.
        let c = b.replace(0..0, "z", 0);
        assert_eq!(c, 1);
        b.undo();
        assert_eq!(all(&b), "+=a+=Xb+=c+=+=d");
    }

    #[test]
    // One match is a one-element list of ranges, not a list of numbers.
    #[allow(clippy::single_range_in_vec_init)]
    fn replace_all_with_nothing_and_longer_needle() {
        let mut b = Buffer::from_text("foo bar foo baz foo");
        b.replace_all(&[0..3, 8..11, 16..19], "", 0);
        assert_eq!(all(&b), " bar  baz ");
        b.undo();
        assert_eq!(all(&b), "foo bar foo baz foo");
        assert!(!b.can_undo() && b.can_redo());
        let mut e = Buffer::from_text("foo");
        e.replace_all(&[0..3], "", 0);
        assert_eq!(all(&e), "");
        e.undo();
        assert_eq!(all(&e), "foo");
        assert!(e.can_redo());
    }

    #[test]
    fn find_and_lines() {
        let b = Buffer::from_text("one\ntwo\nthree\n");
        let find = |t: &str| crate::doc::search::Search::new(&crate::doc::search::Query { text: t.into(), match_case: true, whole_word: false }).unwrap();
        assert_eq!(find("three").find(&b, 0, |_| true), Some(8..13));
        assert_eq!(find("o").rfind(&b, 14, |_| true), Some(6..7));
        assert_eq!(b.line_of_offset(9), Some(2));
        assert_eq!(b.offset_of_line(2), Some(8));
        assert_eq!(b.offset_of_line(3), Some(14));
        assert_eq!(b.offset_of_line(4), None);
        assert_eq!(b.line_count(), Some(4));
        assert_eq!(b.rfind_byte(8, b'\n', 100), Some(7));
        assert_eq!(b.find_byte(0, b'\n', 100), Some(3));
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    #[test]
    fn full_backup_roundtrip() {
        let mut b = Buffer::from_text("hello world");
        b.replace(5..5, ",", 5);
        b.title = Some("new 2".into());
        let mut out = Vec::new();
        b.write_backup(&mut out).unwrap();
        let r = Buffer::read_backup(None, &out).unwrap();
        assert_eq!(r.text(0..r.len()), "hello, world");
        assert!(r.is_dirty());
        assert_eq!(r.line_count(), Some(1));
    }

    #[test]
    fn diff_backup_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rustpad-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        std::fs::write(&path, "abcdefghij").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.replace(3..5, "XYZ", 3);
        // Force the diff format regardless of size.
        let mut out = Vec::new();
        out.extend_from_slice(BACKUP_MAGIC);
        out.push(1);
        let (len, mtime) = b.stamp.unwrap();
        put_u64(&mut out, len).unwrap();
        out.extend_from_slice(&mtime.to_le_bytes());
        put_u64(&mut out, b.pieces.len() as u64).unwrap();
        for p in &b.pieces {
            match p.buf {
                Buf::Orig => {
                    out.push(0);
                    put_u64(&mut out, p.start).unwrap();
                    put_u64(&mut out, p.len).unwrap();
                }
                Buf::Add => {
                    out.push(1);
                    put_u64(&mut out, p.len).unwrap();
                    out.extend_from_slice(b.piece_bytes(p));
                }
            }
        }
        let r = Buffer::read_backup(Some(path.clone()), &out).unwrap();
        assert_eq!(r.text(0..r.len()), "abcXYZfghij");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "changed!!").unwrap();
        assert!(Buffer::read_backup(Some(path.clone()), &out).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;

    #[test]
    fn grow_and_follow() {
        let dir = std::env::temp_dir().join(format!("rustpad-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        assert_eq!(b.disk_change(), None);
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"c\n").unwrap();
        assert_eq!(b.disk_change(), Some(DiskChange::Grown));
        b.reload_appended().unwrap();
        assert_eq!(b.text(0..b.len()), "a\nb\nc\n");
        assert_eq!(b.line_count(), Some(4));
        assert_eq!(b.disk_change(), None);
        assert!(!b.is_dirty());

        // Replaced by a new file (rename).
        let tmp = dir.join("new.txt");
        std::fs::write(&tmp, "x\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert_eq!(b.disk_change(), Some(DiskChange::Replaced));
        b.accept_disk_change(DiskChange::Replaced);
        assert_eq!(b.disk_change(), None);
        assert_eq!(b.text(0..b.len()), "a\nb\nc\n"); // old content kept

        b.reload().unwrap();
        assert_eq!(b.text(0..b.len()), "x\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_dirty_buffer_detaches() {
        let dir = std::env::temp_dir().join(format!("rustpad-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.txt");
        std::fs::write(&path, "hello world").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.replace(0..0, ">> ", 0);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(5).unwrap();
        assert_eq!(b.disk_change(), Some(DiskChange::Truncated));
        b.detach_truncated();
        assert_eq!(b.text(0..b.len()), ">> hello");
        assert!(b.is_dirty());
        assert_eq!(b.disk_change(), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rustpad-enc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn save(b: &Buffer, path: &Path) -> io::Result<()> {
        b.snapshot().write_to(path, |_| true)
    }

    #[test]
    fn legacy_file_round_trip() {
        let path = dir("legacy").join("a.txt");
        std::fs::write(&path, b"Gr\xFC\xDFe\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        assert_eq!((b.encoding, b.bom), (Encoding::Windows1252, false));
        assert_eq!(b.text(0..b.len()), "Grüße\n");
        let end = b.len() - 1;
        b.replace(end..end, " ä", end);
        save(&b, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"Gr\xFC\xDFe \xE4\n");
        // Characters the encoding lacks fail the save and keep the file.
        b.replace(0..0, "😀", 0);
        let err = save(&b, &path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), b"Gr\xFC\xDFe \xE4\n");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn utf8_bom_is_hidden_and_kept() {
        let path = dir("bom").join("b.txt");
        std::fs::write(&path, b"\xEF\xBB\xBFhi").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        assert_eq!((b.encoding, b.bom, b.text(0..b.len())), (Encoding::Utf8, true, "hi".to_string()));
        b.replace(2..2, "!", 2);
        save(&b, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\xEF\xBB\xBFhi!");
        // Convert: drop the BOM, then to UTF-16 LE with BOM.
        b.convert(Encoding::Utf8, false);
        save(&b, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hi!");
        b.convert(Encoding::Utf16Le, true);
        save(&b, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\xFF\xFEh\0i\0!\0");
        let r = Buffer::open(&path).unwrap();
        assert_eq!((r.encoding, r.bom, r.text(0..r.len())), (Encoding::Utf16Le, true, "hi!".to_string()));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn reinterpret_and_reload_keep_the_encoding() {
        let path = dir("forced").join("c.txt");
        std::fs::write(&path, b"h\0i\0").unwrap();
        // Without a BOM UTF-16 is not guessed; opening it as UTF-16 LE works.
        assert_eq!(Buffer::open(&path).unwrap().encoding, Encoding::Utf8);
        let mut b = Buffer::open_with(&path, Some(Encoding::Utf16Le)).unwrap();
        assert_eq!((b.text(0..b.len()), b.bom), ("hi".to_string(), false));
        b.reload().unwrap();
        assert_eq!((b.encoding, b.text(0..b.len())), (Encoding::Utf16Le, "hi".to_string()));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

#[cfg(test)]
mod line_ending_tests {
    use super::*;

    /// Converts `input` cut into the given chunk sizes.
    fn convert(input: &[u8], to: LineEnding, cuts: &[usize]) -> Vec<u8> {
        let mut conv = EolConverter::new(to);
        let (mut out, mut buf, mut rest) = (Vec::new(), Vec::new(), input);
        for &n in cuts.iter().chain(std::iter::once(&usize::MAX)) {
            let (chunk, tail) = rest.split_at(n.min(rest.len()));
            conv.convert(chunk, &mut buf);
            out.extend_from_slice(&buf);
            rest = tail;
        }
        out.extend_from_slice(conv.finish());
        out
    }

    #[test]
    fn mixed_line_endings_become_one_kind() {
        let mixed = b"a\r\nb\nc\r\n\nd\re";
        assert_eq!(convert(mixed, LineEnding::Lf, &[]), b"a\nb\nc\n\nd\re");
        assert_eq!(convert(mixed, LineEnding::CrLf, &[]), b"a\r\nb\r\nc\r\n\r\nd\re");
    }

    #[test]
    fn line_ending_split_between_chunks() {
        // Every cut position, including between "\r" and "\n".
        let text = b"one\r\ntwo\nthree\r\n";
        for cut in 0..=text.len() {
            assert_eq!(convert(text, LineEnding::Lf, &[cut]), b"one\ntwo\nthree\n", "cut {cut}");
            assert_eq!(convert(text, LineEnding::CrLf, &[cut]), b"one\r\ntwo\r\nthree\r\n", "cut {cut}");
        }
        // A lone "\r" at the very end is kept.
        assert_eq!(convert(b"x\r", LineEnding::Lf, &[1]), b"x\r");
    }

    #[test]
    fn saving_converts_line_endings() {
        let dir = std::env::temp_dir().join(format!("rustpad-eol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"one\ntwo\r\nthree\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        assert_eq!(b.line_ending, LineEnding::Lf);
        b.convert_line_endings(LineEnding::CrLf);
        assert!(b.is_dirty());
        // Typed text and the existing lines end up alike.
        let end = b.len();
        b.replace(end..end, "four\n", end);
        b.snapshot().write_to(&path, |_| true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\r\ntwo\r\nthree\r\nfour\r\n");
        let version = b.version();
        b.finish_save(&path, version);
        assert!(!b.convert_eol && !b.is_dirty());
        assert_eq!(b.line_ending, LineEnding::CrLf);
        std::fs::remove_dir_all(&dir).ok();
    }
}
