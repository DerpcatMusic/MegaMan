//! Read-only browser model. Filesystem work never runs on the caller's thread.
//!
//! A browser owns one worker, one replaceable pending request and one replaceable
//! result. Directory contents are complete (there is no hidden result limit).
//! Memory therefore grows with the current directory, while concurrency and
//! pending work remain bounded. Cancellation is cooperative between OS calls;
//! an unresponsive filesystem call cannot be forcibly interrupted by std.

pub mod listing;
pub mod selection;
pub mod workspace;
pub use listing::ListingSnapshot;

use listing::{ListingData, MaterializeJob, TicketData};

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: EntryKind,
    /// Unknown until a dedicated metadata request is implemented. Enumeration
    /// deliberately avoids a stat for every file merely to populate a column.
    pub size: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}
/// Folders always sort before other entries; `descending` reverses only within each group.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SortOrder {
    pub key: SortKey,
    pub descending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadState {
    Loading,
    Complete,
    Failed(String),
}

struct Request {
    generation: u64,
    sort: SortOrder,
    path: PathBuf,
    filter: String,
    location_revision: u64,
}

struct Delivery {
    generation: u64,
    result: Result<ListingSnapshot, String>,
}

struct Shared {
    generation: AtomicU64,
    stop: AtomicBool,
    request: Mutex<Option<Request>>,
    result: Mutex<Option<Delivery>>,
    thread: OnceLock<thread::Thread>,
    ticket_registration: Mutex<Option<Arc<TicketData>>>,
    materialize: Mutex<Option<MaterializeJob>>,
}

impl Shared {
    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
    fn obsolete(&self, generation: u64) -> bool {
        self.stop.load(AtomicOrdering::Relaxed)
            || self.generation.load(AtomicOrdering::Acquire) != generation
    }
}

