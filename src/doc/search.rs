//! Find with options: match case, whole word.
//!
//! Like the plain byte search, this reads the document in windows, so it
//! works on files of any size. Matches are byte ranges: ignoring case, a
//! match can be longer or shorter than the search text ("ß" and "ẞ").

use std::ops::Range;

use regex::bytes::{Regex, RegexBuilder};

use super::Buffer;

/// Bytes read at a time.
const WINDOW: u64 = 4 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub match_case: bool,
    pub whole_word: bool,
}

enum Matcher {
    /// Exact bytes: the fast path.
    Bytes(Box<memchr::memmem::Finder<'static>>),
    /// Case folding needs a regex (of the escaped text).
    Regex(Regex),
}

/// A query ready to run.
pub struct Search {
    matcher: Matcher,
    whole_word: bool,
    /// Longest possible match in bytes, so a match across two windows is
    /// still found.
    max_len: u64,
}

impl Search {
    /// `None` for an empty search text.
    pub fn new(q: &Query) -> Option<Search> {
        if q.text.is_empty() {
            return None;
        }
        let matcher = if q.match_case {
            Matcher::Bytes(Box::new(memchr::memmem::Finder::new(q.text.as_bytes()).into_owned()))
        } else {
            Matcher::Regex(RegexBuilder::new(&regex::escape(&q.text)).case_insensitive(true).build().ok()?)
        };
        // A folded character takes at most 4 bytes in UTF-8.
        let max_len = if q.match_case { q.text.len() } else { q.text.chars().count() * 4 } as u64;
        Some(Search { matcher, whole_word: q.whole_word, max_len })
    }

    /// Matches in `hay` starting at or after `from`, in order.
    fn matches<'a>(&'a self, hay: &'a [u8], from: usize) -> impl Iterator<Item = Range<usize>> + 'a {
        let mut at = from;
        std::iter::from_fn(move || {
            let m = match &self.matcher {
                Matcher::Bytes(f) => f.find(hay.get(at..)?).map(|i| at + i..at + i + f.needle().len()),
                Matcher::Regex(r) => r.find_at(hay, at).map(|m| m.range()),
            }?;
            // Overlapping candidates matter for whole words ("aa" in "aaa aa").
            at = m.start + 1;
            Some(m)
        })
    }

    /// Whether `range` is a whole word in `b`: no letter, digit or "_"
    /// right before or after it.
    fn is_whole_word(&self, b: &Buffer, range: &Range<u64>) -> bool {
        if !self.whole_word {
            return true;
        }
        let before = b.slice(range.start.saturating_sub(4)..range.start);
        let after = b.slice(range.end..(range.end + 4).min(b.len()));
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        !word(String::from_utf8_lossy(&before).chars().next_back()) && !word(String::from_utf8_lossy(&after).chars().next())
    }

    /// First match starting at or after `from`. `progress` receives the
    /// position and may return false to stop.
    pub fn find(&self, b: &Buffer, from: u64, mut progress: impl FnMut(u64) -> bool) -> Option<Range<u64>> {
        let overlap = self.max_len.saturating_sub(1);
        let mut pos = from;
        while pos < b.len() {
            if !progress(pos) {
                return None;
            }
            let end = (pos + WINDOW + overlap).min(b.len());
            let last = end == b.len();
            let hay = b.slice(pos..end);
            for m in self.matches(&hay, 0) {
                // Later starts are found again, whole, in the next window.
                if !last && m.start as u64 >= WINDOW {
                    break;
                }
                let r = pos + m.start as u64..pos + m.end as u64;
                if self.is_whole_word(b, &r) {
                    return Some(r);
                }
            }
            if last {
                break;
            }
            pos += WINDOW;
        }
        None
    }

    /// Last match starting before `before`.
    pub fn rfind(&self, b: &Buffer, before: u64, mut progress: impl FnMut(u64) -> bool) -> Option<Range<u64>> {
        let overlap = self.max_len.saturating_sub(1);
        let mut end = before.min(b.len());
        while end > 0 {
            if !progress(end) {
                return None;
            }
            let start = end.saturating_sub(WINDOW);
            let hay = b.slice(start..(end + overlap).min(b.len()));
            let mut found = None;
            for m in self.matches(&hay, 0) {
                let r = start + m.start as u64..start + m.end as u64;
                if r.start >= end {
                    break;
                }
                if self.is_whole_word(b, &r) {
                    found = Some(r);
                }
            }
            if found.is_some() {
                return found;
            }
            end = start;
        }
        None
    }

