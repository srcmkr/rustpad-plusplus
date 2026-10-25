//! UI translations from gettext `.po` catalogs.
//!
//! Source strings (msgids) are English and wrapped in `tr!` / `trn!` (or
//! `n_!` where the translation happens later through [`tr`]). The catalogs in
//! `po/` are embedded at build time, so nothing has to be installed. The
//! language is a setting ("auto" follows `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`
//! or `LANG` like gettext) and switches live; untranslated strings stay
//! English.
//!
//! Adding a language: copy `po/rustpad.pot` to `po/<lang>.po`, translate it
//! and add it to [`CATALOGS`] with the language's own name. After changing UI strings, regenerate the
//! template with `cargo test -- --ignored write_pot`; the `catalogs_match_sources`
//! test fails while a catalog misses strings or keeps stale ones.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Embedded catalogs: language code, the language's own name, catalog.
const CATALOGS: &[(&str, &str, &str)] = &[
    ("de", "Deutsch", include_str!("../po/de.po")),
    ("es", "Español", include_str!("../po/es.po")),
    ("fr", "Français", include_str!("../po/fr.po")),
    ("it", "Italiano", include_str!("../po/it.po")),
    ("pt_BR", "Português (Brasil)", include_str!("../po/pt_BR.po")),
];

/// Translate a literal UI string; with `name = value` pairs, fill `{name}` placeholders.
macro_rules! tr {
    ($id:literal) => {
        $crate::i18n::tr($id)
    };
    ($id:literal, $($k:ident = $v:expr),+ $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($id), &[$((stringify!($k), &$v as &dyn std::fmt::Display)),+])
    };
}

/// Plural-aware translation; `{n}` is the count, more placeholders as in `tr!`.
macro_rules! trn {
    ($one:literal, $many:literal, $n:expr $(, $k:ident = $v:expr)* $(,)?) => {{
        let n = $n;
        $crate::i18n::fill(
            $crate::i18n::trn($one, $many, n as u64),
            &[("n", &n as &dyn std::fmt::Display) $(, (stringify!($k), &$v as &dyn std::fmt::Display))*],
        )
    }};
}

/// Mark a string for the catalog without translating it here (gettext's `N_`).
macro_rules! n_ {
    ($id:literal) => {
        $id
    };
}

struct Catalog {
    /// msgid -> msgstr forms (one, or one per plural form).
    entries: HashMap<String, Vec<String>>,
    plural: Expr,
}

/// All catalogs, parsed on first use and kept, so `tr` can hand out `&'static str`
/// and switching the language is a single store.
static PARSED: OnceLock<Vec<Catalog>> = OnceLock::new();

const ENGLISH: usize = usize::MAX;
const UNSET: usize = usize::MAX - 1;
/// Index into [`CATALOGS`], or [`ENGLISH`].
static CURRENT: AtomicUsize = AtomicUsize::new(UNSET);

/// Selectable languages for the settings: (code, own name), "auto" first.
pub fn choices() -> Vec<(&'static str, &'static str)> {
    let mut out = vec![("auto", tr!("Automatic (system language)")), ("en", "English")];
    out.extend(CATALOGS.iter().map(|(code, name, _)| (*code, *name)));
    out
}

/// Switch the UI language: "auto", or a code like "de". Windows need a refresh.
pub fn set_language(setting: &str) {
    CURRENT.store(pick(&wanted_languages(Some(setting))), Ordering::Relaxed);
}

/// First wanted language that is English or has a catalog. A language
/// without a region also takes a regional catalog ("pt" takes "pt_BR").
fn pick(wanted: &[String]) -> usize {
    for w in wanted {
        if w == "en" {
            return ENGLISH;
        }
        if let Some(i) = CATALOGS.iter().position(|(code, _, _)| code == w) {
            return i;
        }
        if !w.contains('_')
            && let Some(i) = CATALOGS.iter().position(|(code, _, _)| code.split('_').next() == Some(w.as_str()))
        {
            return i;
        }
    }
    ENGLISH
}

