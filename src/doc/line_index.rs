//! Sparse newline index, built in a background thread.
//!
//! The file is split into fixed blocks; for each block we store the
//! cumulative newline count. Queries combine one table lookup with a short
//! memchr scan inside a single block, so line numbers become available
//! progressively while the file is still being indexed.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::buffer::Source;

pub const BLOCK: u64 = 512 * 1024;

pub struct LineIndex {
    /// `cum[i]` = newlines in `[0, min((i + 1) * BLOCK, len))`.
    cum: RwLock<Vec<u64>>,
    indexed: AtomicU64,
    done: AtomicBool,
    /// Set when the mapped file may no longer be readable (truncated).
    cancelled: AtomicBool,
    len: u64,
}

impl LineIndex {
    pub fn spawn(source: Arc<Source>) -> Arc<LineIndex> {
        Self::spawn_from(source, Vec::new())
    }

    /// Index a file that grew: blocks fully covered by `prev` are reused, so
    /// following a growing log only scans the new bytes.
    pub fn spawn_continuing(source: Arc<Source>, prev: &LineIndex) -> Arc<LineIndex> {
        let cum = prev.cum.read().unwrap();
        let full = (prev.len / BLOCK) as usize;
        Self::spawn_from(source, cum[..cum.len().min(full)].to_vec())
    }

    fn spawn_from(source: Arc<Source>, known: Vec<u64>) -> Arc<LineIndex> {
        let len = source.bytes().len() as u64;
        let start = known.len() as u64 * BLOCK;
        let index = Arc::new(LineIndex {
            indexed: AtomicU64::new(start.min(len)),
            cum: RwLock::new(known),
            done: AtomicBool::new(len == 0),
            cancelled: AtomicBool::new(false),
            len,
        });
        if len - start.min(len) > BLOCK {
            let idx = index.clone();
            std::thread::Builder::new()
                .name("line-index".into())
                .spawn(move || idx.run(&source, true))
                .ok();
        } else {
            // Little to do: index synchronously, it costs microseconds.
            index.run(&source, false);
        }
        index
    }

    /// Stop touching the mapped file.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// `background`: running on its own thread, which stops once the buffer is gone.
    fn run(self: &Arc<Self>, source: &Source, background: bool) {
        let bytes = source.bytes();
        let (mut pos, mut acc) = {
            let cum = self.cum.read().unwrap();
            (cum.len() as u64 * BLOCK, cum.last().copied().unwrap_or(0))
        };
        let mut batch = Vec::with_capacity(64);
        while pos < self.len {
            // The owning buffer is gone, or the file is no longer safe to read.
            // (Run inline, our caller holds the only reference so far.)
            if (background && Arc::strong_count(self) == 1) || self.cancelled.load(Ordering::Acquire) {
                return;
            }
            let end = (pos + BLOCK).min(self.len);
            acc += memchr::memchr_iter(b'\n', &bytes[pos as usize..end as usize]).count() as u64;
            batch.push(acc);
            pos = end;
            if batch.len() == batch.capacity() || pos == self.len {
                self.cum.write().unwrap().extend_from_slice(&batch);
                batch.clear();
                self.indexed.store(pos, Ordering::Release);
            }
        }
        self.done.store(true, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Fraction of the file indexed so far.
    pub fn progress(&self) -> f32 {
        if self.len == 0 {
            1.0
        } else {
            self.indexed.load(Ordering::Acquire) as f32 / self.len as f32
        }
    }

    /// Newlines in `[0, x)`.
    fn before(&self, bytes: &[u8], cum: &[u64], x: u64) -> Option<u64> {
        let b = x / BLOCK;
        let base = if b == 0 { 0 } else { *cum.get(b as usize - 1)? };
        let from = b * BLOCK;
        Some(base + memchr::memchr_iter(b'\n', &bytes[from as usize..x as usize]).count() as u64)
    }

    /// Newlines in `range` of the original file.
    pub fn count(&self, bytes: &[u8], range: Range<u64>) -> Option<u64> {
        if range.is_empty() {
            return Some(0);
        }
        // Short ranges are cheaper to scan directly than to look up.
        if range.end - range.start <= BLOCK {
            let s = &bytes[range.start as usize..range.end as usize];
            return Some(memchr::memchr_iter(b'\n', s).count() as u64);
        }
        let cum = self.cum.read().unwrap();
        Some(self.before(bytes, &cum, range.end)? - self.before(bytes, &cum, range.start)?)
    }

    /// Absolute position of the `k`-th (0-based) newline at or after `from`.
    pub fn nth_newline(&self, bytes: &[u8], from: u64, k: u64) -> Option<u64> {
        let cum = self.cum.read().unwrap();
        let g = self.before(bytes, &cum, from)? + k;
        let block = cum.partition_point(|&c| c <= g);
        if block >= cum.len() {
            return None;
        }
        let base = if block == 0 { 0 } else { cum[block - 1] };
        let start = block as u64 * BLOCK;
        let end = (start + BLOCK).min(self.len);
        memchr::memchr_iter(b'\n', &bytes[start as usize..end as usize])
            .nth((g - base) as usize)
            .map(|i| start + i as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_files_are_indexed_inline() {
        let source = Arc::new(Source::Owned(b"one\ntwo\nthree\n".to_vec()));
        let index = LineIndex::spawn(source.clone());
        assert!(index.is_done());
        assert_eq!(index.nth_newline(source.bytes(), 0, 1), Some(7));
    }
}
