//! Change notification for one directory (inotify). Events only say "something changed";
//! the caller re-lists. A detached thread exits within one poll interval after drop.
use rustix::{
    event::{poll, PollFd, PollFlags},
    fs::inotify::{add_watch, init, CreateFlags, WatchFlags},
    io::{read, Errno},
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

const INTERVAL: Duration = Duration::from_millis(250);

pub struct DirWatch {
    rx: mpsc::Receiver<()>,
    stop: Arc<AtomicBool>,
}
impl DirWatch {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let fd = init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)?;
        add_watch(
            &fd,
            path,
            WatchFlags::CREATE
                | WatchFlags::DELETE
                | WatchFlags::MOVED_FROM
                | WatchFlags::MOVED_TO
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::ATTRIB
                | WatchFlags::DELETE_SELF
                | WatchFlags::MOVE_SELF,
        )?;
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        thread::Builder::new()
            .name("browser-watch".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while !flag.load(Ordering::Relaxed) {
                    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                    let Ok(ready) = poll(&mut fds, Some(&INTERVAL.try_into().unwrap_or_default()))
                    else {
                        continue;
                    };
                    if ready == 0 {
                        continue;
                    }
                    loop {
                        match read(&fd, &mut buf) {
                            Ok(0) | Err(Errno::AGAIN) => break,
                            Ok(_) => {
                                if tx.send(()).is_err() {
                                    return;
                                }
                            }
                            Err(Errno::INTR) => {}
                            Err(_) => return,
                        }
                    }
                }
            })?;
        Ok(Self { rx, stop })
    }
    /// True when at least one change arrived since the last call.
    pub fn changed(&self) -> bool {
        let mut any = false;
        while self.rx.try_recv().is_ok() {
            any = true;
        }
        any
    }
}
impl Drop for DirWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    fn create_and_delete_in_the_watched_folder_are_reported() {
        let dir = std::env::temp_dir().join(format!("watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = DirWatch::new(&dir).unwrap();
        assert!(!w.changed());
        std::fs::write(dir.join("a"), b"x").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !w.changed() {
            assert!(Instant::now() < until, "no event");
            thread::sleep(Duration::from_millis(10));
        }
        std::fs::remove_file(dir.join("a")).unwrap();
        while !w.changed() {
            assert!(Instant::now() < until, "no delete event");
            thread::sleep(Duration::from_millis(10));
        }
        drop(w);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
