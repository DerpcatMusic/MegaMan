//! App-owned bookmarks, recent folders and mounted volumes. State lives in
//! `$XDG_CONFIG_HOME/megaman/places.txt`; writes happen on a short-lived thread.
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_RECENT: usize = 8;
const MAX_BOOKMARKS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Volume {
    pub name: String,
    pub path: PathBuf,
}
#[derive(Default)]
pub(super) struct Places {
    pub bookmarks: Vec<PathBuf>,
    pub recent: Vec<PathBuf>,
    volumes: Arc<Mutex<Vec<Volume>>>,
    file: Option<PathBuf>,
}

fn config_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| Path::new(v).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("megaman/places.txt"))
}
fn parse(text: &[u8]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut bookmarks, mut recent) = (Vec::new(), Vec::new());
    for line in text.split(|b| *b == b'\n') {
        let Some((tag, path)) = line.split_at_checked(2) else {
            continue;
        };
        let path = PathBuf::from(OsString::from_vec(path.to_vec()));
        if !path.is_absolute() {
            continue;
        }
        match tag {
            b"b\t" if bookmarks.len() < MAX_BOOKMARKS => bookmarks.push(path),
            b"r\t" if recent.len() < MAX_RECENT => recent.push(path),
            _ => {}
        }
    }
    (bookmarks, recent)
}
fn serialize(bookmarks: &[PathBuf], recent: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, list) in [(&b"b\t"[..], bookmarks), (&b"r\t"[..], recent)] {
        for path in list {
            let bytes = path.as_os_str().as_bytes();
            if bytes.contains(&b'\n') {
                continue; // line format cannot represent it
            }
            out.extend_from_slice(tag);
            out.extend_from_slice(bytes);
            out.push(b'\n');
        }
    }
    out
}
/// Mount points of removable or user-mounted filesystems from /proc/self/mountinfo.
pub(super) fn parse_mountinfo(text: &str) -> Vec<Volume> {
    let mut volumes = Vec::new();
    for line in text.lines() {
        let Some((head, tail)) = line.split_once(" - ") else {
            continue;
        };
        let Some(point) = head.split(' ').nth(4) else {
            continue;
        };
        let fstype = tail.split(' ').next().unwrap_or("");
        let point = unescape(point);
        let user_mount = ["/media/", "/run/media/", "/mnt/"]
            .iter()
            .any(|p| point.starts_with(p));
        if user_mount && !matches!(fstype, "tmpfs" | "proc" | "sysfs" | "overlay" | "autofs") {
            let name = Path::new(&point)
                .file_name()
                .map_or_else(|| point.clone(), |n| n.to_string_lossy().into_owned());
            volumes.push(Volume {
                name,
                path: PathBuf::from(point),
            });
        }
    }
    volumes.sort_by(|a, b| a.path.cmp(&b.path));
    volumes.dedup();
    volumes
}
fn unescape(field: &str) -> String {
    let (mut out, bytes) = (Vec::new(), field.as_bytes());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Places {
    pub(super) fn load() -> Self {
        let file = config_file();
        let (bookmarks, recent) = file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .map(|t| parse(&t))
            .unwrap_or_default();
        let places = Self {
            bookmarks,
            recent,
            volumes: Arc::default(),
            file,
        };
        places.refresh_volumes();
        places
    }
    pub(super) fn volumes(&self) -> Vec<Volume> {
        self.volumes.lock().map(|v| v.clone()).unwrap_or_default()
    }
    pub(super) fn refresh_volumes(&self) {
        let slot = Arc::clone(&self.volumes);
        std::thread::spawn(move || {
            if let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo")
                && let Ok(mut v) = slot.lock()
            {
                *v = parse_mountinfo(&text);
            }
        });
    }
    fn save(&self) {
        let Some(file) = self.file.clone() else {
            return;
        };
        let data = serialize(&self.bookmarks, &self.recent);
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
    /// Add or remove; returns true when the folder is now bookmarked.
    pub(super) fn toggle_bookmark(&mut self, path: &Path) -> bool {
        let now = if let Some(i) = self.bookmarks.iter().position(|b| b == path) {
            self.bookmarks.remove(i);
            false
        } else if self.bookmarks.len() < MAX_BOOKMARKS {
            self.bookmarks.push(path.to_path_buf());
            true
        } else {
            false
        };
        self.save();
        now
    }
    pub(super) fn visit(&mut self, path: &Path) {
        if self.recent.first().is_some_and(|p| p == path) {
            return;
        }
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips_and_rejects_relative_or_multiline_entries() {
        let b = vec![PathBuf::from("/a b"), PathBuf::from("/c")];
        let r = vec![PathBuf::from("/x\ny"), PathBuf::from("/z")];
        let (pb, pr) = parse(&serialize(&b, &r));
        assert_eq!(pb, b);
        assert_eq!(pr, vec![PathBuf::from("/z")]);
        assert!(parse(b"b\trelative\n").0.is_empty());
    }
    #[test]
    fn mountinfo_keeps_user_mounts_and_decodes_escapes() {
        let text = "41 1 0:35 /@ / rw - btrfs /dev/nvme0n1p3 rw\n\
                    50 41 8:1 / /mnt/Windows11 rw - fuseblk /dev/sda1 rw\n\
                    51 41 8:2 / /run/media/u/My\\040Disk rw - ext4 /dev/sdb1 rw\n\
                    52 41 0:40 / /mnt/ram rw - tmpfs tmpfs rw\n";
        let v = parse_mountinfo(text);
        assert_eq!(
            v.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["Windows11", "My Disk"]
        );
        assert_eq!(v[1].path, PathBuf::from("/run/media/u/My Disk"));
    }
    #[test]
    fn visit_moves_to_front_and_bounds_the_list() {
        let mut p = Places::default();
        for i in 0..12 {
            p.visit(Path::new(&format!("/d{i}")));
        }
        p.visit(Path::new("/d5"));
        assert_eq!(p.recent.len(), MAX_RECENT);
        assert_eq!(p.recent[0], PathBuf::from("/d5"));
        assert_eq!(
            p.recent.iter().filter(|x| **x == Path::new("/d5")).count(),
            1
        );
    }
}
