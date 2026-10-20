//! Immutable directory snapshots and checked lazy selection handoffs.
//!
//! Snapshot/ticket internals are opaque. The browser worker retains a strong
//! lease until all external handles disappear; their last external Drop only
//! decrements an Arc and unparks that worker. Keeping handles intentionally
//! retains catalogs and keeps a closed browser's worker alive. Catalog memory
//! is not globally bounded. Pending registration/materialization slots are.

use crate::selection::{ListingRevision, Selection};
use crate::{Entry, Shared};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Weak};

pub(crate) struct ListingData {
    pub(crate) revision: ListingRevision,
    pub(crate) entries: Vec<Entry>,
    #[cfg(test)]
    drop_thread: std::sync::Mutex<Option<mpsc::Sender<std::thread::ThreadId>>>,
}
impl ListingData {
    pub(crate) fn new(entries: Vec<Entry>) -> Self {
        Self {
            revision: ListingRevision::new(),
            entries,
            #[cfg(test)]
            drop_thread: std::sync::Mutex::new(None),
        }
    }
}
#[cfg(test)]
impl Drop for ListingData {
    fn drop(&mut self) {
        if let Some(sender) = self.drop_thread.get_mut().unwrap().take() {
            let _ = sender.send(std::thread::current().id());
        }
    }
}

/// Cheap clone/handoff for exactly one accepted ordering and membership.
/// No raw Arc is exposed: a worker lease protects its catalog's destruction.
#[derive(Clone)]
pub struct ListingSnapshot {
    pub(crate) data: Option<Arc<ListingData>>,
    pub(crate) owner: Weak<Shared>,
}
impl fmt::Debug for ListingSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListingSnapshot")
            .field("revision", self.revision())
            .field("len", &self.len())
            .finish()
    }
}
impl ListingSnapshot {
    fn data(&self) -> &ListingData {
        self.data.as_deref().expect("live listing handle")
    }
    pub fn revision(&self) -> &ListingRevision {
        &self.data().revision
    }
    pub fn entries(&self) -> &[Entry] {
        &self.data().entries
    }
    pub fn len(&self) -> usize {
        self.data().entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data().entries.is_empty()
    }

    /// Copies only checked selection runs, never paths or one index per row.
    /// Cost is O(r) in run count (Select All has one run), capped before copying.
    /// The one pending worker-registration slot can return Busy; retry later.
    pub fn ticket(
        &self,
        selection: &Selection,
        limits: TicketLimits,
    ) -> Result<SelectionTicket, TicketError> {
        let ranges = selection
            .ranges(self.revision())
            .map_err(|_| TicketError::StaleRevision)?;
        if selection.len() != self.len() {
            return Err(TicketError::InvalidRanges);
        }
        check_ranges(ranges, self.len(), selection.count(), limits)?;
        let shared = self.owner.upgrade().ok_or(TicketError::WorkerClosed)?;
        if shared.stop.load(Ordering::Acquire) {
            return Err(TicketError::WorkerClosed);
        }
        let mut pending = shared
            .ticket_registration
            .lock()
            .expect("ticket registration mutex poisoned");
        if shared.stop.load(Ordering::Acquire) {
            return Err(TicketError::WorkerClosed);
        }
        if pending.is_some() {
            return Err(TicketError::Busy);
        }
        let data = Arc::new(TicketData {
            snapshot: self.clone(),
            revision: self.revision().clone(),
            ranges: ranges.to_vec(),
            count: selection.count(),
            #[cfg(test)]
            drop_thread: std::sync::Mutex::new(None),
        });
        // Register the worker lease before exposing a handle to the caller.
        *pending = Some(Arc::clone(&data));
        drop(pending);
        shared.wake();
        Ok(SelectionTicket {
            data: Some(data),
            owner: Arc::downgrade(&shared),
        })
    }
}
impl Drop for ListingSnapshot {
    fn drop(&mut self) {
        // Release the external reference BEFORE notifying; the worker lease
        // guarantees this decrement cannot destroy the catalog.
        drop(self.data.take());
        if let Some(shared) = self.owner.upgrade() {
            shared.wake();
        }
    }
}