    /// Every match from `from` on, without overlaps (for replace all).
    pub fn find_all(&self, b: &Buffer, mut from: u64, mut progress: impl FnMut(u64) -> bool) -> Vec<Range<u64>> {
        let mut out = Vec::new();
        while let Some(r) = self.find(b, from, &mut progress) {
            // An empty match cannot happen (the text is not empty), but never loop.
            from = r.end.max(r.start + 1);
            out.push(r);
        }
        out
    }

    /// Whether `range` of `b` is a match on its own (the selection, before
    /// "Replace" replaces it).
    pub fn is_match(&self, b: &Buffer, range: Range<u64>) -> bool {
        let hay = b.slice(range.clone());
        self.matches(&hay, 0).next().is_some_and(|m| m == (0..hay.len())) && self.is_whole_word(b, &range)
    }
}

#[cfg(test)]
// Expected matches are lists of ranges, some with just one.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn q(text: &str, match_case: bool, whole_word: bool) -> Search {
        Search::new(&Query { text: text.into(), match_case, whole_word }).unwrap()
    }

    fn all(b: &Buffer, s: &Search) -> Vec<Range<u64>> {
        s.find_all(b, 0, |_| true)
    }

    #[test]
    fn match_case_or_not() {
        let b = Buffer::from_text("Straße STRASSE straße Äpfel äpfel");
        assert_eq!(all(&b, &q("straße", true, false)), [16..23]);
        assert_eq!(all(&b, &q("straße", false, false)), [0..7, 16..23]);
        assert_eq!(all(&b, &q("ÄPFEL", false, false)), [24..30, 31..37]);
    }

    #[test]
    fn whole_words_only() {
        let b = Buffer::from_text("cat concat cat_x cat. (cat) scät cat");
        let hits = all(&b, &q("cat", true, true));
        let words: Vec<_> = hits.iter().map(|r| r.start).collect();
        assert_eq!(words, [0, 17, 23, 34]);
        // A text that starts with a non-word character still matches as a
        // word; only letters, digits and "_" around it count.
        let b = Buffer::from_text("a -x b--x a-x -x");
        // "-x" in "a-x" follows a letter, so it is no word there.
        assert_eq!(all(&b, &q("-x", true, true)), [2..4, 7..9, 14..16]);
        let b = Buffer::from_text("x-x2 -x_ -x");
        assert_eq!(all(&b, &q("-x", true, true)), [9..11]);
        // Overlapping candidates: the second "aa" of "aaa" is no word, the last one is.
        let b = Buffer::from_text("aaa aa");
        assert_eq!(all(&b, &q("aa", true, true)), [4..6]);
    }

    #[test]
    fn backwards() {
        let b = Buffer::from_text("Foo foo FOO food");
        let s = q("foo", false, true);
        assert_eq!(s.rfind(&b, b.len(), |_| true), Some(8..11));
        assert_eq!(s.rfind(&b, 8, |_| true), Some(4..7));
        assert_eq!(s.rfind(&b, 0, |_| true), None);
    }

    #[test]
    fn match_across_windows() {
        // A case-folded match that starts just before the window edge.
        let mut text = "x".repeat(WINDOW as usize - 3);
        text.push_str("ÄRGER");
        let b = Buffer::from_text(&text);
        let start = WINDOW - 3;
        assert_eq!(q("ärger", false, false).find(&b, 0, |_| true), Some(start..start + 6));
        assert_eq!(q("ärger", false, true).find(&b, 0, |_| true), None);
    }

    #[test]
    fn selection_check() {
        let b = Buffer::from_text("Hallo hallo halloween");
        let s = q("HALLO", false, true);
        assert!(s.is_match(&b, 0..5));
        assert!(s.is_match(&b, 6..11));
        assert!(!s.is_match(&b, 12..17));
        assert!(!s.is_match(&b, 0..4));
    }
}
