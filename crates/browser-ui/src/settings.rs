//! Persisted view defaults: `$XDG_CONFIG_HOME/megaman/settings.txt`, `key=value` lines.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Settings {
    pub show_hidden: bool,
    pub sort: SortOrder,
    pub inspector: bool,
    pub grid: bool,
    pub theme: u8,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            show_hidden: true,
            sort: SortOrder::default(),
            inspector: false,
            grid: false,
            theme: 0,
        }
    }
}
fn key_name(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
        SortKey::Kind => "kind",
    }
}
impl Settings {
    pub(super) fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match (k.trim(), v.trim()) {
                ("show_hidden", v) => s.show_hidden = v != "false",
                ("inspector", v) => s.inspector = v != "false",
                ("grid", v) => s.grid = v == "true",
                ("theme", v) => s.theme = v.parse().unwrap_or(0).min(THEMES - 1),
                ("sort_desc", v) => s.sort.descending = v == "true",
                ("sort", "size") => s.sort.key = SortKey::Size,
                ("sort", "modified") => s.sort.key = SortKey::Modified,
                ("sort", "kind") => s.sort.key = SortKey::Kind,
                ("sort", _) => s.sort.key = SortKey::Name,
                _ => {}
            }
        }
        s
    }
    pub(super) fn serialize(&self) -> String {
        format!(
            "show_hidden={}\ninspector={}\ngrid={}\ntheme={}\nsort={}\nsort_desc={}\n",
            self.show_hidden,
            self.inspector,
            self.grid,
            self.theme,
            key_name(self.sort.key),
            self.sort.descending
        )
    }
    fn file() -> Option<PathBuf> {
        if cfg!(test) {
            return None; // tests never touch the developer's real settings
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| FsPath::new(v).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| FsPath::new(&h).join(".config")))?;
        Some(base.join("megaman/settings.txt"))
    }
    pub(super) fn load() -> Self {
        Self::file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }
    pub(super) fn save(&self) {
        let Some(file) = Self::file() else {
            return;
        };
        let data = self.serialize();
        std::thread::spawn(move || {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = file.with_extension("tmp");
            if std::fs::write(&tmp, data).is_ok() {
                let _ = std::fs::rename(tmp, file);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips_and_ignores_unknown_or_malformed_lines() {
        let s = Settings {
            show_hidden: false,
            sort: SortOrder {
                key: SortKey::Modified,
                descending: true,
            },
            inspector: false,
            grid: true,
            theme: 2,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
        assert_eq!(Settings::parse("junk\nunknown=1\n"), Settings::default());
    }
}