/// Absolute admission caps bound UI-side run copying and result buffers.
pub const MAX_TICKET_RANGES: usize = 4096;
pub const MAX_TICKET_RANGE_BYTES: usize = MAX_TICKET_RANGES * 16;
pub const MAX_MATERIALIZED_ITEMS: usize = 4096;
pub const MAX_MATERIALIZED_PATH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct TicketLimits {
    max_ranges: usize,
    max_range_bytes: usize,
}
impl TicketLimits {
    pub fn new(max_ranges: usize, max_range_bytes: usize) -> Result<Self, TicketError> {
        if max_ranges > MAX_TICKET_RANGES || max_range_bytes > MAX_TICKET_RANGE_BYTES {
            return Err(TicketError::InvalidLimits);
        }
        Ok(Self {
            max_ranges,
            max_range_bytes,
        })
    }
}
impl Default for TicketLimits {
    fn default() -> Self {
        Self {
            max_ranges: MAX_TICKET_RANGES,
            max_range_bytes: MAX_TICKET_RANGE_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MaterializeLimits {
    max_items: usize,
    max_native_path_bytes: usize,
}
impl MaterializeLimits {
    pub fn new(max_items: usize, max_native_path_bytes: usize) -> Result<Self, TicketError> {
        if max_items > MAX_MATERIALIZED_ITEMS || max_native_path_bytes > MAX_MATERIALIZED_PATH_BYTES
        {
            return Err(TicketError::InvalidLimits);
        }
        Ok(Self {
            max_items,
            max_native_path_bytes,
        })
    }
}
impl Default for MaterializeLimits {
    fn default() -> Self {
        Self {
            max_items: MAX_MATERIALIZED_ITEMS,
            max_native_path_bytes: MAX_MATERIALIZED_PATH_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TicketError {
    StaleRevision,
    InvalidRanges,
    InvalidLimits,
    TooManyRanges,
    TooManyItems,
    RangeBytesExceeded,
    PathBytesExceeded,
    Cancelled,
    Busy,
    WorkerClosed,
}
impl fmt::Display for TicketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StaleRevision => "selection/action belongs to a different listing revision",
            Self::InvalidRanges => "selection ranges do not match the ordered listing",
            Self::InvalidLimits => "requested limits exceed the supported admission caps",
            Self::TooManyRanges => "selection exceeds the run-count limit",
            Self::TooManyItems => "selection exceeds the materialized item limit",
            Self::RangeBytesExceeded => "selection exceeds the canonical run-byte limit",
            Self::PathBytesExceeded => "selection exceeds the native path-byte limit",
            Self::Cancelled => "selection materialization was cancelled",
            Self::Busy => "the worker's pending handoff slot is occupied; retry later",
            Self::WorkerClosed => "the browser worker is closed",
        })
    }
}
impl std::error::Error for TicketError {}

pub(crate) struct TicketData {
    snapshot: ListingSnapshot,
    revision: ListingRevision,
    ranges: Vec<Range<usize>>,
    count: usize,
    #[cfg(test)]
    drop_thread: std::sync::Mutex<Option<mpsc::Sender<std::thread::ThreadId>>>,
}
#[cfg(test)]
impl Drop for TicketData {
    fn drop(&mut self) {
        if let Some(sender) = self.drop_thread.get_mut().unwrap().take() {
            let _ = sender.send(std::thread::current().id());
        }
    }
}

/// Opaque checked runs tied to their immutable snapshot, not the current path.
/// A later navigation does not retarget this ticket to a new ordered listing.
#[derive(Clone)]
pub struct SelectionTicket {
    pub(crate) data: Option<Arc<TicketData>>,
    owner: Weak<Shared>,
}
impl fmt::Debug for SelectionTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectionTicket")
            .field("revision", self.revision())
            .field("count", &self.count())
            .field("range_count", &self.range_count())
            .finish()
    }
}
impl SelectionTicket {
    fn data(&self) -> &TicketData {
        self.data.as_deref().expect("live ticket handle")
    }
    pub fn revision(&self) -> &ListingRevision {
        &self.data().revision
    }
    pub fn count(&self) -> usize {
        self.data().count
    }
    pub fn range_count(&self) -> usize {
        self.data().ranges.len()
    }

