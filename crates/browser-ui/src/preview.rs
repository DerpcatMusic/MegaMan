//! Bounded inspector previews: UTF-8 text heads and downscaled images, decoded on a worker.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

const TEXT_BYTES: usize = 8 * 1024;
const TEXT_LINES: usize = 24;
const MAX_IMAGE_FILE: u64 = 32 << 20;
const MAX_PIXELS: u64 = 64_000_000;
const THUMB: u32 = 512;

pub(super) enum Content {
    Text(String),
    Image(Arc<Image>),
    Unavailable(&'static str),
    Folder(FolderTotals),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct FolderTotals {
    files: u64,
    folders: u64,
    bytes: u64,
    /// Walk stopped at the entry cap, was cancelled, or hit unreadable directories.
    incomplete: bool,
}
const MAX_WALK: u64 = 2_000_000;

/// Iterative, symlink-safe size walk. Stops at the entry cap or on cancellation.
fn walk(root: &FsPath, cancel: &AtomicBool) -> FolderTotals {
    let mut totals = FolderTotals::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            totals.incomplete = true;
            continue;
        };
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) || totals.files + totals.folders >= MAX_WALK {
                totals.incomplete = true;
                return totals;
            }
            let Ok(meta) = entry.metadata() else {
                totals.incomplete = true;
                continue;
            };
            if meta.is_dir() {
                totals.folders += 1;
                pending.push(entry.path());
            } else {
                totals.files += 1;
                totals.bytes += meta.len();
            }
        }
    }
    totals
}
enum State {
    Loading(mpsc::Receiver<Content>, Arc<AtomicBool>),
    Ready(Content),
}
#[derive(Default)]
pub(super) struct Preview {
    current: Option<(PathBuf, State)>,
}

