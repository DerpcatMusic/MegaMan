//! Per-folder view overrides (layout and sort), remembered across sessions in
//! `$XDG_CONFIG_HOME/megaman/views.txt`: `grid|sort|desc<TAB>path` lines.
use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct View {
    pub grid: bool,
    pub sort: SortOrder,
}
const MAX: usize = 512;
static STORE: Mutex<Option<HashMap<PathBuf, View>>> = Mutex::new(None);

fn file() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| FsPath::new(v).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| FsPath::new(&h).join(".config")))?;
    Some(base.join("megaman/views.txt"))
}
fn parse(text: &str) -> HashMap<PathBuf, View> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((spec, path)) = line.split_once('\t') else {
            continue;
        };
        let mut f = spec.split('|');
        let (Some(grid), Some(sort), Some(desc)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let key = match sort {
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "kind" => SortKey::Kind,
            _ => SortKey::Name,
        };
        if FsPath::new(path).is_absolute() && map.len() < MAX {
            map.insert(
                PathBuf::from(path),
                View {
                    grid: grid == "grid",
                    sort: SortOrder {
                        key,
                        descending: desc == "desc",
                    },
                },
            );
        }
    }
    map
}
fn serialize(map: &HashMap<PathBuf, View>) -> String {
    let mut lines: Vec<String> = map
        .iter()
        .filter_map(|(p, v)| {
            let path = p.to_str().filter(|s| !s.contains('\n'))?;
            let sort = match v.sort.key {
                SortKey::Name => "name",
                SortKey::Size => "size",
                SortKey::Modified => "modified",
                SortKey::Kind => "kind",
            };
            Some(format!(
                "{}|{sort}|{}\t{path}",
                if v.grid { "grid" } else { "list" },
                if v.sort.descending { "desc" } else { "asc" }
            ))
        })
        .collect();
    lines.sort();
    lines.join("\n") + "\n"
}
fn with<R>(f: impl FnOnce(&mut HashMap<PathBuf, View>) -> R) -> R {
    let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(|| {
        file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| parse(&t))
            .unwrap_or_default()
    });
    f(map)
}
pub(super) fn get(path: &FsPath) -> Option<View> {
    with(|m| m.get(path).copied())
}
pub(super) fn set(path: &FsPath, view: View) {
    let data = with(|m| {
        if m.len() >= MAX && !m.contains_key(path) {
            return None;
        }
        (m.insert(path.to_path_buf(), view) != Some(view)).then(|| serialize(m))
    });
    let (Some(data), Some(file)) = (data, file()) else {
        return;
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips_and_skips_relative_paths() {
        let mut m = HashMap::new();
        let v = View {
            grid: true,
            sort: SortOrder {
                key: SortKey::Size,
                descending: true,
            },
        };
        m.insert(PathBuf::from("/a b"), v);
        assert_eq!(parse(&serialize(&m)), m);
        assert!(parse("list|name|asc\trel\n").is_empty());
    }
}