    /// Schedule native-path materialization on the browser worker, with no FS
    /// calls. Caller must provide the revision expected by its action request.
    /// Only one pending job is admitted; Busy never silently replaces an action.
    pub fn materialize(
        &self,
        expected: &ListingRevision,
        limits: MaterializeLimits,
    ) -> Result<Materialization, TicketError> {
        if expected != self.revision() {
            return Err(TicketError::StaleRevision);
        }
        if self.count() > limits.max_items {
            return Err(TicketError::TooManyItems);
        }
        let shared = self.owner.upgrade().ok_or(TicketError::WorkerClosed)?;
        if shared.stop.load(Ordering::Acquire) {
            return Err(TicketError::WorkerClosed);
        }
        let mut pending = shared
            .materialize
            .lock()
            .expect("materialization mutex poisoned");
        if shared.stop.load(Ordering::Acquire) {
            return Err(TicketError::WorkerClosed);
        }
        if pending.is_some() {
            return Err(TicketError::Busy);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        *pending = Some(MaterializeJob {
            ticket: self.clone(),
            expected: expected.clone(),
            limits,
            cancel: Arc::clone(&cancel),
            sender,
        });
        drop(pending);
        shared.wake();
        Ok(Materialization {
            receiver,
            cancel,
            owner: self.owner.clone(),
            finished: false,
        })
    }
}
impl Drop for SelectionTicket {
    fn drop(&mut self) {
        drop(self.data.take());
        if let Some(shared) = self.owner.upgrade() {
            shared.wake();
        }
    }
}
/// Pollable single response. Dropping it requests cancellation and wakes the
/// worker. A received path buffer is caller-owned and bounded, NOT O(1) to drop.
pub struct Materialization {
    receiver: mpsc::Receiver<Result<MaterializedPaths, TicketError>>,
    cancel: Arc<AtomicBool>,
    owner: Weak<Shared>,
    finished: bool,
}
impl Materialization {
    pub fn poll(&mut self) -> Option<Result<MaterializedPaths, TicketError>> {
        if self.finished {
            return None;
        }
        match self.receiver.try_recv() {
            Ok(result) => {
                self.finished = true;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Err(TicketError::WorkerClosed))
            }
        }
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(shared) = self.owner.upgrade() {
            shared.wake();
        }
    }
}
impl Drop for Materialization {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Bounded caller-owned output. Limits count native storage bytes (Unix bytes,
/// Windows UTF-16 bytes), not lossy display bytes or URI/clipboard encoding size.
/// This is an identity handoff, not a filesystem existence/freshness guarantee.
#[derive(Debug)]
pub struct MaterializedPaths {
    revision: ListingRevision,
    paths: Vec<PathBuf>,
    native_path_bytes: usize,
}
impl MaterializedPaths {
    pub fn revision(&self) -> &ListingRevision {
        &self.revision
    }
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
    pub fn native_path_bytes(&self) -> usize {
        self.native_path_bytes
    }
    pub fn into_paths(self) -> Vec<PathBuf> {
        self.paths
    }
}

pub(crate) struct MaterializeJob {
    ticket: SelectionTicket,
    expected: ListingRevision,
    limits: MaterializeLimits,
    cancel: Arc<AtomicBool>,
    sender: mpsc::SyncSender<Result<MaterializedPaths, TicketError>>,
}
impl MaterializeJob {
    pub(crate) fn execute(self, shared: &Shared) {
        let result = self.run(shared);
        // The final check is immediately before publication; cancellation can
        // still race after send, so consumers must validate their action token.
        let result = if self.cancelled(shared) {
            Err(TicketError::Cancelled)
        } else {
            result
        };
        let _ = self.sender.send(result);
    }
    pub(crate) fn close(self) {
        let _ = self.sender.send(Err(TicketError::WorkerClosed));
    }
    fn cancelled(&self, shared: &Shared) -> bool {
        self.cancel.load(Ordering::Acquire) || shared.stop.load(Ordering::Acquire)
    }
    fn run(&self, shared: &Shared) -> Result<MaterializedPaths, TicketError> {
        let data = self.ticket.data();
        if self.expected != data.revision || data.revision != *data.snapshot.revision() {
            return Err(TicketError::StaleRevision);
        }
        check_ranges(
            &data.ranges,
            data.snapshot.len(),
            data.count,
            TicketLimits::default(),
        )?;
        if data.count > self.limits.max_items {
            return Err(TicketError::TooManyItems);
        }
        let mut native_path_bytes = 0usize;
        // Preflight all lengths before cloning any native paths. This also
        // checks cancellation per selected item, bounded by max_items.
        for range in &data.ranges {
            for entry in &data.snapshot.entries()[range.clone()] {
                if self.cancelled(shared) {
                    return Err(TicketError::Cancelled);
                }
                native_path_bytes = native_path_bytes
                    .checked_add(native_bytes(&entry.path))
                    .ok_or(TicketError::PathBytesExceeded)?;
                if native_path_bytes > self.limits.max_native_path_bytes {
                    return Err(TicketError::PathBytesExceeded);
                }
            }
        }
        let mut paths = Vec::with_capacity(data.count);
        for range in &data.ranges {
            for entry in &data.snapshot.entries()[range.clone()] {
                if self.cancelled(shared) {
                    return Err(TicketError::Cancelled);
                }
                paths.push(entry.path.clone());
            }
        }
        if self.expected != *data.snapshot.revision() {
            return Err(TicketError::StaleRevision);
        }
        Ok(MaterializedPaths {
            revision: self.expected.clone(),
            paths,
            native_path_bytes,
        })
    }
}

fn check_ranges(
    ranges: &[Range<usize>],
    len: usize,
    expected_count: usize,
    limits: TicketLimits,
) -> Result<(), TicketError> {
    if ranges.len() > limits.max_ranges {
        return Err(TicketError::TooManyRanges);
    }
    // A canonical serialized run is two u64 values (16 bytes), independent of
    // this architecture's Vec/Range allocation overhead, which is not claimed.
    let bytes = ranges
        .len()
        .checked_mul(16)
        .ok_or(TicketError::RangeBytesExceeded)?;
    if bytes > limits.max_range_bytes {
        return Err(TicketError::RangeBytesExceeded);
    }
    let mut count = 0usize;
    let mut previous_end = None;
    for range in ranges {
        if range.start >= range.end
            || range.end > len
            || previous_end.is_some_and(|end| range.start <= end)
        {
            return Err(TicketError::InvalidRanges);
        }
        count = count
            .checked_add(range.end - range.start)
            .ok_or(TicketError::InvalidRanges)?;
        previous_end = Some(range.end);
    }
    if count != expected_count {
        return Err(TicketError::InvalidRanges);
    }
    Ok(())
}

#[cfg(windows)]
fn native_bytes(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count().saturating_mul(2)
}
#[cfg(not(windows))]
fn native_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{Action, Modifiers};
    use crate::{Browser, EntryKind, LoadState};
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    // Publish an actual immutable catalog from a worker without creating a
    // million filesystem objects. This tests handoff correctness, not memory,
    // I/O throughput or wall-clock latency.
    fn seeded(entries: Vec<Entry>) -> Browser {
        let shared = Arc::new(Shared {
            generation: std::sync::atomic::AtomicU64::new(0),
            stop: AtomicBool::new(false),
            request: Mutex::new(None),
            result: Mutex::new(None),
            thread: OnceLock::new(),
            ticket_registration: Mutex::new(None),
            materialize: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            let data = Arc::new(ListingData::new(entries));
            *worker_shared.result.lock().unwrap() = Some(crate::Delivery {
                generation: 0,
                result: Ok(ListingSnapshot {
                    data: Some(Arc::clone(&data)),
                    owner: Arc::downgrade(&worker_shared),
                }),
            });
            crate::worker_loop_leased(worker_shared, vec![data]);
        });
        let mut browser = Browser {
            path: PathBuf::from("synthetic"),
            snapshot: None,
            filter: String::new(),
            sort: crate::SortOrder::default(),
            show_hidden: true,
            state: LoadState::Loading,
            history: vec![PathBuf::from("synthetic")],
            history_index: 0,
            location_revision: 0,
            shared,
            worker: Some(worker),
        };
        wait_until(|| browser.poll());
        browser
    }
    fn row(path: impl Into<PathBuf>) -> Entry {
        let path = path.into();
        Entry {
            name: path.file_name().unwrap_or_default().to_os_string(),
            path,
            kind: EntryKind::File,
            size: None,
            modified: None,
        }
    }
    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < until, "worker handshake timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn all(snapshot: &ListingSnapshot) -> Selection {
        let mut selection = Selection::new(snapshot.revision().clone(), snapshot.len());
        selection
            .apply(snapshot.revision(), Action::SelectAll)
            .unwrap();
        selection
    }
    fn ticket(snapshot: &ListingSnapshot, selection: &Selection) -> SelectionTicket {
        let mut output = None;
        wait_until(
            || match snapshot.ticket(selection, TicketLimits::default()) {
                Ok(ticket) => {
                    output = Some(ticket);
                    true
                }
                Err(TicketError::Busy) => false,
                Err(error) => panic!("ticket admission: {error}"),
            },
        );
        output.unwrap()
    }
    fn output(
        ticket: &SelectionTicket,
        limits: MaterializeLimits,
    ) -> Result<MaterializedPaths, TicketError> {
        let mut job = ticket.materialize(ticket.revision(), limits)?;
        let mut result = None;
        wait_until(|| {
            result = job.poll();
            result.is_some()
        });
        result.unwrap()
    }

