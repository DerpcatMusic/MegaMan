//! Folder filter syntax: plain words match names; `kind:dir|file|link`,
//! `ext:png`, `size:>10mb` / `size:<1kb` and `modified:<7d` (newer than) /
//! `modified:>30d` (older than) narrow further. All terms must match.
use crate::{Entry, EntryKind};
use std::time::{Duration, SystemTime};

#[derive(Debug, Default, PartialEq)]
pub struct Query {
    words: Vec<String>,
    kind: Option<EntryKind>,
    ext: Option<String>,
    /// (is_min, bytes)
    size: Option<(bool, u64)>,
    /// (older_than, age)
    age: Option<(bool, Duration)>,
}
fn number(s: &str, units: &[(&str, u64)]) -> Option<u64> {
    let s = s.to_lowercase();
    let at = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(at);
    let mult = if unit.is_empty() {
        1
    } else {
        units.iter().find(|u| u.0 == unit)?.1
    };
    n.parse::<u64>().ok()?.checked_mul(mult)
}
impl Query {
    pub fn parse(text: &str) -> Self {
        let mut q = Self::default();
        for term in text.split_whitespace() {
            let lower = term.to_lowercase();
            let parsed = match lower.split_once(':') {
                Some(("kind", v)) => {
                    q.kind = match v {
                        "dir" | "folder" => Some(EntryKind::Directory),
                        "file" => Some(EntryKind::File),
                        "link" | "symlink" => Some(EntryKind::Symlink),
                        _ => None,
                    };
                    q.kind.is_some()
                }
                Some(("ext", v)) if !v.is_empty() => {
                    q.ext = Some(v.trim_start_matches('.').to_owned());
                    true
                }
                Some(("size", v)) => {
                    let (min, rest) = match v.as_bytes().first() {
                        Some(b'>') => (true, &v[1..]),
                        Some(b'<') => (false, &v[1..]),
                        _ => (true, v),
                    };
                    let units = [("b", 1), ("kb", 1 << 10), ("mb", 1 << 20), ("gb", 1 << 30)];
                    q.size = number(rest, &units).map(|n| (min, n));
                    q.size.is_some()
                }
                Some(("modified", v)) => {
                    let (older, rest) = match v.as_bytes().first() {
                        Some(b'>') => (true, &v[1..]),
                        Some(b'<') => (false, &v[1..]),
                        _ => (false, v),
                    };
                    let units = [
                        ("h", 3600),
                        ("d", 86_400),
                        ("w", 604_800),
                        ("y", 31_536_000),
                    ];
                    q.age = number(rest, &units).map(|n| (older, Duration::from_secs(n)));
                    q.age.is_some()
                }
                _ => false,
            };
            if !parsed {
                q.words.push(lower);
            }
        }
        q
    }
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    /// `folded_name` is the entry name lower-cased once by the caller.
    pub fn matches(&self, folded_name: &str, entry: &Entry, now: SystemTime) -> bool {
        if !self.words.iter().all(|w| folded_name.contains(w.as_str())) {
            return false;
        }
        if self.kind.is_some_and(|k| k != entry.kind) {
            return false;
        }
        if self
            .ext
            .as_deref()
            .is_some_and(|ext| folded_name.rsplit_once('.').map(|(_, e)| e) != Some(ext))
        {
            return false;
        }
        if let Some((min, n)) = self.size {
            match entry.size {
                Some(s) if (s >= n) == min => {}
                _ => return false,
            }
        }
        if let Some((older, age)) = self.age {
            let Some(m) = entry.modified else {
                return false;
            };
            let actual = now.duration_since(m).unwrap_or_default();
            if (actual > age) != older {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn entry(name: &str, kind: EntryKind, size: Option<u64>, age_days: u64) -> Entry {
        Entry {
            path: PathBuf::from(name),
            name: name.into(),
            kind,
            size,
            modified: Some(SystemTime::now() - Duration::from_secs(age_days * 86_400)),
        }
    }
    #[test]
    fn terms_combine_and_unknown_syntax_falls_back_to_words() {
        let now = SystemTime::now();
        let photo = entry("Holiday.PNG", EntryKind::File, Some(5 << 20), 3);
        let dir = entry("pics", EntryKind::Directory, None, 40);
        let m = |q: &str, e: &Entry| {
            Query::parse(q).matches(&e.name.to_string_lossy().to_lowercase(), e, now)
        };
        assert!(m("holiday ext:png size:>1mb modified:<7d", &photo));
        assert!(!m("ext:jpg", &photo));
        assert!(!m("size:<1mb", &photo));
        assert!(!m("modified:>30d", &photo));
        assert!(m("kind:dir modified:>30d", &dir));
        assert!(!m("kind:file", &dir));
        assert!(
            !m("size:>1b", &dir),
            "unknown sizes never match a size term"
        );
        assert!(!m("kind:bogus", &photo), "unparsed term becomes a word");
        assert!(Query::parse("  ").is_empty());
    }
}
