//! Folder sizes for the Size column. One worker walks requested folders (never following
//! symlinks); a row is Pending until measured, Partial when a limit or an unreadable child cut
//! the walk short, and never shows an implicit zero.
use super::*;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const MAX_ENTRIES: u64 = 5_000_000;
const MAX_TIME: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DirSize {
    Measured(u64),
    Partial(u64),
    Unavailable,
}
impl DirSize {
    pub(super) fn text(self) -> String {
        match self {
            Self::Measured(n) => bytes(n),
            Self::Partial(n) => format!("≥ {}", bytes(n)),
            Self::Unavailable => "—".into(),
        }
    }
}

/// Apparent size of everything under `root`, or None when `epoch` moved on (abandoned).
fn walk(root: &FsPath, mine: u64, epoch: &AtomicU64) -> Option<DirSize> {
    let (mut total, mut seen, mut partial) = (0u64, 0u64, false);
    let start = Instant::now();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if epoch.load(Ordering::Relaxed) != mine {
            return None;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            if dir == root {
                return Some(DirSize::Unavailable);
            }
            partial = true;
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES || start.elapsed() > MAX_TIME {
                return Some(DirSize::Partial(total));
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => total += entry.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    Some(if partial {
        DirSize::Partial(total)
    } else {
        DirSize::Measured(total)
    })
}

#[derive(Default)]
pub(super) struct DirSizes {
    owner: PathBuf,
    done: HashMap<PathBuf, DirSize>,
    asked: HashSet<PathBuf>,
    worker: Option<mpsc::Sender<(u64, PathBuf)>>,
    results: Option<mpsc::Receiver<(u64, PathBuf, DirSize)>>,
    epoch: Arc<AtomicU64>,
}
impl DirSizes {
    /// Forget everything when the listed folder changes; abandons walks in flight.
    pub(super) fn follow(&mut self, folder: &FsPath) {
        if self.owner != folder {
            self.owner = folder.to_path_buf();
            self.reset();
        }
    }
    pub(super) fn reset(&mut self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.done.clear();
        self.asked.clear();
    }
    pub(super) fn get(&self, path: &FsPath) -> Option<DirSize> {
        self.done.get(path).copied()
    }
    pub(super) fn want(&mut self, path: PathBuf) {
        if self.done.contains_key(&path) || !self.asked.insert(path.clone()) {
            return;
        }
        let mine = self.epoch.load(Ordering::Relaxed);
        if self.worker.is_none() {
            let (ask, rx) = mpsc::channel::<(u64, PathBuf)>();
            let (tx, results) = mpsc::channel();
            let epoch = self.epoch.clone();
            std::thread::spawn(move || {
                while let Ok((mine, path)) = rx.recv() {
                    if epoch.load(Ordering::Relaxed) != mine {
                        continue;
                    }
                    if let Some(size) = walk(&path, mine, &epoch)
                        && tx.send((mine, path, size)).is_err()
                    {
                        break;
                    }
                }
            });
            self.worker = Some(ask);
            self.results = Some(results);
        }
        if let Some(worker) = &self.worker {
            let _ = worker.send((mine, path));
        }
    }
    /// True when a result arrived and the list should repaint.
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        let now = self.epoch.load(Ordering::Relaxed);
        while let Some((mine, path, size)) = self.results.as_ref().and_then(|r| r.try_recv().ok()) {
            if mine == now {
                self.done.insert(path, size);
                changed = true;
            }
        }
        changed
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn measures_nested_files_without_following_symlinks_and_reports_unreadable_roots() {
        let d = std::env::temp_dir().join(format!("dirsize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/one"), [0u8; 10]).unwrap();
        std::fs::write(d.join("a/b/two"), [0u8; 5]).unwrap();
        std::os::unix::fs::symlink("/usr", d.join("a/link")).unwrap();
        let epoch = AtomicU64::new(0);
        assert_eq!(walk(&d.join("a"), 0, &epoch), Some(DirSize::Measured(15)));
        assert_eq!(walk(&d.join("nope"), 0, &epoch), Some(DirSize::Unavailable));
        epoch.store(1, Ordering::Relaxed);
        assert_eq!(
            walk(&d.join("a"), 0, &epoch),
            None,
            "abandoned walk reports nothing"
        );
        let mut sizes = DirSizes::default();
        sizes.want(d.join("a"));
        let end = Instant::now() + Duration::from_secs(5);
        while sizes.get(&d.join("a")).is_none() && Instant::now() < end {
            sizes.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(sizes.get(&d.join("a")), Some(DirSize::Measured(15)));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