    #[test]
    fn million_select_all_handoff_keeps_one_run_and_refuses_materialization_cap() {
        let entries: Vec<_> = (0..1_000_000).map(|_| row("synthetic-file")).collect();
        let browser = seeded(entries);
        let snapshot = browser.snapshot().unwrap();
        assert_eq!(snapshot.len(), 1_000_000);
        let selection = all(&snapshot);
        let ticket = ticket(&snapshot, &selection);
        assert_eq!(ticket.count(), 1_000_000);
        assert_eq!(ticket.range_count(), 1);
        assert!(matches!(
            ticket.materialize(snapshot.revision(), MaterializeLimits::default()),
            Err(TicketError::TooManyItems)
        ));
        // Lazy Select All is valid; a too-large clipboard action is explicitly
        // refused rather than silently clipping it to the first 4,096 files.
    }

    #[test]
    fn same_length_reordered_catalog_rejects_old_indices_and_expected_revision() {
        let old_browser = seeded(vec![row("a"), row("b")]);
        let new_browser = seeded(vec![row("b"), row("a")]);
        let old = old_browser.snapshot().unwrap();
        let new = new_browser.snapshot().unwrap();
        let selected = all(&old);
        assert!(matches!(
            new.ticket(&selected, TicketLimits::default()),
            Err(TicketError::StaleRevision)
        ));
        let old_ticket = ticket(&old, &selected);
        assert!(matches!(
            old_ticket.materialize(new.revision(), MaterializeLimits::default()),
            Err(TicketError::StaleRevision)
        ));
        assert_eq!(
            output(&old_ticket, MaterializeLimits::default())
                .unwrap()
                .paths(),
            [PathBuf::from("a"), PathBuf::from("b")]
        );
    }