fn catalog() -> Option<&'static Catalog> {
    let mut i = CURRENT.load(Ordering::Relaxed);
    if i == UNSET {
        set_language("auto");
        i = CURRENT.load(Ordering::Relaxed);
    }
    if i == ENGLISH {
        return None;
    }
    PARSED.get_or_init(|| CATALOGS.iter().map(|(_, _, po)| parse_po(po)).collect()).get(i)
}

/// Language codes in order of preference, most specific first ("de_AT", "de").
fn wanted_languages(configured: Option<&str>) -> Vec<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let list = match configured.filter(|c| !c.is_empty() && *c != "auto") {
        Some(c) => c.to_string(),
        None => {
            let locale = env("LC_ALL").or_else(|| env("LC_MESSAGES")).or_else(|| env("LANG")).unwrap_or_default();
            // Like gettext: the C locale means untranslated, whatever LANGUAGE says.
            if locale.is_empty() || locale == "C" || locale == "POSIX" || locale.starts_with("C.") {
                return Vec::new();
            }
            env("LANGUAGE").unwrap_or(locale)
        }
    };
    let mut out = Vec::new();
    for item in list.split(':') {
        let code = item.split(['.', '@']).next().unwrap_or_default();
        for c in [code, code.split('_').next().unwrap_or_default()] {
            if !c.is_empty() && !out.iter().any(|o| o == c) {
                out.push(c.to_string());
            }
        }
    }
    out
}

pub fn tr(id: &'static str) -> &'static str {
    catalog()
        .and_then(|c| c.entries.get(id))
        .and_then(|forms| forms.first())
        .filter(|s| !s.is_empty())
        .map_or(id, String::as_str)
}

pub fn trn(one: &'static str, many: &'static str, n: u64) -> &'static str {
    let english = if n == 1 { one } else { many };
    let Some(c) = catalog() else { return english };
    c.entries
        .get(one)
        .and_then(|forms| forms.get(c.plural.eval(n) as usize))
        .filter(|s| !s.is_empty())
        .map_or(english, String::as_str)
}

/// Replace `{name}` placeholders.
pub fn fill(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        out = out.replace(&format!("{{{k}}}"), &v.to_string());
    }
    out
}

// ---------------------------------------------------------------- .po parsing

