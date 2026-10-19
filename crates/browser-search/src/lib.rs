//! Background filename indexing. See README.md for scope and integration contracts.
mod session;
pub use session::*;

use caseless::default_case_fold_str;
use std::{
    cmp::Ordering,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    thread,
    time::{Duration, Instant},
};

/// Unicode version used by the full default (non-Turkic) case-folding tables.
pub const CASE_FOLD_UNICODE_VERSION: (u64, u64, u64) = caseless::UNICODE_VERSION;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Generation(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Coverage {
    Idle,
    Indexing,
    Complete,
    Partial,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    /// Display/search representation only. Never use this to perform operations.
    pub display_name: String,
    folded_name: String,
    folded_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub operation: &'static str,
    pub kind: io::ErrorKind,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct IndexOptions {
    /// Explicit directory roots; an empty list produces Partial, never a whole-disk scan.
    pub roots: Vec<PathBuf>,
    /// Native absolute or relative subtrees to omit. Linux defaults omit /proc, /sys, /dev.
    pub excluded_roots: Vec<PathBuf>,
    /// Minimum interval between nonterminal publications (default 100 ms).
    pub publish_interval: Duration,
}

impl IndexOptions {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            excluded_roots: if cfg!(target_os = "linux") {
                vec!["/proc".into(), "/sys".into(), "/dev".into()]
            } else {
                Vec::new()
            },
            publish_interval: Duration::from_millis(100),
        }
    }
}