    #[test]
    fn range_admission_and_order_are_checked_before_copy_or_publication() {
        let browser = seeded((0..10).map(|index| row(index.to_string())).collect());
        let snapshot = browser.snapshot().unwrap();
        let mut selection = Selection::new(snapshot.revision().clone(), snapshot.len());
        for index in [0, 2, 4] {
            selection
                .apply(
                    snapshot.revision(),
                    Action::Click {
                        index,
                        modifiers: Modifiers {
                            control: true,
                            shift: false,
                        },
                    },
                )
                .unwrap();
        }
        assert!(matches!(
            snapshot.ticket(&selection, TicketLimits::new(2, 48).unwrap()),
            Err(TicketError::TooManyRanges)
        ));
        assert!(matches!(
            snapshot.ticket(&selection, TicketLimits::new(3, 47).unwrap()),
            Err(TicketError::RangeBytesExceeded)
        ));
        assert!(matches!(
            TicketLimits::new(MAX_TICKET_RANGES + 1, 0),
            Err(TicketError::InvalidLimits)
        ));
        assert!(matches!(
            MaterializeLimits::new(MAX_MATERIALIZED_ITEMS + 1, 0),
            Err(TicketError::InvalidLimits)
        ));
        assert_eq!(
            check_ranges(&[2..4, 1..2], 4, 3, TicketLimits::default()),
            Err(TicketError::InvalidRanges)
        );
        assert_eq!(
            check_ranges(&[0..2, 2..4], 4, 4, TicketLimits::default()),
            Err(TicketError::InvalidRanges)
        );
        assert_eq!(
            check_ranges(
                std::slice::from_ref(&(0..usize::MAX)),
                3,
                usize::MAX,
                TicketLimits::default()
            ),
            Err(TicketError::InvalidRanges)
        );
        let ticket = ticket(&snapshot, &selection);
        assert_eq!(
            output(&ticket, MaterializeLimits::default())
                .unwrap()
                .paths(),
            [PathBuf::from("0"), PathBuf::from("2"), PathBuf::from("4")]
        );
    }

