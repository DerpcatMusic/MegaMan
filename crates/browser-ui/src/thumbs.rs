//! Bounded thumbnail cache: one decode worker, requests only for visible image tiles.
use super::*;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, mpsc},
};

const CAP: usize = 512;
const SIDE: u32 = 96;
const MAX_FILE: u64 = 16 << 20;

enum Slot {
    Pending,
    Ready(Arc<Image>),
    Failed,
}
pub(super) struct Thumbs {
    slots: HashMap<PathBuf, Slot>,
    order: VecDeque<PathBuf>,
    jobs: mpsc::Sender<PathBuf>,
    done: mpsc::Receiver<(PathBuf, Option<Arc<Image>>)>,
}

fn decode(path: &FsPath) -> Option<Arc<Image>> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let img = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let thumb = img.thumbnail(SIDE, SIDE).to_rgba8();
    let (w, h) = thumb.dimensions();
    Image::rgba(w, h, thumb.into_raw()).map(Arc::new)
}
pub(super) fn is_image(path: &FsPath) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp"
        )
    })
}

impl Default for Thumbs {
    fn default() -> Self {
        let (jobs, job_rx) = mpsc::channel::<PathBuf>();
        let (done_tx, done) = mpsc::channel();
        // Exits when the Thumbs (sender) is dropped.
        std::thread::spawn(move || {
            while let Ok(path) = job_rx.recv() {
                let result = decode(&path);
                if done_tx.send((path, result)).is_err() {
                    break;
                }
            }
        });
        Self {
            slots: HashMap::new(),
            order: VecDeque::new(),
            jobs,
            done,
        }
    }
}
impl Thumbs {
    /// The ready thumbnail, requesting a decode the first time a path is seen.
    pub(super) fn get(&mut self, path: &FsPath) -> Option<Arc<Image>> {
        match self.slots.get(path) {
            Some(Slot::Ready(img)) => return Some(Arc::clone(img)),
            Some(_) => return None,
            None => {}
        }
        if self.order.len() >= CAP
            && let Some(old) = self.order.pop_front()
        {
            self.slots.remove(&old);
        }
        self.slots.insert(path.to_path_buf(), Slot::Pending);
        self.order.push_back(path.to_path_buf());
        let _ = self.jobs.send(path.to_path_buf());
        None
    }
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok((path, result)) = self.done.try_recv() {
            if let Some(slot) = self.slots.get_mut(&path) {
                *slot = result.map_or(Slot::Failed, Slot::Ready);
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_once_bounds_the_cache_and_marks_bad_files_failed() {
        let dir = std::env::temp_dir().join(format!("thumbs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(400, 200)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(dir.join("a.png"), &png).unwrap();
        std::fs::write(dir.join("bad.png"), b"nope").unwrap();
        let mut t = Thumbs::default();
        assert!(t.get(&dir.join("a.png")).is_none());
        assert!(t.get(&dir.join("bad.png")).is_none());
        for _ in 0..500 {
            t.poll();
            if t.get(&dir.join("a.png")).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let img = t.get(&dir.join("a.png")).expect("decoded");
        assert_eq!((img.width, img.height), (96, 48));
        assert!(
            t.get(&dir.join("bad.png")).is_none(),
            "failures are remembered, not retried"
        );
        for i in 0..CAP + 10 {
            t.get(&dir.join(format!("x{i}.png")));
        }
        assert!(t.slots.len() <= CAP);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