fn parse_po(text: &str) -> Catalog {
    #[derive(Default)]
    struct Entry {
        ctxt: Option<String>,
        id: String,
        forms: Vec<String>,
        fuzzy: bool,
    }
    enum Field {
        Ctxt,
        Id,
        IdPlural,
        Str(usize),
    }
    let mut entries = HashMap::new();
    let mut header = String::new();
    let mut cur = Entry::default();
    let mut field = None;
    let mut started = false;
    let mut flush = |e: Entry, header: &mut String| {
        if e.id.is_empty() {
            *header = e.forms.into_iter().next().unwrap_or_default();
        } else if !e.fuzzy {
            let key = match e.ctxt {
                Some(c) => format!("{c}\u{4}{}", e.id),
                None => e.id,
            };
            entries.insert(key, e.forms);
        }
    };
    for line in text.lines().map(str::trim) {
        let (kw, rest) = match line.split_once(' ') {
            Some((k, r)) if !line.starts_with('"') => (k, r.trim()),
            _ => ("", line),
        };
        // A new entry starts at its first comment or keyword after a msgstr.
        let starts_entry = line.starts_with('#') || kw == "msgctxt" || kw == "msgid";
        if starts_entry && matches!(field, Some(Field::Str(_))) {
            flush(std::mem::take(&mut cur), &mut header);
            field = None;
            started = false;
        }
        if line.starts_with("#,") && line.contains("fuzzy") {
            cur.fuzzy = true;
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let new_field = match kw {
            "msgctxt" => Some(Field::Ctxt),
            "msgid" => Some(Field::Id),
            "msgid_plural" => Some(Field::IdPlural),
            "msgstr" => Some(Field::Str(0)),
            k if k.starts_with("msgstr[") => k[7..k.len() - 1].parse().ok().map(Field::Str),
            _ => None,
        };
        if let Some(f) = new_field {
            field = Some(f);
            started = true;
        }
        if !started {
            continue;
        }
        let s = unquote(rest);
        match field {
            Some(Field::Ctxt) => cur.ctxt.get_or_insert_with(String::new).push_str(&s),
            Some(Field::Id) => cur.id.push_str(&s),
            Some(Field::IdPlural) => {}
            Some(Field::Str(i)) => {
                if cur.forms.len() <= i {
                    cur.forms.resize(i + 1, String::new());
                }
                cur.forms[i].push_str(&s);
            }
            None => {}
        }
    }
    if matches!(field, Some(Field::Str(_))) {
        flush(cur, &mut header);
    }
    let plural = header
        .lines()
        .find_map(|l| l.strip_prefix("Plural-Forms:"))
        .and_then(|l| l.split(';').find_map(|p| p.trim().strip_prefix("plural=")))
        .and_then(Expr::parse)
        .unwrap_or(Expr::Ne(Box::new(Expr::N), Box::new(Expr::Num(1))));
    Catalog { entries, plural }
}

/// Contents of a quoted .po string, unescaped.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let inner = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// The C subset used by `Plural-Forms` expressions.
#[derive(Debug)]
enum Expr {
    N,
    Num(u64),
    Not(Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

impl Expr {
    fn parse(src: &str) -> Option<Expr> {
        let tokens = tokenize(src)?;
        let mut p = Parser { tokens, pos: 0 };
        let e = p.ternary()?;
        (p.pos == p.tokens.len()).then_some(e)
    }

    fn eval(&self, n: u64) -> u64 {
        match self {
            Expr::N => n,
            Expr::Num(v) => *v,
            Expr::Not(e) => (e.eval(n) == 0) as u64,
            Expr::Ne(a, b) => (a.eval(n) != b.eval(n)) as u64,
            Expr::If(c, a, b) => {
                if c.eval(n) != 0 {
                    a.eval(n)
                } else {
                    b.eval(n)
                }
            }
            Expr::Bin(op, a, b) => {
                let (a, b) = (a.eval(n), b.eval(n));
                match *op {
                    "||" => (a != 0 || b != 0) as u64,
                    "&&" => (a != 0 && b != 0) as u64,
                    "==" => (a == b) as u64,
                    "<" => (a < b) as u64,
                    ">" => (a > b) as u64,
                    "<=" => (a <= b) as u64,
                    ">=" => (a >= b) as u64,
                    "+" => a.wrapping_add(b),
                    "-" => a.wrapping_sub(b),
                    "*" => a.wrapping_mul(b),
                    "/" => a.checked_div(b).unwrap_or(0),
                    "%" => a.checked_rem(b).unwrap_or(0),
                    _ => 0,
                }
            }
        }
    }
}

fn tokenize(src: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            out.push(src[start..i].to_string());
        } else if let Some(op) = ["||", "&&", "==", "!=", "<=", ">="].iter().find(|op| src[i..].starts_with(*op)) {
            out.push(op.to_string());
            i += 2;
        } else if "n!<>+-*/%?:()".contains(c) {
            out.push(c.to_string());
            i += 1;
        } else {
            return None;
        }
    }
    Some(out)
}

struct Parser {
    tokens: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(String::as_str)
    }

    fn eat(&mut self, t: &str) -> bool {
        let hit = self.peek() == Some(t);
        self.pos += hit as usize;
        hit
    }

    fn ternary(&mut self) -> Option<Expr> {
        let cond = self.binary(0)?;
        if !self.eat("?") {
            return Some(cond);
        }
        let a = self.ternary()?;
        self.eat(":").then_some(())?;
        let b = self.ternary()?;
        Some(Expr::If(Box::new(cond), Box::new(a), Box::new(b)))
    }

    /// Binary operators by C precedence, loosest first.
    fn binary(&mut self, level: usize) -> Option<Expr> {
        const LEVELS: &[&[&str]] =
            &[&["||"], &["&&"], &["==", "!="], &["<", ">", "<=", ">="], &["+", "-"], &["*", "/", "%"]];
        let Some(ops) = LEVELS.get(level) else { return self.unary() };
        let mut left = self.binary(level + 1)?;
        while let Some(op) = ops.iter().find(|op| self.peek() == Some(**op)) {
            self.pos += 1;
            let right = self.binary(level + 1)?;
            left = if *op == "!=" { Expr::Ne(Box::new(left), Box::new(right)) } else { Expr::Bin(op, Box::new(left), Box::new(right)) };
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<Expr> {
        if self.eat("!") {
            return Some(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat("(") {
            let e = self.ternary()?;
            return self.eat(")").then_some(e);
        }
        let t = self.peek()?.to_string();
        self.pos += 1;
        if t == "n" { Some(Expr::N) } else { t.parse().ok().map(Expr::Num) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn plural_rules() {
        let de = Expr::parse("(n != 1)").unwrap();
        assert_eq!([0, 1, 2].map(|n| de.eval(n)), [1, 0, 1]);
        let pl = Expr::parse("(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2)").unwrap();
        assert_eq!([1, 2, 5, 12, 22, 25].map(|n| pl.eval(n)), [0, 1, 2, 2, 1, 2]);
        assert!(Expr::parse("n +").is_none());
    }

    #[test]
    fn parses_po() {
        let c = parse_po(concat!(
            "msgid \"\"\nmsgstr \"\"\n\"Plural-Forms: nplurals=2; plural=(n != 1);\\n\"\n\n",
            "#: src/x.rs\nmsgid \"Find\"\nmsgstr \"Suchen\"\n\n",
            "msgid \"\"\n\"Two \"\n\"lines\"\nmsgstr \"Zwei\\nZeilen \\\"q\\\"\"\n\n",
            "#, fuzzy\nmsgid \"Old\"\nmsgstr \"Alt\"\n\n",
            "msgctxt \"menu\"\nmsgid \"Open\"\nmsgstr \"Öffnen\"\n\n",
            "msgid \"{n} file\"\nmsgid_plural \"{n} files\"\nmsgstr[0] \"{n} Datei\"\nmsgstr[1] \"{n} Dateien\"\n",
        ));
        assert_eq!(c.entries["Find"], ["Suchen"]);
        assert_eq!(c.entries["Two lines"], ["Zwei\nZeilen \"q\""]);
        assert!(!c.entries.contains_key("Old"));
        assert_eq!(c.entries["menu\u{4}Open"], ["Öffnen"]);
        assert_eq!(c.entries["{n} file"], ["{n} Datei", "{n} Dateien"]);
        assert_eq!(c.plural.eval(1), 0);
    }

    #[test]
    fn fills_placeholders() {
        assert_eq!(fill("{a} and {b}, {a}", &[("a", &1), ("b", &"x")]), "1 and x, 1");
    }

    // ------------------------------------------------------------ catalog checks

    /// (msgid, plural, source locations) for every `tr!`, `trn!` and `n_!` in `src/`.
    fn extract() -> BTreeMap<String, (Option<String>, Vec<String>)> {
        fn literal(s: &str) -> Option<(String, &str)> {
            let s = s.trim_start().strip_prefix('"')?;
            let mut out = String::new();
            let mut chars = s.char_indices();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => return Some((out, &s[i + 1..])),
                    '\\' => match chars.next()?.1 {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        other => out.push(other),
                    },
                    c => out.push(c),
                }
            }
            None
        }
        fn walk(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, files);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        files.sort();
        let mut out: BTreeMap<String, (Option<String>, Vec<String>)> = BTreeMap::new();
        for f in files {
            let mut text = std::fs::read_to_string(&f).unwrap();
            let rel = f.strip_prefix(root).unwrap().display().to_string();
            // The tests of this file contain sample strings, not UI text.
            if rel.ends_with("i18n.rs")
                && let Some(i) = text.find("#[cfg(test)]")
            {
                text.truncate(i);
            }
            for mac in ["tr!(", "trn!(", "n_!("] {
                let mut from = 0;
                while let Some(i) = text[from..].find(mac).map(|i| i + from) {
                    from = i + mac.len();
                    if text[..i].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let Some((id, rest)) = literal(&text[from..]) else { continue };
                    let plural = if mac == "trn!(" {
                        literal(rest.trim_start().strip_prefix(',').unwrap_or(rest)).map(|(p, _)| p)
                    } else {
                        None
                    };
                    let line = text[..i].lines().count();
                    let e = out.entry(id).or_insert((plural, Vec::new()));
                    e.1.push(format!("{rel}:{line}"));
                }
            }
        }
        out
    }

    fn po_escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\t', "\\t")
    }

    #[test]
    fn catalogs_match_sources() {
        let ids = extract();
        assert!(!ids.is_empty());
        for (lang, _, po) in CATALOGS {
            let c = parse_po(po);
            let missing: Vec<_> = ids.keys().filter(|id| c.entries.get(*id).is_none_or(|f| f.iter().any(String::is_empty))).collect();
            let stale: Vec<_> = c.entries.keys().filter(|id| !ids.contains_key(*id)).collect();
            assert!(missing.is_empty(), "po/{lang}.po lacks translations for {missing:#?}");
            assert!(stale.is_empty(), "po/{lang}.po has entries no longer in the sources: {stale:#?}");
            for (id, (plural, _)) in &ids {
                if plural.is_some() {
                    assert!(c.entries[id].len() >= 2, "po/{lang}.po: {id:?} needs plural forms");
                }
            }
        }
    }

    /// Regenerate `po/rustpad.pot`: `cargo test -- --ignored write_pot`.
    #[test]
    #[ignore]
    fn write_pot() {
        let mut out = String::from(concat!(
            "# Translation template for rustpad++. Copy to <lang>.po to start a language.\n",
            "# Keep {placeholders} as they are; they are filled in at runtime.\n",
            "msgid \"\"\nmsgstr \"\"\n",
            "\"Project-Id-Version: rustpad++\\n\"\n",
            "\"Content-Type: text/plain; charset=UTF-8\\n\"\n",
            "\"Plural-Forms: nplurals=2; plural=(n != 1);\\n\"\n",
        ));
        for (id, (plural, locs)) in extract() {
            out.push_str(&format!("\n#: {}\n", locs.join(" ")));
            out.push_str(&format!("msgid \"{}\"\n", po_escape(&id)));
            match plural {
                Some(p) => out.push_str(&format!("msgid_plural \"{}\"\nmsgstr[0] \"\"\nmsgstr[1] \"\"\n", po_escape(&p))),
                None => out.push_str("msgstr \"\"\n"),
            }
        }
        std::fs::write(Path::new(env!("CARGO_MANIFEST_DIR")).join("po/rustpad.pot"), out).unwrap();
    }

    #[test]
    fn language_from_environment() {
        assert_eq!(wanted_languages(Some("de_AT.UTF-8")), ["de_AT", "de"]);
        assert_eq!(wanted_languages(Some("nl:de")), ["nl", "de"]);
        let de = CATALOGS.iter().position(|c| c.0 == "de").unwrap();
        assert_eq!(pick(&wanted_languages(Some("nl:de"))), de);
        // An earlier English preference wins over a later catalog.
        assert_eq!(pick(&wanted_languages(Some("en_GB:de"))), ENGLISH);
        assert_eq!(pick(&wanted_languages(Some("nl"))), ENGLISH);
        // Portuguese from Portugal falls back to the Brazilian catalog, before English.
        let pt = CATALOGS.iter().position(|c| c.0 == "pt_BR").unwrap();
        assert_eq!(pick(&wanted_languages(Some("pt_BR.UTF-8"))), pt);
        assert_eq!(pick(&wanted_languages(Some("pt_PT:en"))), pt);
    }
}