    #[test]
    fn native_path_byte_limit_is_exact_and_no_filesystem_lookup_occurs() {
        let browser = seeded(vec![
            row("definitely-missing/a"),
            row("definitely-missing/b"),
        ]);
        let snapshot = browser.snapshot().unwrap();
        let ticket = ticket(&snapshot, &all(&snapshot));
        let bytes: usize = snapshot
            .entries()
            .iter()
            .map(|entry| native_bytes(&entry.path))
            .sum();
        assert!(matches!(
            output(&ticket, MaterializeLimits::new(2, bytes - 1).unwrap()),
            Err(TicketError::PathBytesExceeded)
        ));
        let result = output(&ticket, MaterializeLimits::new(2, bytes).unwrap()).unwrap();
        assert_eq!(result.native_path_bytes(), bytes);
        assert_eq!(result.paths().len(), 2);
        assert_eq!(result.revision(), snapshot.revision());
        assert!(matches!(
            output(&ticket, MaterializeLimits::new(1, bytes).unwrap()),
            Err(TicketError::TooManyItems)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn worker_handoff_preserves_non_utf8_native_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let native = OsString::from_vec(b"missing-\xff-native".to_vec());
        let browser = seeded(vec![row(PathBuf::from(native))]);
        let snapshot = browser.snapshot().unwrap();
        let ticket = ticket(&snapshot, &all(&snapshot));
        let result = output(&ticket, MaterializeLimits::default()).unwrap();
        assert_eq!(
            result.paths()[0].as_os_str().as_bytes(),
            b"missing-\xff-native"
        );
        assert_eq!(result.native_path_bytes(), b"missing-\xff-native".len());
    }

    #[test]
    fn last_external_snapshot_and_ticket_release_reclaims_on_worker_after_browser_close() {
        let browser = seeded(vec![row("a"), row("b")]);
        let snapshot = browser.snapshot().unwrap();
        let ticket = ticket(&snapshot, &all(&snapshot));
        let owner = Arc::downgrade(&browser.shared);
        let (sender, receiver) = mpsc::channel();
        let (ticket_sender, ticket_receiver) = mpsc::channel();
        *ticket.data.as_ref().unwrap().drop_thread.lock().unwrap() = Some(ticket_sender);
        *snapshot.data.as_ref().unwrap().drop_thread.lock().unwrap() = Some(sender);
        let worker_id = browser.worker.as_ref().unwrap().thread().id();
        drop(browser);
        drop(snapshot);
        assert!(
            owner.upgrade().is_some(),
            "ticket must retain its worker lease"
        );
        assert!(matches!(
            ticket.materialize(ticket.revision(), MaterializeLimits::default()),
            Err(TicketError::WorkerClosed)
        ));
        drop(ticket);
        assert_eq!(
            ticket_receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            worker_id
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
            worker_id
        );
        wait_until(|| owner.upgrade().is_none());
    }

    #[test]
    fn registration_mailbox_is_one_slot_and_cancelled_work_never_publishes_paths() {
        // No worker yet: deterministically fill the registration and action slots.
        let shared = Arc::new(Shared {
            generation: std::sync::atomic::AtomicU64::new(0),
            stop: AtomicBool::new(false),
            request: Mutex::new(None),
            result: Mutex::new(None),
            thread: OnceLock::new(),
            ticket_registration: Mutex::new(None),
            materialize: Mutex::new(None),
        });
        let data = Arc::new(ListingData::new(vec![row("a")]));
        let snapshot = ListingSnapshot {
            data: Some(Arc::clone(&data)),
            owner: Arc::downgrade(&shared),
        };
        let selected = all(&snapshot);
        let first = snapshot.ticket(&selected, TicketLimits::default()).unwrap();
        assert!(matches!(
            snapshot.ticket(&selected, TicketLimits::default()),
            Err(TicketError::Busy)
        ));
        let mut pending = first
            .materialize(first.revision(), MaterializeLimits::default())
            .unwrap();
        assert!(matches!(
            first.materialize(first.revision(), MaterializeLimits::default()),
            Err(TicketError::Busy)
        ));
        pending.cancel();
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || crate::worker_loop_leased(worker_shared, vec![data]));
        let mut result = None;
        wait_until(|| {
            result = pending.poll();
            result.is_some()
        });
        assert!(matches!(result.unwrap(), Err(TicketError::Cancelled)));
        drop(first);
        drop(snapshot);
        drop(pending);
        shared.stop.store(true, Ordering::Release);
        shared.wake();
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::selection::Action;
    use crate::{Browser, LoadState};
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::thread;
    use std::time::{Duration, Instant};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn wait(browser: &mut Browser) {
        let until = Instant::now() + Duration::from_secs(10);
        while matches!(browser.state(), LoadState::Loading) {
            browser.poll();
            assert!(Instant::now() < until, "directory worker timeout");
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn accepted_refresh_filter_and_failure_never_recycle_revisions_or_retarget_ticket() {
        let root = Root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            ".test-output-listing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir(&root.0).unwrap();
        fs::write(root.0.join("a"), b"a").unwrap();
        let mut browser = Browser::new(root.0.clone());
        wait(&mut browser);
        let first = browser.snapshot().unwrap();
        let mut selected = Selection::new(first.revision().clone(), first.len());
        selected.apply(first.revision(), Action::SelectAll).unwrap();
        let ticket = first.ticket(&selected, TicketLimits::default()).unwrap();
        fs::remove_file(root.0.join("a")).unwrap();
        fs::write(root.0.join("b"), b"b").unwrap();
        browser.navigate(root.0.clone());
        assert!(browser.snapshot().is_none());
        assert!(browser.entries().is_empty());
        wait(&mut browser);
        let second = browser.snapshot().unwrap();
        assert_eq!(second.len(), first.len());
        assert_ne!(second.revision(), first.revision());
        assert!(matches!(
            second.ticket(&selected, TicketLimits::default()),
            Err(TicketError::StaleRevision)
        ));
        browser.set_filter("b");
        wait(&mut browser);
        let filtered = browser.snapshot().unwrap();
        assert_ne!(filtered.revision(), second.revision());
        assert_eq!(filtered.entries()[0].path, root.0.join("b"));
        for _ in 0..200 {
            browser.set_filter("a");
            browser.set_filter("b");
        }
        wait(&mut browser);
        assert_eq!(browser.entries().len(), 1);
        browser.navigate(root.0.join("missing"));
        wait(&mut browser);
        assert!(matches!(browser.state(), LoadState::Failed(_)));
        assert!(browser.snapshot().is_none());
        // The accepted old ticket deliberately still means 'a', even though
        // that path no longer exists and today's same-length listing is 'b'.
        let mut job = ticket
            .materialize(first.revision(), MaterializeLimits::default())
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = job.poll() {
                assert_eq!(result.unwrap().paths(), [root.0.join("a")]);
                break;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(1));
        }
    }
}