/// Own this model on the UI thread and call `poll` when the host wakes.
/// Navigation, history and filter changes only enqueue work; they do no I/O.
/// `poll` never waits for a worker and returns true only for an accepted result.
/// A host must arrange periodic worker polling while state is `Loading`.
/// Paths remain native and are never reconstructed from display strings.
/// Relative inputs keep the process-relative semantics of `std::fs::read_dir`.
pub struct Browser {
    path: PathBuf,
    snapshot: Option<ListingSnapshot>,
    state: LoadState,
    filter: String,
    sort: SortOrder,
    history: Vec<PathBuf>,
    history_index: usize,
    location_revision: u64,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Browser {
    pub fn new(path: PathBuf) -> Self {
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            request: Mutex::new(None),
            result: Mutex::new(None),
            thread: OnceLock::new(),
            ticket_registration: Mutex::new(None),
            materialize: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("browser-directory".into())
            .spawn(move || worker_loop(worker_shared));
        let mut browser = Self {
            history: vec![path.clone()],
            path,
            snapshot: None,
            state: LoadState::Loading,
            filter: String::new(),
            sort: SortOrder::default(),
            history_index: 0,
            location_revision: 0,
            shared,
            worker: None,
        };
        match worker {
            Ok(worker) => {
                browser.worker = Some(worker);
                browser.submit();
            }
            Err(error) => {
                browser.state = LoadState::Failed(format!("Start directory worker: {error}"))
            }
        }
        browser
    }

    pub fn navigate(&mut self, path: PathBuf) {
        if self.path != path {
            self.history.truncate(self.history_index + 1);
            self.history.push(path.clone());
            self.history_index += 1;
            self.path = path;
        }
        self.location_revision = self.location_revision.wrapping_add(1);
        self.submit();
    }

    pub fn poll(&mut self) -> bool {
        // Only tiny mailbox critical sections use these mutexes; filesystem and
        // filtering/sorting never hold them. try_lock also avoids UI contention.
        let delivery = match self.shared.result.try_lock() {
            Ok(mut result) => result.take(),
            Err(_) => None,
        };
        let Some(delivery) = delivery else {
            return false;
        };
        if delivery.generation != self.shared.generation.load(AtomicOrdering::Acquire) {
            return false;
        }
        match delivery.result {
            Ok(entries) => {
                self.snapshot = Some(entries);
                self.state = LoadState::Complete;
            }
            Err(message) => {
                self.snapshot = None;
                self.state = LoadState::Failed(message);
            }
        }
        true
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn entries(&self) -> &[Entry] {
        self.snapshot.as_ref().map_or(&[], ListingSnapshot::entries)
    }
    /// Cheap immutable handoff for the current accepted ordered membership.
    /// Loading/failed states have no snapshot; Complete empty listings do.
    pub fn snapshot(&self) -> Option<ListingSnapshot> {
        self.snapshot.clone()
    }

    pub fn state(&self) -> &LoadState {
        &self.state
    }

    pub fn sort(&self) -> SortOrder {
        self.sort
    }
    pub fn set_sort(&mut self, sort: SortOrder) {
        if self.sort != sort {
            self.sort = sort;
            self.submit();
        }
    }
    pub fn set_filter(&mut self, filter: &str) {
        if self.filter != filter {
            self.filter.clear();
            self.filter.push_str(filter);
            self.submit();
        }
    }

    pub fn back(&mut self) {
        if self.can_back() {
            self.history_index -= 1;
            self.path = self.history[self.history_index].clone();
            self.location_revision = self.location_revision.wrapping_add(1);
            self.submit();
        }
    }

    pub fn forward(&mut self) {
        if self.can_forward() {
            self.history_index += 1;
            self.path = self.history[self.history_index].clone();
            self.location_revision = self.location_revision.wrapping_add(1);
            self.submit();
        }
    }

    pub fn up(&mut self) {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            self.navigate(parent.to_path_buf());
        }
    }

    pub fn can_back(&self) -> bool {
        self.history_index > 0
    }
    pub fn can_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    fn submit(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let generation = self.shared.generation.fetch_add(1, AtomicOrdering::AcqRel) + 1;
        self.snapshot = None;
        self.state = LoadState::Loading;
        let request = Request {
            generation,
            sort: self.sort,
            path: self.path.clone(),
            filter: self.filter.clone(),
            location_revision: self.location_revision,
        };
        *self
            .shared
            .request
            .lock()
            .expect("directory request mutex poisoned") = Some(request);
        self.shared.wake();
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Release the external catalog handle before waking the worker. Its
        // strong lease ensures catalog destruction cannot happen here.
        self.snapshot = None;
        self.shared.stop.store(true, AtomicOrdering::Release);
        self.shared.wake();
        // The worker remains alive until all external snapshot/ticket leases
        // release, and never joins a potentially blocked filesystem syscall.
        if let Some(worker) = self.worker.take() {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

struct Record {
    entry: Entry,
    folded_name: String,
}

struct Cache {
    sort: SortOrder,
    path: PathBuf,
    location_revision: u64,
    records: Vec<Record>,
}

fn stale_meta(request: &Request) -> Cache {
    Cache {
        sort: request.sort,
        path: request.path.clone(),
        location_revision: request.location_revision,
        records: Vec::new(),
    }
}

fn worker_loop(shared: Arc<Shared>) {
    worker_loop_leased(shared, Vec::new());
}

fn worker_loop_leased(shared: Arc<Shared>, mut listings: Vec<Arc<ListingData>>) {
    // park/unpark retains a wake token, including wake-before-park. Drop
    // notifications need no lock and cannot lose a Condvar predicate race.
    let _ = shared.thread.set(thread::current());
    let mut cache: Option<Cache> = None;
    let mut tickets: Vec<Arc<TicketData>> = Vec::new();
    loop {
        let registered = shared
            .ticket_registration
            .lock()
            .expect("ticket registration mutex poisoned")
            .take();
        if let Some(ticket) = registered {
            tickets.push(ticket);
        }
        // Destructors run here, never while holding a mailbox mutex. Ticket
        // destruction releases its snapshot before catalog leases are reaped.
        tickets.retain(|ticket| Arc::strong_count(ticket) > 1);
        listings.retain(|listing| Arc::strong_count(listing) > 1);

        if shared.stop.load(AtomicOrdering::Acquire) {
            cache = None;
            let request = shared
                .request
                .lock()
                .expect("directory request mutex poisoned")
                .take();
            let result = shared
                .result
                .lock()
                .expect("directory result mutex poisoned")
                .take();
            let job = shared
                .materialize
                .lock()
                .expect("materialization mutex poisoned")
                .take();
            drop(request);
            drop(result);
            if let Some(job) = job {
                job.close();
            }
            tickets.retain(|ticket| Arc::strong_count(ticket) > 1);
            listings.retain(|listing| Arc::strong_count(listing) > 1);
            if listings.is_empty() && tickets.is_empty() {
                return;
            }
            thread::park();
            continue;
        }
        let job = shared
            .materialize
            .lock()
            .expect("materialization mutex poisoned")
            .take();
        if let Some(job) = job {
            job.execute(&shared);
            continue;
        }
        let request = shared
            .request
            .lock()
            .expect("directory request mutex poisoned")
            .take();
        let Some(request) = request else {
            thread::park();
            continue;
        };
        if shared.obsolete(request.generation) {
            continue;
        }
        if let Some(stale) = cache
            .take_if(|c| c.path == request.path && c.location_revision == request.location_revision)
        {
            cache = if stale.sort == request.sort {
                Some(stale)
            } else {
                sort_cancellable(stale.records, request.sort, request.generation, &shared).map(
                    |records| Cache {
                        sort: request.sort,
                        records,
                        ..stale_meta(&request)
                    },
                )
            };
            if cache.is_none() {
                continue;
            }
        }
        let result = if cache.as_ref().is_some_and(|cache| {
            cache.path == request.path && cache.location_revision == request.location_revision
        }) {
            filter_entries(&cache.as_ref().unwrap().records, &request, &shared)
        } else {
            match enumerate(&request, &shared) {
                Ok(Some(records)) => {
                    cache = Some(Cache {
                        sort: request.sort,
                        path: request.path.clone(),
                        location_revision: request.location_revision,
                        records,
                    });
                    filter_entries(&cache.as_ref().unwrap().records, &request, &shared)
                }
                Ok(None) => continue,
                Err(error) => Some(Err(error)),
            }
        };
        if shared.obsolete(request.generation) {
            continue;
        }
        if let Some(result) = result {
            let result = result.map(|entries| {
                let data = Arc::new(ListingData::new(entries));
                listings.push(Arc::clone(&data));
                ListingSnapshot {
                    data: Some(data),
                    owner: Arc::downgrade(&shared),
                }
            });
            let previous = shared
                .result
                .lock()
                .expect("directory result mutex poisoned")
                .replace(Delivery {
                    generation: request.generation,
                    result,
                });
            drop(previous);
        }
    }
}

fn enumerate(request: &Request, shared: &Shared) -> Result<Option<Vec<Record>>, String> {
    let entries = fs::read_dir(&request.path)
        .map_err(|error| format!("Read {}: {error}", request.path.display()))?;
    let mut records = Vec::new();
    for entry in entries {
        if shared.obsolete(request.generation) {
            return Ok(None);
        }
        let entry =
            entry.map_err(|error| format!("Read entry in {}: {error}", request.path.display()))?;
        let kind = entry
            .file_type()
            .map_err(|error| format!("Read type of {}: {error}", entry.path().display()))?;
        let name = entry.file_name();
        let meta = entry.metadata().ok();
        let folded_name = name.to_string_lossy().to_lowercase();
        records.push(Record {
            entry: Entry {
                path: entry.path(),
                name,
                kind: if kind.is_symlink() {
                    EntryKind::Symlink
                } else if kind.is_dir() {
                    EntryKind::Directory
                } else if kind.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                },
                size: meta.as_ref().filter(|m| !m.is_dir()).map(fs::Metadata::len),
                modified: meta.and_then(|m| m.modified().ok()),
            },
            folded_name,
        });
    }
    Ok(sort_cancellable(
        records,
        request.sort,
        request.generation,
        shared,
    ))
}

fn kind_rank(entry: &Entry) -> u8 {
    match entry.kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Other => 3,
    }
}
fn extension(record: &Record) -> &str {
    record
        .folded_name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or("", |(_, ext)| ext)
}
fn compare(order: SortOrder, a: &Record, b: &Record) -> Ordering {
    let by_name = |a: &Record, b: &Record| {
        a.folded_name
            .cmp(&b.folded_name)
            .then_with(|| a.entry.name.cmp(&b.entry.name))
    };
    let keyed = match order.key {
        SortKey::Name => Ordering::Equal,
        SortKey::Size => a.entry.size.cmp(&b.entry.size),
        SortKey::Modified => a.entry.modified.cmp(&b.entry.modified),
        SortKey::Kind => kind_rank(&a.entry)
            .cmp(&kind_rank(&b.entry))
            .then_with(|| extension(a).cmp(extension(b))),
    };
    let keyed = if order.descending {
        keyed.reverse()
    } else {
        keyed
    };
    let by_name_dir = if order.key == SortKey::Name && order.descending {
        by_name(a, b).reverse()
    } else {
        by_name(a, b)
    };
    (a.entry.kind != EntryKind::Directory)
        .cmp(&(b.entry.kind != EntryKind::Directory))
        .then(keyed)
        .then(by_name_dir)
}

/// Sort small chunks, then merge them with cancellation checkpoints. Unlike an
/// interrupted comparator, each comparison remains a consistent total order.
fn sort_cancellable(
    records: Vec<Record>,
    order: SortOrder,
    generation: u64,
    shared: &Shared,
) -> Option<Vec<Record>> {
    const CHUNK: usize = 1024;
    let mut chunks = VecDeque::new();
    let mut records = records.into_iter();
    loop {
        if shared.obsolete(generation) {
            return None;
        }
        let mut chunk: Vec<_> = records.by_ref().take(CHUNK).collect();
        if chunk.is_empty() {
            break;
        }
        chunk.sort_unstable_by(|a, b| compare(order, a, b));
        chunks.push_back(chunk);
    }
    // Merge adjacent pairs in rounds so an uneven final chunk cannot alter the
    // O(n log n) bound or result order.
    while chunks.len() > 1 {
        let count = chunks.len();
        for _ in 0..count / 2 {
            if shared.obsolete(generation) {
                return None;
            }
            let a = chunks.pop_front().unwrap();
            let b = chunks.pop_front().unwrap();
            let mut merged = Vec::with_capacity(a.len() + b.len());
            let mut a = a.into_iter().peekable();
            let mut b = b.into_iter().peekable();
            while a.peek().is_some() || b.peek().is_some() {
                if merged.len() % 256 == 0 && shared.obsolete(generation) {
                    return None;
                }
                let take_a = match (a.peek(), b.peek()) {
                    (Some(a), Some(b)) => compare(order, a, b) != Ordering::Greater,
                    (Some(_), None) => true,
                    _ => false,
                };
                merged.push(if take_a {
                    a.next().unwrap()
                } else {
                    b.next().unwrap()
                });
            }
            chunks.push_back(merged);
        }
        if count % 2 != 0 {
            let unpaired = chunks.pop_front().unwrap();
            chunks.push_back(unpaired);
        }
    }
    if shared.obsolete(generation) {
        None
    } else {
        Some(chunks.pop_front().unwrap_or_default())
    }
}

fn filter_entries(
    records: &[Record],
    request: &Request,
    shared: &Shared,
) -> Option<Result<Vec<Entry>, String>> {
    let filter = request.filter.to_lowercase();
    let mut entries = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if index % 256 == 0 && shared.obsolete(request.generation) {
            return None;
        }
        if filter.is_empty() || record.folded_name.contains(&filter) {
            entries.push(record.entry.clone());
        }
    }
    Some(Ok(entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            // Test scratch stays in this checkout (on the dev drive), never /tmp.
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                ".test-output-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
        fn file(&self, name: impl AsRef<Path>) {
            fs::write(self.path(name), b"fixture").unwrap();
        }
        fn dir(&self, name: impl AsRef<Path>) {
            fs::create_dir(self.path(name)).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn wait(browser: &mut Browser) {
        let until = Instant::now() + Duration::from_secs(15);
        while matches!(browser.state(), LoadState::Loading) {
            browser.poll();
            assert!(
                Instant::now() < until,
                "worker timed out: {:?}",
                browser.path()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn empty_complete_and_failed_roots_are_distinct() {
        let fixture = Fixture::new();
        let mut browser = Browser::new(fixture.0.clone());
        assert_eq!(browser.state(), &LoadState::Loading);
        wait(&mut browser);
        assert_eq!(browser.state(), &LoadState::Complete);
        assert!(browser.entries().is_empty());
        fixture.file("not-a-directory");
        browser.navigate(fixture.path("not-a-directory"));
        wait(&mut browser);
        assert!(matches!(browser.state(), LoadState::Failed(message) if !message.is_empty()));
        assert!(browser.entries().is_empty());
        browser.navigate(fixture.path("missing"));
        wait(&mut browser);
        assert!(matches!(browser.state(), LoadState::Failed(_)));
        browser.back();
        browser.back();
        wait(&mut browser);
        assert_eq!(browser.state(), &LoadState::Complete);
    }

    #[test]
    fn history_branches_and_up_reloads_directory_contents() {
        let fixture = Fixture::new();
        fixture.dir("a");
        fixture.dir("b");
        fixture.dir("c");
        let mut browser = Browser::new(fixture.0.clone());
        wait(&mut browser);
        browser.navigate(fixture.path("a"));
        browser.navigate(fixture.path("b"));
        browser.back();
        assert_eq!(browser.path(), fixture.path("a"));
        assert!(browser.can_forward());
        browser.forward();
        assert_eq!(browser.path(), fixture.path("b"));
        browser.back();
        browser.navigate(fixture.path("c"));
        assert!(!browser.can_forward());
        browser.up();
        wait(&mut browser);
        assert_eq!(browser.path(), fixture.0);
        fixture.file("new");
        browser.navigate(fixture.0.clone());
        wait(&mut browser);
        assert!(browser.entries().iter().any(|entry| entry.name == "new"));
    }

    #[test]
    fn stale_delivery_cannot_replace_current_state() {
        let fixture = Fixture::new();
        let mut browser = Browser::new(fixture.0.clone());
        wait(&mut browser);
        browser.navigate(fixture.path("missing"));
        wait(&mut browser);
        let current_generation = browser.shared.generation.load(AtomicOrdering::Acquire);
        *browser.shared.result.lock().unwrap() = Some(Delivery {
            generation: current_generation - 1,
            result: Ok(ListingSnapshot {
                data: Some(Arc::new(ListingData::new(Vec::new()))),
                owner: Arc::downgrade(&browser.shared),
            }),
        });
        assert!(!browser.poll());
        assert_eq!(browser.path(), fixture.path("missing"));
        assert!(matches!(browser.state(), LoadState::Failed(_)));
    }

    #[test]
    fn rapid_navigation_and_filtering_only_publish_latest_generation() {
        let fixture = Fixture::new();
        fixture.dir("old");
        fixture.dir("current");
        fs::write(fixture.path("old/stale"), b"").unwrap();
        fs::write(fixture.path("current/kept"), b"").unwrap();
        fs::write(fixture.path("current/hidden"), b"").unwrap();
        let mut browser = Browser::new(fixture.path("old"));
        for _ in 0..50 {
            browser.navigate(fixture.path("old"));
            browser.set_filter("stale");
            browser.navigate(fixture.path("current"));
            browser.set_filter("kept");
        }
        wait(&mut browser);
        assert_eq!(browser.state(), &LoadState::Complete);
        assert_eq!(browser.entries().len(), 1);
        assert_eq!(browser.entries()[0].path, fixture.path("current/kept"));
    }

    #[test]
    fn complete_filter_finds_late_match_beyond_old_five_thousand_limit() {
        let fixture = Fixture::new();
        for index in 0..6001 {
            fixture.file(format!("entry-{index:05}"));
        }
        fixture.file("zz-final-NEEDLE");
        let mut browser = Browser::new(fixture.0.clone());
        wait(&mut browser);
        assert_eq!(browser.entries().len(), 6002);
        assert!(browser
            .entries()
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name));
        browser.set_filter("needle");
        wait(&mut browser);
        assert_eq!(browser.entries().len(), 1);
        assert_eq!(browser.entries()[0].name, "zz-final-NEEDLE");
        browser.set_filter("");
        wait(&mut browser);
        assert_eq!(browser.entries().len(), 6002);
    }

    #[test]
    fn directories_first_then_case_folded_name_with_native_ties() {
        let mut entries: Vec<_> = [
            ("A", EntryKind::File),
            ("a", EntryKind::File),
            ("B", EntryKind::File),
            ("z-dir", EntryKind::Directory),
            ("b-dir", EntryKind::Directory),
        ]
        .into_iter()
        .map(|(name, kind)| Record {
            entry: Entry {
                path: PathBuf::from(name),
                name: OsString::from(name),
                kind,
                size: None,
                modified: None,
            },
            folded_name: name.to_lowercase(),
        })
        .collect();
        entries.sort_by(|a, b| compare(SortOrder::default(), a, b));
        let names: Vec<_> = entries
            .iter()
            .map(|record| record.entry.name.to_string_lossy())
            .collect();
        assert_eq!(names, ["b-dir", "z-dir", "A", "a", "B"]);
    }

    #[test]
    fn size_and_modified_are_listed_and_resorted_without_changing_folder_grouping() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("dir")).unwrap();
        fs::write(fixture.0.join("small"), b"1").unwrap();
        fs::write(fixture.0.join("big"), b"123456").unwrap();
        fs::write(fixture.0.join("mid.txt"), b"123").unwrap();
        let mut browser = Browser::new(fixture.0.clone());
        wait(&mut browser);
        let names = |b: &Browser| -> Vec<String> {
            b.entries()
                .iter()
                .map(|e| e.name.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names(&browser), ["dir", "big", "mid.txt", "small"]);
        let big = &browser.entries()[1];
        assert_eq!(big.size, Some(6));
        assert!(big.modified.is_some());
        assert_eq!(browser.entries()[0].size, None, "directories have no size");
        browser.set_sort(SortOrder {
            key: SortKey::Size,
            descending: true,
        });
        wait(&mut browser);
        assert_eq!(names(&browser), ["dir", "big", "mid.txt", "small"]);
        browser.set_sort(SortOrder {
            key: SortKey::Size,
            descending: false,
        });
        wait(&mut browser);
        assert_eq!(names(&browser), ["dir", "small", "mid.txt", "big"]);
        browser.set_sort(SortOrder {
            key: SortKey::Name,
            descending: true,
        });
        wait(&mut browser);
        assert_eq!(names(&browser), ["dir", "small", "mid.txt", "big"]);
        browser.set_sort(SortOrder {
            key: SortKey::Kind,
            descending: false,
        });
        wait(&mut browser);
        assert_eq!(names(&browser), ["dir", "big", "small", "mid.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_round_trip_and_symlinks_are_not_followed() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let native = OsString::from_vec(b"bad-\xff-name".to_vec());
        fixture.file(&native);
        symlink("missing-target", fixture.path("broken-link")).unwrap();
        let mut browser = Browser::new(fixture.0.clone());
        wait(&mut browser);
        assert_eq!(browser.state(), &LoadState::Complete);
        let entry = browser
            .entries()
            .iter()
            .find(|entry| entry.name == native)
            .unwrap();
        assert_eq!(entry.path.file_name().unwrap().as_bytes(), b"bad-\xff-name");
        assert_eq!(fs::read(&entry.path).unwrap(), b"fixture");
        assert_eq!(
            browser
                .entries()
                .iter()
                .find(|entry| entry.name == "broken-link")
                .unwrap()
                .kind,
            EntryKind::Symlink
        );
    }

    #[test]
    fn cancellation_discards_sort_and_filter_work() {
        let shared = Shared {
            generation: AtomicU64::new(2),
            stop: AtomicBool::new(false),
            request: Mutex::new(None),
            result: Mutex::new(None),
            thread: OnceLock::new(),
            ticket_registration: Mutex::new(None),
            materialize: Mutex::new(None),
        };
        let request = Request {
            generation: 1,
            location_revision: 0,
            path: PathBuf::new(),
            filter: String::new(),
            sort: SortOrder::default(),
        };
        assert!(sort_cancellable(Vec::new(), SortOrder::default(), 1, &shared).is_none());
        let record = Record {
            entry: Entry {
                path: PathBuf::from("a"),
                name: OsString::from("a"),
                kind: EntryKind::File,
                size: None,
                modified: None,
            },
            folded_name: "a".into(),
        };
        assert!(filter_entries(&[record], &request, &shared).is_none());
    }
}