fn is_image(path: &FsPath) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp"
        )
    })
}
fn load(path: &FsPath, cancel: &AtomicBool) -> Content {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Content::Unavailable("Cannot read this file.");
    };
    if meta.is_dir() {
        return Content::Folder(walk(path, cancel));
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Content::Unavailable("Cannot read this file.");
    };
    if !meta.is_file() {
        return Content::Unavailable("Not a regular file.");
    }
    if is_image(path) {
        if meta.len() > MAX_IMAGE_FILE {
            return Content::Unavailable("Image is too large to preview.");
        }
        let decoded = image::ImageReader::open(path)
            .and_then(|r| r.with_guessed_format())
            .map_err(|_| ())
            .and_then(|r| {
                let (w, h) = r.into_dimensions().map_err(|_| ())?;
                if u64::from(w) * u64::from(h) > MAX_PIXELS {
                    return Err(());
                }
                image::ImageReader::open(path)
                    .map_err(|_| ())?
                    .with_guessed_format()
                    .map_err(|_| ())?
                    .decode()
                    .map_err(|_| ())
            });
        return match decoded {
            Ok(img) => {
                let thumb = img.thumbnail(THUMB, THUMB).to_rgba8();
                let (w, h) = thumb.dimensions();
                Image::rgba(w, h, thumb.into_raw())
                    .map_or(Content::Unavailable("Image has no pixels."), |i| {
                        Content::Image(Arc::new(i))
                    })
            }
            Err(()) => Content::Unavailable("Image could not be decoded (or is too large)."),
        };
    }
    let mut head = Vec::new();
    let read =
        std::fs::File::open(path).and_then(|f| f.take(TEXT_BYTES as u64).read_to_end(&mut head));
    if read.is_err() {
        return Content::Unavailable("Cannot read this file.");
    }
    if head.is_empty() {
        return Content::Unavailable("Empty file.");
    }
    // A cut multi-byte tail is fine; anything else invalid, or a NUL, means binary.
    let text = match std::str::from_utf8(&head) {
        Ok(t) => Some(t),
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&head[..e.valid_up_to()]).ok(),
        Err(_) => None,
    };
    match text {
        Some(t) if !t.contains('\0') => Content::Text(
            t.lines()
                .take(TEXT_LINES)
                .map(|l| l.chars().take(200).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => Content::Unavailable("Binary file · no preview."),
    }
}

impl Preview {
    /// Start loading `path` unless it is already current. `None` clears.
    pub(super) fn show(&mut self, path: Option<&FsPath>) {
        match path {
            None => {
                self.cancel_current();
                self.current = None;
            }
            Some(p) if self.current.as_ref().is_some_and(|(c, _)| c == p) => {}
            Some(p) => {
                let (tx, rx) = mpsc::channel();
                let owned = p.to_path_buf();
                let cancel = Arc::new(AtomicBool::new(false));
                let flag = Arc::clone(&cancel);
                std::thread::spawn(move || {
                    let _ = tx.send(load(&owned, &flag));
                });
                self.cancel_current();
                self.current = Some((p.to_path_buf(), State::Loading(rx, cancel)));
            }
        }
    }
    fn cancel_current(&self) {
        if let Some((_, State::Loading(_, cancel))) = &self.current {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    pub(super) fn poll(&mut self) -> bool {
        if let Some((_, state)) = &mut self.current
            && let State::Loading(rx, _) = state
        {
            match rx.try_recv() {
                Ok(content) => {
                    *state = State::Ready(content);
                    return true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    *state = State::Ready(Content::Unavailable("Preview failed."));
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        false
    }
    pub(super) fn view(&self, width: f64) -> Option<El> {
        let (_, state) = self.current.as_ref()?;
        Some(match state {
            State::Loading(..) => detail("Loading preview…").pad(14),
            State::Ready(Content::Folder(t)) => detail(format!(
                "{} files · {} folders · {}{}",
                t.files,
                t.folders,
                bytes(t.bytes),
                if t.incomplete { " (partial)" } else { "" }
            ))
            .wrap()
            .pad(14),
            State::Ready(Content::Unavailable(why)) => detail(*why).wrap().pad(14),
            State::Ready(Content::Text(text)) => col([label(text.clone()).wrap().w(pct(100))])
                .pad(14)
                .named("Text preview"),
            State::Ready(Content::Image(img)) => {
                let side = (width - 28.).max(48.);
                let ratio = f64::from(img.height) / f64::from(img.width.max(1));
                col([])
                    .w(side)
                    .h((side * ratio).min(side * 1.5))
                    .fill(Fill::Image(Arc::clone(img), Fit::Contain))
                    .named("Image preview")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static NO: AtomicBool = AtomicBool::new(false);
    fn tmp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("preview-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }
    #[test]
    fn text_binary_empty_and_image_are_classified_and_bounded() {
        let long = "line\n".repeat(100);
        let p = tmp("a.txt", long.as_bytes());
        let Content::Text(t) = load(&p, &NO) else {
            panic!("text")
        };
        assert_eq!(t.lines().count(), TEXT_LINES);
        let p = tmp("b.bin", b"abc\0def");
        assert!(matches!(load(&p, &NO), Content::Unavailable(_)));
        let p = tmp("c.txt", b"");
        assert!(matches!(load(&p, &NO), Content::Unavailable("Empty file.")));
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(1000, 500)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let p = tmp("d.png", &png);
        let Content::Image(img) = load(&p, &NO) else {
            panic!("image")
        };
        assert_eq!((img.width, img.height), (512, 256));
        let p = tmp("e.png", b"not a png");
        assert!(matches!(load(&p, &NO), Content::Unavailable(_)));
    }
    #[test]
    fn folder_walk_counts_without_following_symlinks_and_honours_cancel() {
        let d = std::env::temp_dir().join(format!("walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/f1"), b"12345").unwrap();
        std::fs::write(d.join("a/b/f2"), b"123").unwrap();
        std::os::unix::fs::symlink(&d, d.join("loop")).unwrap();
        let t = walk(&d, &NO);
        assert_eq!(
            (t.files, t.folders, t.bytes, t.incomplete),
            (3, 2, 8 + t.bytes - 8, false)
        );
        assert!(t.bytes >= 8);
        let stop = AtomicBool::new(true);
        assert!(walk(&d, &stop).incomplete);
        std::fs::remove_dir_all(d).unwrap();
    }
}