// Persistent immutable chunks make publication O(1), independent of catalog size.
#[derive(Debug)]
struct Chunk {
    entries: Vec<IndexEntry>,
    previous: Option<Arc<Chunk>>,
}
impl Drop for Chunk {
    fn drop(&mut self) {
        // Avoid recursive destruction of a long exclusively-owned chunk chain.
        let mut next = self.previous.take();
        while let Some(chunk) = next {
            match Arc::try_unwrap(chunk) {
                Ok(mut chunk) => next = chunk.previous.take(),
                Err(_) => break,
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexSnapshot {
    pub generation: Generation,
    pub coverage: Coverage,
    pub indexed_entries: usize,
    pub directories_scanned: usize,
    /// All symlinks are results but never traversal targets.
    pub symlinks_not_followed: usize,
    pub excluded_subtrees: usize,
    pub error_count: usize,
    pub requested_roots: Arc<[PathBuf]>,
    pub effective_roots: Arc<[PathBuf]>,
    pub excluded_roots: Arc<[PathBuf]>,
    /// First 64 errors; error_count always counts every failure.
    pub diagnostics: Arc<[Diagnostic]>,
    head: Option<Arc<Chunk>>,
}

impl Default for IndexSnapshot {
    fn default() -> Self {
        Self {
            generation: Generation(0),
            coverage: Coverage::Idle,
            indexed_entries: 0,
            directories_scanned: 0,
            symlinks_not_followed: 0,
            excluded_subtrees: 0,
            error_count: 0,
            requested_roots: Arc::from([]),
            effective_roots: Arc::from([]),
            excluded_roots: Arc::from([]),
            diagnostics: Arc::from([]),
            head: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortOrder {
    #[default]
    NameAscending,
    NameDescending,
    PathAscending,
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Caller-owned identity for rejecting a superseded query, independent of index generation.
    pub request_id: u64,
    pub text: String,
    pub offset: usize,
    pub limit: usize,
    pub sort: SortOrder,
}

#[derive(Clone, Debug)]
pub struct QueryPage {
    pub generation: Generation,
    pub request_id: u64,
    pub coverage: Coverage,
    /// Exact for this immutable snapshot, even when limit is zero.
    pub total_matches: usize,
    pub offset: usize,
    pub entries: Vec<IndexEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryCancelled;

impl IndexSnapshot {
    /// Memory-only but potentially expensive: invoke on a query worker, not in render/layout.
    pub fn query(&self, query: &Query) -> QueryPage {
        self.query_cancellable(query, &AtomicBool::new(false))
            .expect("uncancelled query")
    }

    /// Cancellation is checked while matching and sorting. No page is returned on cancellation.
    pub fn query_cancellable(
        &self,
        query: &Query,
        cancelled: &AtomicBool,
    ) -> Result<QueryPage, QueryCancelled> {
        let needle = default_case_fold_str(&query.text);
        let mut matches = Vec::new();
        for chunk in std::iter::successors(self.head.as_deref(), |chunk| chunk.previous.as_deref())
        {
            for entry in &chunk.entries {
                if cancelled.load(AtomicOrdering::Relaxed) {
                    return Err(QueryCancelled);
                }
                if entry.folded_name.contains(&needle) {
                    matches.push(entry);
                }
            }
        }
        // Iterative merge sort provides cancellation between bounded runs, unlike sort_unstable.
        let compare = |a: &&IndexEntry, b: &&IndexEntry| {
            let name = || {
                a.folded_name
                    .cmp(&b.folded_name)
                    .then_with(|| a.path.cmp(&b.path))
            };
            match query.sort {
                SortOrder::NameAscending => name(),
                SortOrder::NameDescending => name().reverse(),
                SortOrder::PathAscending => a
                    .folded_path
                    .cmp(&b.folded_path)
                    .then_with(|| a.path.cmp(&b.path)),
            }
        };
        cancellable_sort(&mut matches, compare, cancelled)?;
        let total_matches = matches.len();
        let mut entries = Vec::new();
        for entry in matches.into_iter().skip(query.offset).take(query.limit) {
            if cancelled.load(AtomicOrdering::Relaxed) {
                return Err(QueryCancelled);
            }
            entries.push(entry.clone());
        }
        if cancelled.load(AtomicOrdering::Relaxed) {
            return Err(QueryCancelled);
        }
        Ok(QueryPage {
            generation: self.generation,
            request_id: query.request_id,
            coverage: self.coverage,
            total_matches,
            offset: query.offset,
            entries,
        })
    }
}

fn cancellable_sort<T: Copy>(
    values: &mut [T],
    compare: impl Fn(&T, &T) -> Ordering,
    cancel: &AtomicBool,
) -> Result<(), QueryCancelled> {
    let mut scratch = values.to_vec();
    let mut width = 1usize;
    while width < values.len() {
        for base in (0..values.len()).step_by(width.saturating_mul(2)) {
            let middle = base.saturating_add(width).min(values.len());
            let end = middle.saturating_add(width).min(values.len());
            let (mut left, mut right) = (base, middle);
            for target in &mut scratch[base..end] {
                if cancel.load(AtomicOrdering::Relaxed) {
                    return Err(QueryCancelled);
                }
                if left < middle
                    && (right >= end || compare(&values[left], &values[right]) != Ordering::Greater)
                {
                    *target = values[left];
                    left += 1;
                } else {
                    *target = values[right];
                    right += 1;
                }
            }
        }
        values.copy_from_slice(&scratch);
        width = width.saturating_mul(2);
    }
    Ok(())
}

struct Job {
    generation: Generation,
    options: IndexOptions,
    cancel: Arc<AtomicBool>,
}
struct State {
    snapshot: Arc<IndexSnapshot>,
    // A single catalog awaiting worker-side destruction, never an unbounded queue.
    retired: Option<Arc<IndexSnapshot>>,
    pending: Option<Job>,
    cancel: Arc<AtomicBool>,
    shutdown: bool,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// One persistent worker and at most one pending replacement per service.
pub struct SearchIndex {
    shared: Arc<Shared>,
}
impl SearchIndex {
    pub fn new() -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                snapshot: Arc::new(IndexSnapshot::default()),
                retired: None,
                pending: None,
                cancel: Arc::new(AtomicBool::new(false)),
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let worker = shared.clone();
        thread::Builder::new()
            .name("filename-index".into())
            .spawn(move || worker_loop(worker))?;
        Ok(Self { shared })
    }

    pub fn start(&self, options: IndexOptions) -> Generation {
        let mut state = self.shared.state.lock().expect("index state poisoned");
        state.cancel.store(true, AtomicOrdering::Relaxed);
        let generation = Generation(
            state
                .snapshot
                .generation
                .0
                .checked_add(1)
                .expect("generation exhausted"),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let replacement = Arc::new(IndexSnapshot {
            generation,
            coverage: Coverage::Indexing,
            requested_roots: options.roots.clone().into(),
            ..IndexSnapshot::default()
        });
        retire_snapshot(&mut state, replacement);
        state.cancel = cancel.clone();
        state.pending = Some(Job {
            generation,
            options,
            cancel,
        });
        self.shared.wake.notify_one();
        generation
    }

    /// Immediately freezes the latest published data as Cancelled; stale generations are ignored.
    pub fn cancel(&self, generation: Generation) -> bool {
        let mut state = self.shared.state.lock().expect("index state poisoned");
        if state.snapshot.generation != generation || state.snapshot.coverage != Coverage::Indexing
        {
            return false;
        }
        state.cancel.store(true, AtomicOrdering::Relaxed);
        state.pending = None;
        state.snapshot = Arc::new(IndexSnapshot {
            coverage: Coverage::Cancelled,
            ..(*state.snapshot).clone()
        });
        true
    }

    /// Cheap Arc clone; never performs filesystem I/O or catalog traversal.
    pub fn snapshot(&self) -> Arc<IndexSnapshot> {
        self.shared
            .state
            .lock()
            .expect("index state poisoned")
            .snapshot
            .clone()
    }
}
impl Drop for SearchIndex {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("index state poisoned");
        state.shutdown = true;
        state.pending = None;
        state.cancel.store(true, AtomicOrdering::Relaxed);
        retire_snapshot(&mut state, Arc::new(IndexSnapshot::default()));
        self.shared.wake.notify_one();
        // Do not join: an OS/network filesystem call may be uninterruptible.
    }
}

fn worker_loop(shared: Arc<Shared>) {
    worker_loop_with(shared, scan);
}

// Keep job execution separate from mailbox ownership so lifecycle tests can
// synchronize an actual worker before/after scan, without changing the public API.
fn worker_loop_with(shared: Arc<Shared>, mut run_scan: impl FnMut(&Shared, Job)) {
    loop {
        let (retired, job, shutdown) = {
            let mut state = shared.state.lock().expect("index state poisoned");
            while state.pending.is_none() && state.retired.is_none() && !state.shutdown {
                state = shared.wake.wait(state).expect("index state poisoned");
            }
            (state.retired.take(), state.pending.take(), state.shutdown)
        };
        // Catalog destruction may be O(entries), always outside the mutex and on this worker.
        drop(retired);
        if shutdown {
            return;
        }
        if let Some(job) = job {
            run_scan(&shared, job);
        }
    }
}

fn retire_snapshot(state: &mut State, replacement: Arc<IndexSnapshot>) {
    let previous = std::mem::replace(&mut state.snapshot, replacement);
    if previous.head.is_some() {
        // A scan can publish a new catalog only after the worker drains this slot.
        // Repeated requests replace empty snapshots and do not grow retirement storage.
        assert!(
            state.retired.is_none(),
            "retirement slot must be drained before publication"
        );
        state.retired = Some(previous);
    }
}

fn resolve_exclusion(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    // Resolving symlink/.. through a nonexistent component has no stable native meaning.
    // Reject rather than lexically collapsing it to a different subtree.
    if path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nonexistent exclusion containing '..' is ambiguous; supply a resolved path",
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for name in suffix.into_iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(ancestor).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "exclusion contains an unresolved existing symlink",
                    ));
                }
                suffix.push(ancestor.file_name().ok_or(error)?.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "exclusion has no resolvable ancestor",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

struct Scan<'a> {
    shared: &'a Shared,
    job: &'a Job,
    snapshot: IndexSnapshot,
    batch: Vec<IndexEntry>,
    errors: Vec<Diagnostic>,
    last_publish: Instant,
}
impl Scan<'_> {
    fn cancelled(&self) -> bool {
        self.job.cancel.load(AtomicOrdering::Relaxed)
    }
    fn error(&mut self, path: &Path, operation: &'static str, error: io::Error) {
        self.snapshot.error_count += 1;
        if self.errors.len() < 64 {
            self.errors.push(Diagnostic {
                path: path.into(),
                operation,
                kind: error.kind(),
                message: error.to_string(),
            });
        }
    }
    fn flush(&mut self) {
        if !self.batch.is_empty() {
            self.snapshot.head = Some(Arc::new(Chunk {
                entries: std::mem::take(&mut self.batch),
                previous: self.snapshot.head.take(),
            }));
        }
    }
    fn publish(&mut self, terminal: bool) {
        if !terminal && self.last_publish.elapsed() < self.job.options.publish_interval {
            return;
        }
        self.flush();
        self.snapshot.diagnostics = self.errors.clone().into();
        let mut state = self.shared.state.lock().expect("index state poisoned");
        if state.snapshot.generation == self.job.generation && !self.cancelled() {
            state.snapshot = Arc::new(self.snapshot.clone());
        }
        self.last_publish = Instant::now();
    }
    fn excluded(&self, path: &Path) -> bool {
        self.snapshot
            .excluded_roots
            .iter()
            .any(|excluded| path.starts_with(excluded))
    }
    fn add(&mut self, path: PathBuf, kind: EntryKind) {
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let folded_name = default_case_fold_str(&name);
        let folded_path = default_case_fold_str(&path.to_string_lossy());
        self.batch.push(IndexEntry {
            path,
            kind,
            display_name: name,
            folded_name,
            folded_path,
        });
        self.snapshot.indexed_entries += 1;
        if self.batch.len() >= 1024 {
            self.flush();
        }
        self.publish(false);
    }
}

fn scan(shared: &Shared, job: Job) {
    let mut scan = Scan {
        shared,
        job: &job,
        snapshot: IndexSnapshot {
            generation: job.generation,
            coverage: Coverage::Indexing,
            requested_roots: job.options.roots.clone().into(),
            ..IndexSnapshot::default()
        },
        batch: Vec::new(),
        errors: Vec::new(),
        last_publish: Instant::now(),
    };
    if job.options.roots.is_empty() {
        scan.error(
            Path::new(""),
            "roots",
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one explicit directory root is required",
            ),
        );
    }
    // Canonicalization preserves native bytes and happens only on the worker.
    let mut excluded = Vec::new();
    for path in &job.options.excluded_roots {
        if scan.cancelled() {
            return;
        }
        match resolve_exclusion(path) {
            Ok(path) => excluded.push(path),
            Err(error) => scan.error(path, "exclude", error),
        }
    }
    scan.snapshot.excluded_roots = excluded.into();
    let mut roots = Vec::new();
    for path in &job.options.roots {
        if scan.cancelled() {
            return;
        }
        // POSIX resolves a trailing separator or '/.' before lstat, disguising a
        // final symlink as its target directory. Components removes only redundant
        // separators/dots; ParentDir is retained so symlink/.. native semantics survive.
        let checked_root: PathBuf = path.components().collect();
        match fs::symlink_metadata(&checked_root) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                match fs::canonicalize(path) {
                    Ok(path) => roots.push(path),
                    Err(error) => scan.error(path, "root canonicalize", error),
                }
            }
            Ok(_) => scan.error(
                path,
                "root",
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "root must be a directory, not a file or symlink",
                ),
            ),
            Err(error) => scan.error(path, "root", error),
        }
    }
    roots.sort();
    roots.dedup();
    let mut effective = Vec::<PathBuf>::new();
    for root in roots {
        if !effective.iter().any(|parent| root.starts_with(parent)) {
            effective.push(root);
        }
    }
    scan.snapshot.effective_roots = effective.clone().into();
    let mut pending = effective;
    while let Some(directory) = pending.pop() {
        if scan.cancelled() {
            return;
        }
        if scan.excluded(&directory) {
            scan.snapshot.excluded_subtrees += 1;
            continue;
        }
        // Recheck queued directories so a replaced symlink is not intentionally followed.
        match fs::symlink_metadata(&directory) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => (),
            Ok(_) => {
                scan.error(
                    &directory,
                    "directory changed",
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory replaced while indexing",
                    ),
                );
                continue;
            }
            Err(error) => {
                scan.error(&directory, "directory metadata", error);
                continue;
            }
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                scan.error(&directory, "read directory", error);
                continue;
            }
        };
        scan.snapshot.directories_scanned += 1;
        // At most one ReadDir is live; child paths are queued rather than recursive handles.
        for entry in entries {
            if scan.cancelled() {
                return;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan.error(&directory, "read entry", error);
                    continue;
                }
            };
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => EntryKind::Symlink,
                Ok(kind) if kind.is_dir() => EntryKind::Directory,
                Ok(kind) if kind.is_file() => EntryKind::File,
                Ok(_) => EntryKind::Other,
                Err(error) => {
                    scan.error(&path, "entry type", error);
                    continue;
                }
            };
            if kind == EntryKind::Directory {
                pending.push(path.clone());
            }
            if kind == EntryKind::Symlink {
                scan.snapshot.symlinks_not_followed += 1;
            }
            scan.add(path, kind);
        }
        scan.publish(false);
    }
    scan.snapshot.coverage = if scan.snapshot.error_count == 0 {
        Coverage::Complete
    } else {
        Coverage::Partial
    };
    scan.publish(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn paused_index() -> SearchIndex {
        SearchIndex {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    snapshot: Arc::new(IndexSnapshot::default()),
                    retired: None,
                    pending: None,
                    cancel: Arc::new(AtomicBool::new(false)),
                    shutdown: false,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    #[test]
    fn cancellation_and_stale_publication_are_deterministic_without_worker_scheduling() {
        let index = paused_index();
        let first = index.start(IndexOptions::new(vec![PathBuf::from("owned-fixture")]));
        let old_job = index.shared.state.lock().unwrap().pending.take().unwrap();
        let second = index.start(IndexOptions::new(vec![PathBuf::from("new-owned-fixture")]));
        assert!(old_job.cancel.load(AtomicOrdering::Relaxed));
        // Simulate a superseded worker reaching publication after start replaced the generation.
        let mut old_scan = Scan {
            shared: &index.shared,
            job: &old_job,
            snapshot: IndexSnapshot {
                generation: first,
                coverage: Coverage::Complete,
                ..IndexSnapshot::default()
            },
            batch: Vec::new(),
            errors: Vec::new(),
            last_publish: Instant::now(),
        };
        old_scan.publish(true);
        assert_eq!(index.snapshot().generation, second);
        assert_eq!(index.snapshot().coverage, Coverage::Indexing);
        let second_job = index.shared.state.lock().unwrap().pending.take().unwrap();
        assert!(index.cancel(second));
        let mut second_scan = Scan {
            shared: &index.shared,
            job: &second_job,
            snapshot: IndexSnapshot {
                generation: second,
                coverage: Coverage::Complete,
                ..IndexSnapshot::default()
            },
            batch: Vec::new(),
            errors: Vec::new(),
            last_publish: Instant::now(),
        };
        second_scan.publish(true);
        assert_eq!(index.snapshot().coverage, Coverage::Cancelled);
        assert!(!index.cancel(first));
    }

    #[test]
    fn in_flight_generations_and_cancellation_use_worker_handshakes() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let fixture = Fixture(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures-generated")
                .join(format!("generation-handshake-{}", std::process::id())),
        );
        fs::create_dir_all(fixture.0.join("old")).unwrap();
        fs::create_dir_all(fixture.0.join("new")).unwrap();
        fs::write(fixture.0.join("old/obsolete"), b"old").unwrap();
        fs::write(fixture.0.join("new/current"), b"new").unwrap();
        let old_options = || IndexOptions::new(vec![fixture.0.join("old")]);
        let new_options = || IndexOptions::new(vec![fixture.0.join("new")]);
        let index = paused_index();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let shared = index.shared.clone();
        let worker = thread::spawn(move || {
            worker_loop_with(shared, |shared, job| {
                let generation = job.generation;
                started_tx.send(generation).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("test must release the paused scan");
                scan(shared, job);
                finished_tx.send(generation).unwrap();
            })
        });
        let await_started = |generation| {
            assert_eq!(
                started_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                generation
            )
        };
        let await_finished = |generation| {
            assert_eq!(
                finished_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                generation
            )
        };

        let first = index.start(old_options());
        await_started(first);
        let retained = index.snapshot();
        assert_eq!(retained.generation, first);
        assert_eq!(retained.coverage, Coverage::Indexing);
        let second = index.start(new_options());
        assert!(second.0 > first.0);
        assert!(!index.cancel(first));
        release_tx.send(()).unwrap();
        await_finished(first);
        await_started(second);
        assert_eq!(index.snapshot().generation, second);
        assert_eq!(index.snapshot().coverage, Coverage::Indexing);
        release_tx.send(()).unwrap();
        await_finished(second);
        let second_snapshot = index.snapshot();
        assert_eq!(second_snapshot.generation, second);
        assert_eq!(second_snapshot.coverage, Coverage::Complete);
        assert_eq!(
            second_snapshot
                .query(&Query {
                    text: "obsolete".into(),
                    limit: 10,
                    ..Query::default()
                })
                .total_matches,
            0
        );
        assert_eq!(
            second_snapshot
                .query(&Query {
                    text: "current".into(),
                    limit: 10,
                    ..Query::default()
                })
                .total_matches,
            1
        );
        assert_eq!(retained.generation, first);
        assert_eq!(retained.coverage, Coverage::Indexing);
        assert_eq!(retained.indexed_entries, 0);

        let third = index.start(old_options());
        await_started(third);
        assert!(
            index.cancel(third),
            "worker is paused with an in-flight Indexing generation"
        );
        assert_eq!(index.snapshot().coverage, Coverage::Cancelled);
        release_tx.send(()).unwrap();
        await_finished(third);
        assert_eq!(index.snapshot().generation, third);
        assert_eq!(index.snapshot().coverage, Coverage::Cancelled);
        assert!(!index.cancel(third));

        let fourth = index.start(new_options());
        await_started(fourth);
        release_tx.send(()).unwrap();
        await_finished(fourth);
        let final_snapshot = index.snapshot();
        assert_eq!(final_snapshot.generation, fourth);
        assert_eq!(final_snapshot.coverage, Coverage::Complete);
        assert_eq!(
            final_snapshot
                .query(&Query {
                    text: "current".into(),
                    limit: 10,
                    ..Query::default()
                })
                .total_matches,
            1
        );
        assert_eq!(
            final_snapshot
                .query(&Query {
                    text: "obsolete".into(),
                    limit: 10,
                    ..Query::default()
                })
                .total_matches,
            0
        );
        drop(index);
        worker.join().unwrap();
    }

    #[test]
    fn cancelled_empty_queries_and_sorting_return_no_page() {
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            IndexSnapshot::default()
                .query_cancellable(&Query::default(), &cancelled)
                .unwrap_err(),
            QueryCancelled
        );
        let mut values = vec![3, 1, 2];
        assert_eq!(
            cancellable_sort(&mut values, Ord::cmp, &cancelled),
            Err(QueryCancelled)
        );
        let flag = AtomicBool::new(false);
        let compare = |a: &i32, b: &i32| {
            flag.store(true, AtomicOrdering::Relaxed);
            a.cmp(b)
        };
        assert_eq!(
            cancellable_sort(&mut values, compare, &flag),
            Err(QueryCancelled)
        );
    }

    #[test]
    fn retirement_is_a_single_slot_across_rapid_replacements_and_drains_on_worker() {
        let index = paused_index();
        let head = Arc::new(Chunk {
            entries: Vec::new(),
            previous: None,
        });
        let weak = Arc::downgrade(&head);
        index.shared.state.lock().unwrap().snapshot = Arc::new(IndexSnapshot {
            head: Some(head),
            coverage: Coverage::Complete,
            ..IndexSnapshot::default()
        });
        for _ in 0..100 {
            index.start(IndexOptions::new(Vec::new()));
        }
        {
            let state = index.shared.state.lock().unwrap();
            assert!(state.retired.is_some());
            assert!(state.snapshot.head.is_none());
            assert_eq!(state.pending.as_ref().unwrap().generation, Generation(100));
            assert!(
                weak.upgrade().is_some(),
                "caller must not destroy the retired catalog"
            );
        }
        let shared = index.shared.clone();
        let worker = thread::spawn(move || worker_loop(shared));
        let deadline = Instant::now() + Duration::from_secs(5);
        while weak.upgrade().is_some() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        drop(index);
        worker.join().unwrap();
    }

    #[test]
    fn unicode_tables_cover_modern_case_pairs_without_normalization() {
        assert_eq!(CASE_FOLD_UNICODE_VERSION, (16, 0, 0));
        // Georgian Mtavruli (Unicode 11), Vithkuqi (14), Cyrillic Tje (16).
        for (upper, lower) in [
            ("\u{1c90}", "\u{10d0}"),
            ("\u{10570}", "\u{10597}"),
            ("\u{1c89}", "\u{1c8a}"),
        ] {
            assert_eq!(default_case_fold_str(upper), default_case_fold_str(lower));
        }
        assert_ne!(
            default_case_fold_str("é"),
            default_case_fold_str("e\u{301}")
        );
    }
}
