//! Bounded memory-only tab ordering and directory resume state.
//!
//! The application owns live payloads and global jobs. This module does not
//! inspect a filesystem, start workers, invoke factories, or retire a removed
//! live payload. Close returns that payload; reopen receives a freshly prepared
//! payload and mints a new identity. Returned admission failures preserve both
//! the workspace and the supplied input. Allocator failure follows Rust's normal
//! allocation behavior rather than a recoverable admission guarantee.

use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// Count/byte limits cover model metadata and closed resume state, not live `T`.
/// Byte accounting includes record/path headers and encoded native string bytes;
/// allocator overhead and bounded collection spare capacity are separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_open: usize,
    /// Zero disables remembered closes; use `close_without_history` instead.
    pub max_closed: usize,
    pub history_per_direction: usize,
    pub max_snapshot_bytes: usize,
    pub max_closed_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_open: 32,
            max_closed: 16,
            history_per_direction: 32,
            max_snapshot_bytes: 256 * 1024,
            max_closed_bytes: 2 * 1024 * 1024,
        }
    }
}

impl Limits {
    fn validate(self) -> Result<Self, Error> {
        if self.max_open == 0
            || self.max_open > 256
            || self.max_closed > 64
            || self.history_per_direction > 64
            || self.max_snapshot_bytes == 0
            || self.max_snapshot_bytes > 1024 * 1024
            || self.max_closed_bytes > 16 * 1024 * 1024
            || (self.max_closed > 0 && self.max_snapshot_bytes > self.max_closed_bytes)
        {
            return Err(Error::InvalidLimits);
        }
        Ok(self)
    }
}

/// Opaque process-local identity. Reading its display key does not permit
/// construction, deserialization or rebinding of an identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TabId {
    owner: NonZeroU64,
    sequence: NonZeroU64,
}

/// A retained close record, distinct from the live tab that was removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ClosedTabId {
    owner: NonZeroU64,
    sequence: NonZeroU64,
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tab:{}:{}", self.owner, self.sequence)
    }
}
impl fmt::Display for ClosedTabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "closed-tab:{}:{}", self.owner, self.sequence)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Activation {
    Foreground,
    Background,
}

/// Resume data deliberately cannot own a live browser, catalog, job or ticket.
/// Boxed storage removes caller-provided excess String/Vec/PathBuf capacity.
/// Construction does not validate paths or limits; admission validates lengths
/// without canonicalizing or converting native filenames to display text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabSnapshot {
    location: Box<Path>,
    back: Box<[Box<Path>]>,
    forward: Box<[Box<Path>]>,
    filter: Box<str>,
}

impl TabSnapshot {
    pub fn new(
        location: PathBuf,
        back: Vec<PathBuf>,
        forward: Vec<PathBuf>,
        filter: String,
    ) -> Self {
        Self {
            location: location.into_boxed_path(),
            back: back.into_iter().map(PathBuf::into_boxed_path).collect(),
            forward: forward.into_iter().map(PathBuf::into_boxed_path).collect(),
            filter: filter.into_boxed_str(),
        }
    }
    pub fn location(&self) -> &Path {
        &self.location
    }
    /// Ordering is caller-defined and preserved verbatim on close/reopen.
    pub fn back(&self) -> impl ExactSizeIterator<Item = &Path> + DoubleEndedIterator {
        self.back.iter().map(AsRef::as_ref)
    }
    pub fn forward(&self) -> impl ExactSizeIterator<Item = &Path> + DoubleEndedIterator {
        self.forward.iter().map(AsRef::as_ref)
    }
    pub fn filter(&self) -> &str {
        &self.filter
    }
    fn accounted_bytes(&self) -> Option<usize> {
        let mut bytes = std::mem::size_of::<ClosedTab>().checked_add(self.filter.len())?;
        let history = self.back.len().checked_add(self.forward.len())?;
        bytes = bytes.checked_add(history.checked_mul(std::mem::size_of::<Box<Path>>())?)?;
        for path in std::iter::once(self.location.as_ref())
            .chain(self.back())
            .chain(self.forward())
        {
            bytes = bytes.checked_add(path.as_os_str().as_encoded_bytes().len())?;
        }
        Some(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidLimits,
    OwnerExhausted,
    IdentityExhausted,
    ForeignTab,
    StaleTab,
    ForeignClosedTab,
    StaleClosedTab,
    OpenLimit,
    HistoryDisabled,
    HistoryLimit,
    SnapshotTooLarge,
    PositionOutOfBounds { index: usize, len: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PositionOutOfBounds { index, len } => {
                write!(f, "tab position {index} is outside length {len}")
            }
            other => f.write_str(match other {
                Self::InvalidLimits => "workspace limits are invalid",
                Self::OwnerExhausted => "workspace owner identities are exhausted",
                Self::IdentityExhausted => "workspace tab identities are exhausted",
                Self::ForeignTab => "tab belongs to another workspace",
                Self::StaleTab => "tab is no longer open",
                Self::ForeignClosedTab => "close record belongs to another workspace",
                Self::StaleClosedTab => "close record is no longer retained",
                Self::OpenLimit => "open tab limit reached",
                Self::HistoryDisabled => "recently closed history is disabled",
                Self::HistoryLimit => "resume history exceeds the direction limit",
                Self::SnapshotTooLarge => "resume state exceeds the byte limit",
                Self::PositionOutOfBounds { .. } => unreachable!(),
            }),
        }
    }
}
impl std::error::Error for Error {}

/// The owner retains the rejected live payload or snapshot. Its destructor is
/// never invoked by an admission method returning this error.
#[derive(Debug)]
pub struct AdmissionError<T> {
    pub kind: Error,
    pub input: T,
}
impl<T> fmt::Display for AdmissionError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}
impl<T: fmt::Debug> std::error::Error for AdmissionError<T> {}

/// A live payload removed from the model; the owner chooses its retirement path.
#[derive(Debug)]
pub struct OpenTab<T> {
    id: TabId,
    live: T,
}
impl<T> OpenTab<T> {
    pub fn id(&self) -> TabId {
        self.id
    }
    pub fn into_parts(self) -> (TabId, T) {
        (self.id, self.live)
    }
}

#[derive(Debug)]
pub struct ClosedTab {
    id: ClosedTabId,
    former_index: usize,
    snapshot: TabSnapshot,
    bytes: usize,
}
impl ClosedTab {
    pub fn id(&self) -> ClosedTabId {
        self.id
    }
    pub fn former_index(&self) -> usize {
        self.former_index
    }
    pub fn snapshot(&self) -> &TabSnapshot {
        &self.snapshot
    }
    pub fn accounted_bytes(&self) -> usize {
        self.bytes
    }
}

#[derive(Debug)]
pub struct Closed<T> {
    pub removed: OpenTab<T>,
    pub remembered: Option<ClosedTabId>,
    /// Oldest first; no live payloads are present in these records.
    pub evicted: Vec<ClosedTab>,
}

#[derive(Debug)]
pub struct Reopened {
    pub id: TabId,
    pub snapshot: TabSnapshot,
}

#[derive(Debug)]
pub struct Parts<T> {
    pub active: Option<TabId>,
    pub open: Vec<OpenTab<T>>,
    /// Oldest first.
    pub closed: Vec<ClosedTab>,
}

/// A workspace may be empty. A nonempty workspace always has one active tab.
/// Open `T` memory is caller-controlled. Use `into_parts` before destruction if
/// live payloads need nonjoining/off-thread retirement; ordinary Drop drops `T`.
pub struct Workspace<T> {
    owner: NonZeroU64,
    next_sequence: u64,
    limits: Limits,
    tabs: Vec<OpenTab<T>>,
    active: Option<TabId>,
    closed: VecDeque<ClosedTab>,
    closed_bytes: usize,
}

fn allocate_owner(counter: &AtomicU64) -> Result<NonZeroU64, Error> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            if n == 0 {
                None
            } else {
                n.checked_add(1)
            }
        })
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(Error::OwnerExhausted)
}

impl<T> Workspace<T> {
    pub fn new(limits: Limits) -> Result<Self, Error> {
        let limits = limits.validate()?;
        Ok(Self {
            owner: allocate_owner(&NEXT_OWNER)?,
            next_sequence: 1,
            limits,
            tabs: Vec::new(),
            active: None,
            closed: VecDeque::new(),
            closed_bytes: 0,
        })
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    pub fn len(&self) -> usize {
        self.tabs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
    pub fn active(&self) -> Option<TabId> {
        self.active
    }
    pub fn tabs(&self) -> impl ExactSizeIterator<Item = (TabId, &T)> + DoubleEndedIterator {
        self.tabs.iter().map(|tab| (tab.id, &tab.live))
    }
    pub fn recently_closed(
        &self,
    ) -> impl ExactSizeIterator<Item = &ClosedTab> + DoubleEndedIterator {
        self.closed.iter().rev()
    }
    pub fn closed_bytes(&self) -> usize {
        self.closed_bytes
    }
    fn index(&self, id: TabId) -> Result<usize, Error> {
        if id.owner != self.owner {
            return Err(Error::ForeignTab);
        }
        self.tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(Error::StaleTab)
    }
    fn closed_index(&self, id: ClosedTabId) -> Result<usize, Error> {
        if id.owner != self.owner {
            return Err(Error::ForeignClosedTab);
        }
        self.closed
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(Error::StaleClosedTab)
    }
    fn next_identity(&self) -> Result<(NonZeroU64, u64), Error> {
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(Error::IdentityExhausted)?;
        let sequence = NonZeroU64::new(self.next_sequence).ok_or(Error::IdentityExhausted)?;
        Ok((sequence, next))
    }
    pub fn get(&self, id: TabId) -> Result<&T, Error> {
        Ok(&self.tabs[self.index(id)?].live)
    }
    pub fn get_mut(&mut self, id: TabId) -> Result<&mut T, Error> {
        let index = self.index(id)?;
        Ok(&mut self.tabs[index].live)
    }
    pub fn closed(&self, id: ClosedTabId) -> Result<&ClosedTab, Error> {
        Ok(&self.closed[self.closed_index(id)?])
    }
    pub fn activate(&mut self, id: TabId) -> Result<bool, Error> {
        self.index(id)?;
        let changed = self.active != Some(id);
        self.active = Some(id);
        Ok(changed)
    }
    pub fn create(&mut self, live: T, activation: Activation) -> Result<TabId, AdmissionError<T>> {
        self.insert(self.len(), live, activation)
    }
    /// `index` is the insertion position in 0..=len. Background insertion into
    /// an empty workspace becomes active, preserving the nonempty invariant.
    pub fn insert(
        &mut self,
        index: usize,
        live: T,
        activation: Activation,
    ) -> Result<TabId, AdmissionError<T>> {
        let validation = if index > self.len() {
            Err(Error::PositionOutOfBounds {
                index,
                len: self.len(),
            })
        } else if self.len() >= self.limits.max_open {
            Err(Error::OpenLimit)
        } else {
            self.next_identity()
        };
        let (sequence, next) = match validation {
            Ok(identity) => identity,
            Err(kind) => return Err(AdmissionError { kind, input: live }),
        };
        let id = TabId {
            owner: self.owner,
            sequence,
        };
        self.tabs.insert(index, OpenTab { id, live });
        self.next_sequence = next;
        if self.active.is_none() || activation == Activation::Foreground {
            self.active = Some(id);
        }
        Ok(id)
    }
    /// `final_index` is the tab's index after reordering, in 0..len.
    pub fn reorder(&mut self, id: TabId, final_index: usize) -> Result<bool, Error> {
        let index = self.index(id)?;
        if final_index >= self.len() {
            return Err(Error::PositionOutOfBounds {
                index: final_index,
                len: self.len(),
            });
        }
        if final_index == index {
            return Ok(false);
        }
        let tab = self.tabs.remove(index);
        self.tabs.insert(final_index, tab);
        Ok(true)
    }
    fn remove(&mut self, index: usize) -> OpenTab<T> {
        let tab = self.tabs.remove(index);
        if self.active == Some(tab.id) {
            self.active = self
                .tabs
                .get(index)
                .or_else(|| self.tabs.last())
                .map(|tab| tab.id);
        }
        tab
    }
    pub fn close_without_history(&mut self, id: TabId) -> Result<Closed<T>, Error> {
        let index = self.index(id)?;
        Ok(Closed {
            removed: self.remove(index),
            remembered: None,
            evicted: Vec::new(),
        })
    }
    /// Remembers only the supplied resume state; never clones or destroys `T`.
    /// Oldest records are evicted by count/bytes and returned to the caller.
    pub fn close(
        &mut self,
        id: TabId,
        snapshot: TabSnapshot,
    ) -> Result<Closed<T>, AdmissionError<TabSnapshot>> {
        let validation = (|| {
            let index = self.index(id)?;
            if self.limits.max_closed == 0 {
                return Err(Error::HistoryDisabled);
            }
            if snapshot.back.len() > self.limits.history_per_direction
                || snapshot.forward.len() > self.limits.history_per_direction
            {
                return Err(Error::HistoryLimit);
            }
            let bytes = snapshot.accounted_bytes().ok_or(Error::SnapshotTooLarge)?;
            if bytes > self.limits.max_snapshot_bytes {
                return Err(Error::SnapshotTooLarge);
            }
            let (sequence, next) = self.next_identity()?;
            Ok((index, bytes, sequence, next))
        })();
        let (index, bytes, sequence, next) = match validation {
            Ok(values) => values,
            Err(kind) => {
                return Err(AdmissionError {
                    kind,
                    input: snapshot,
                })
            }
        };
        let mut evicted = Vec::new();
        while self.closed.len() >= self.limits.max_closed
            || self.closed_bytes + bytes > self.limits.max_closed_bytes
        {
            let old = self
                .closed
                .pop_front()
                .expect("validated snapshot fits an empty history");
            self.closed_bytes -= old.bytes;
            evicted.push(old);
        }
        let closed_id = ClosedTabId {
            owner: self.owner,
            sequence,
        };
        self.closed.push_back(ClosedTab {
            id: closed_id,
            former_index: index,
            snapshot,
            bytes,
        });
        self.closed_bytes += bytes;
        self.next_sequence = next;
        Ok(Closed {
            removed: self.remove(index),
            remembered: Some(closed_id),
            evicted,
        })
    }
    /// Reconstruct `live` outside the model from `closed(id)?.snapshot()` first.
    /// Failure returns it without consuming the close record. Success inserts at
    /// the old position clamped to len, and returns the original resume state.
    pub fn reopen(
        &mut self,
        id: ClosedTabId,
        live: T,
        activation: Activation,
    ) -> Result<Reopened, AdmissionError<T>> {
        let validation = (|| {
            let index = self.closed_index(id)?;
            if self.len() >= self.limits.max_open {
                return Err(Error::OpenLimit);
            }
            let (sequence, next) = self.next_identity()?;
            Ok((index, sequence, next))
        })();
        let (index, sequence, next) = match validation {
            Ok(values) => values,
            Err(kind) => return Err(AdmissionError { kind, input: live }),
        };
        let record = self.closed.remove(index).expect("validated close record");
        self.closed_bytes -= record.bytes;
        let id = TabId {
            owner: self.owner,
            sequence,
        };
        self.tabs
            .insert(record.former_index.min(self.len()), OpenTab { id, live });
        self.next_sequence = next;
        if self.active.is_none() || activation == Activation::Foreground {
            self.active = Some(id);
        }
        Ok(Reopened {
            id,
            snapshot: record.snapshot,
        })
    }
    pub fn clear_recently_closed(&mut self) -> Vec<ClosedTab> {
        self.closed_bytes = 0;
        self.closed.drain(..).collect()
    }
    /// Lets the owner schedule live retirement before workspace destruction.
    pub fn into_parts(self) -> Parts<T> {
        Parts {
            active: self.active,
            open: self.tabs,
            closed: self.closed.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn snapshot(name: &str) -> TabSnapshot {
        TabSnapshot::new(PathBuf::from(name), Vec::new(), Vec::new(), String::new())
    }
    fn names<'a>(workspace: &Workspace<&'a str>) -> Vec<&'a str> {
        workspace.tabs().map(|(_, name)| *name).collect()
    }

    #[test]
    fn interleaved_order_close_and_reopen_keep_identity_and_deterministic_neighbors() {
        let mut workspace = Workspace::new(Limits::default()).unwrap();
        let a = workspace.create("a", Activation::Background).unwrap();
        let b = workspace.create("b", Activation::Background).unwrap();
        let c = workspace.create("c", Activation::Foreground).unwrap();
        assert_eq!(workspace.active(), Some(c));
        workspace.activate(a).unwrap();
        workspace.reorder(c, 0).unwrap();
        assert_eq!(names(&workspace), ["c", "a", "b"]);
        assert_eq!(workspace.active(), Some(a));
        let closed_a = workspace.close(a, snapshot("/a")).unwrap();
        assert_eq!(closed_a.removed.into_parts(), (a, "a"));
        assert_eq!(
            workspace.active(),
            Some(b),
            "active close chooses right neighbor"
        );
        let closed_a = closed_a.remembered.unwrap();
        let closed_c = workspace
            .close(c, snapshot("/c"))
            .unwrap()
            .remembered
            .unwrap();
        assert_eq!(
            workspace.active(),
            Some(b),
            "inactive close preserves active ID"
        );
        let reopened_a = workspace
            .reopen(closed_a, "a resumed", Activation::Foreground)
            .unwrap();
        assert_ne!(reopened_a.id, a);
        assert_eq!(reopened_a.snapshot.location(), Path::new("/a"));
        assert_eq!(names(&workspace), ["b", "a resumed"]);
        workspace
            .reopen(closed_c, "c resumed", Activation::Background)
            .unwrap();
        assert_eq!(names(&workspace), ["c resumed", "b", "a resumed"]);
        assert_eq!(workspace.active(), Some(reopened_a.id));
        assert_eq!(workspace.get(a), Err(Error::StaleTab));
        assert_eq!(
            workspace.closed(closed_a).unwrap_err(),
            Error::StaleClosedTab
        );
        workspace.close_without_history(reopened_a.id).unwrap();
        assert_eq!(
            workspace.active(),
            Some(b),
            "last-position close chooses left"
        );
        workspace.close_without_history(b).unwrap();
        let last = workspace.active().unwrap();
        workspace.close_without_history(last).unwrap();
        assert!(workspace.is_empty());
        assert_eq!(workspace.active(), None);
        let first = workspace
            .create("first again", Activation::Background)
            .unwrap();
        assert_eq!(workspace.active(), Some(first));
    }

    #[test]
    fn returned_errors_preserve_payload_history_active_order_and_capacity() {
        let limits = Limits {
            max_open: 2,
            ..Limits::default()
        };
        let mut workspace = Workspace::new(limits).unwrap();
        let a = workspace.create("a", Activation::Foreground).unwrap();
        let saved = workspace
            .close(a, snapshot("/a"))
            .unwrap()
            .remembered
            .unwrap();
        let b = workspace.create("b", Activation::Foreground).unwrap();
        workspace.create("c", Activation::Background).unwrap();
        let failed = workspace
            .reopen(saved, "new live a", Activation::Foreground)
            .unwrap_err();
        assert_eq!(failed.kind, Error::OpenLimit);
        assert_eq!(failed.input, "new live a");
        assert_eq!(names(&workspace), ["b", "c"]);
        assert_eq!(workspace.active(), Some(b));
        assert_eq!(
            workspace.closed(saved).unwrap().snapshot().location(),
            Path::new("/a")
        );
        assert!(workspace.reorder(b, 2).is_err());
        assert_eq!(names(&workspace), ["b", "c"]);
        workspace.close_without_history(b).unwrap();
        let reopened = workspace
            .reopen(saved, failed.input, Activation::Foreground)
            .unwrap();
        assert_eq!(workspace.get(reopened.id), Ok(&"new live a"));
        assert_eq!(workspace.closed(saved).unwrap_err(), Error::StaleClosedTab);
        assert_eq!(workspace.recently_closed().len(), 0);
    }

    #[test]
    fn byte_and_count_eviction_are_oldest_first_and_return_plain_resume_records() {
        let bytes = snapshot("/a").accounted_bytes().unwrap();
        let limits = Limits {
            max_closed: 3,
            max_snapshot_bytes: bytes,
            max_closed_bytes: 2 * bytes,
            ..Limits::default()
        };
        let mut workspace = Workspace::new(limits).unwrap();
        let mut closed_ids = Vec::new();
        for name in ["/a", "/b", "/c"] {
            let tab = workspace.create(name, Activation::Foreground).unwrap();
            let closed = workspace.close(tab, snapshot(name)).unwrap();
            closed_ids.push(closed.remembered.unwrap());
            if name == "/c" {
                assert_eq!(closed.evicted.len(), 1);
                assert_eq!(closed.evicted[0].id(), closed_ids[0]);
                assert_eq!(closed.evicted[0].snapshot().location(), Path::new("/a"));
            } else {
                assert!(closed.evicted.is_empty());
            }
        }
        assert_eq!(workspace.closed_bytes(), 2 * bytes);
        assert_eq!(
            workspace
                .recently_closed()
                .map(ClosedTab::id)
                .collect::<Vec<_>>(),
            [closed_ids[2], closed_ids[1]]
        );
        assert_eq!(
            workspace.closed(closed_ids[0]).unwrap_err(),
            Error::StaleClosedTab
        );
        let removed = workspace.clear_recently_closed();
        assert_eq!(
            removed.iter().map(ClosedTab::id).collect::<Vec<_>>(),
            [closed_ids[1], closed_ids[2]]
        );
        assert_eq!(workspace.closed_bytes(), 0);

        let mut count_limited = Workspace::new(Limits {
            max_closed: 1,
            ..Limits::default()
        })
        .unwrap();
        let first = count_limited.create(1, Activation::Foreground).unwrap();
        let first = count_limited
            .close(first, snapshot("/first"))
            .unwrap()
            .remembered
            .unwrap();
        let second = count_limited.create(2, Activation::Foreground).unwrap();
        let result = count_limited.close(second, snapshot("/second")).unwrap();
        assert_eq!(result.evicted[0].id(), first);
        assert_eq!(count_limited.recently_closed().len(), 1);
    }

    #[test]
    fn history_and_native_byte_budgets_reject_without_closing_the_live_tab() {
        let bytes = snapshot("/a").accounted_bytes().unwrap();
        let mut workspace = Workspace::new(Limits {
            history_per_direction: 1,
            max_snapshot_bytes: bytes,
            max_closed_bytes: 2 * bytes,
            ..Limits::default()
        })
        .unwrap();
        let live = workspace.create("live", Activation::Foreground).unwrap();
        let large_history = TabSnapshot::new(
            "/a".into(),
            vec!["/back1".into(), "/back2".into()],
            Vec::new(),
            String::new(),
        );
        let error = workspace.close(live, large_history.clone()).unwrap_err();
        assert_eq!(error.kind, Error::HistoryLimit);
        assert_eq!(error.input, large_history);
        let extra_bytes = TabSnapshot::new("/a".into(), Vec::new(), Vec::new(), "é".into());
        let error = workspace.close(live, extra_bytes.clone()).unwrap_err();
        assert_eq!(error.kind, Error::SnapshotTooLarge);
        assert_eq!(error.input, extra_bytes);
        assert_eq!(workspace.active(), Some(live));
        assert_eq!(workspace.get(live), Ok(&"live"));
        assert_eq!(workspace.recently_closed().len(), 0);
        assert_eq!(workspace.closed_bytes(), 0);
    }

    #[test]
    fn stale_async_delivery_cannot_target_a_reordered_or_reopened_tab_by_index() {
        let mut workspace = Workspace::new(Limits::default()).unwrap();
        let a = workspace
            .create(String::from("a"), Activation::Foreground)
            .unwrap();
        let b = workspace
            .create(String::from("b"), Activation::Foreground)
            .unwrap();
        workspace.reorder(b, 0).unwrap();
        workspace.get_mut(a).unwrap().push_str(" delivery");
        assert_eq!(
            workspace
                .tabs()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            ["b", "a delivery"]
        );
        let closed = workspace.close(a, snapshot("/a")).unwrap();
        let closed_id = closed.remembered.unwrap();
        let reopened = workspace
            .reopen(closed_id, String::from("fresh"), Activation::Foreground)
            .unwrap();
        assert_eq!(workspace.get_mut(a), Err(Error::StaleTab));
        assert_eq!(workspace.get(reopened.id).unwrap(), "fresh");
        let mut foreign = Workspace::new(Limits::default()).unwrap();
        let foreign_id = foreign
            .create(String::new(), Activation::Foreground)
            .unwrap();
        assert_eq!(workspace.activate(foreign_id), Err(Error::ForeignTab));
        let foreign_closed = foreign
            .close(foreign_id, snapshot("/foreign"))
            .unwrap()
            .remembered
            .unwrap();
        assert_eq!(
            workspace
                .reopen(
                    foreign_closed,
                    String::from("rejected"),
                    Activation::Background
                )
                .unwrap_err()
                .kind,
            Error::ForeignClosedTab
        );
        assert_eq!(workspace.get(reopened.id).unwrap(), "fresh");
    }

    #[derive(Debug)]
    struct Live(Rc<Cell<usize>>);
    impl Drop for Live {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn removed_and_rejected_live_payloads_return_to_owner_without_model_retirement() {
        let drops = Rc::new(Cell::new(0));
        let mut workspace = Workspace::new(Limits {
            max_open: 1,
            ..Limits::default()
        })
        .unwrap();
        let first = workspace
            .create(Live(drops.clone()), Activation::Foreground)
            .unwrap();
        let rejected = workspace
            .create(Live(drops.clone()), Activation::Background)
            .unwrap_err();
        assert_eq!(rejected.kind, Error::OpenLimit);
        assert_eq!(drops.get(), 0);
        let removed = workspace.close(first, snapshot("/resume")).unwrap();
        assert_eq!(drops.get(), 0);
        assert!(workspace.is_empty());
        let again = workspace
            .create(rejected.input, Activation::Foreground)
            .unwrap();
        let parts = workspace.into_parts();
        assert_eq!(parts.active, Some(again));
        assert_eq!(parts.open.len(), 1);
        assert_eq!(parts.closed.len(), 1);
        assert_eq!(
            drops.get(),
            0,
            "into_parts must hand off live ownership too"
        );
        drop(removed);
        assert_eq!(drops.get(), 1);
        drop(parts);
        assert_eq!(
            drops.get(),
            2,
            "retirement occurs only when owner drops returned data"
        );
    }

    #[test]
    fn identity_exhaustion_is_checked_and_failed_admission_does_not_burn_the_last_id() {
        let counter = AtomicU64::new(u64::MAX);
        assert_eq!(allocate_owner(&counter), Err(Error::OwnerExhausted));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        let mut workspace = Workspace::new(Limits::default()).unwrap();
        workspace.next_sequence = u64::MAX - 1;
        let failed = workspace
            .insert(1, "out of bounds", Activation::Foreground)
            .unwrap_err();
        assert_eq!(failed.kind, Error::PositionOutOfBounds { index: 1, len: 0 });
        let last = workspace.create("last", Activation::Foreground).unwrap();
        assert_eq!(
            workspace
                .create("not admitted", Activation::Foreground)
                .unwrap_err()
                .kind,
            Error::IdentityExhausted
        );
        let error = workspace.close(last, snapshot("/last")).unwrap_err();
        assert_eq!(error.kind, Error::IdentityExhausted);
        assert_eq!(workspace.get(last), Ok(&"last"));
        assert_eq!(workspace.recently_closed().len(), 0);
        let removed = workspace.close_without_history(last).unwrap();
        assert_eq!(removed.removed.into_parts(), (last, "last"));
        assert!(workspace.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_state_round_trips_without_display_conversion_or_filesystem_access() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(
            b"/not-created/native-\xff".to_vec(),
        ));
        let back = PathBuf::from(std::ffi::OsString::from_vec(b"relative-\xfe".to_vec()));
        let resume = TabSnapshot::new(
            path.clone(),
            vec![back.clone()],
            vec!["/also/not-created".into()],
            "日本語".into(),
        );
        let mut workspace = Workspace::new(Limits::default()).unwrap();
        let tab = workspace.create((), Activation::Foreground).unwrap();
        let saved = workspace
            .close(tab, resume.clone())
            .unwrap()
            .remembered
            .unwrap();
        assert_eq!(workspace.closed(saved).unwrap().snapshot(), &resume);
        let restored = workspace.reopen(saved, (), Activation::Background).unwrap();
        assert_eq!(restored.snapshot.location(), path);
        assert_eq!(restored.snapshot.back().next().unwrap(), back);
        assert_eq!(restored.snapshot.filter(), "日本語");
    }

    #[test]
    fn disabled_history_and_invalid_limits_fail_explicitly() {
        assert!(matches!(
            Workspace::<()>::new(Limits {
                max_open: 0,
                ..Limits::default()
            }),
            Err(Error::InvalidLimits)
        ));
        assert!(matches!(
            Workspace::<()>::new(Limits {
                max_open: 257,
                ..Limits::default()
            }),
            Err(Error::InvalidLimits)
        ));
        assert!(matches!(
            Workspace::<()>::new(Limits {
                max_closed_bytes: 1,
                ..Limits::default()
            }),
            Err(Error::InvalidLimits)
        ));
        let mut workspace = Workspace::new(Limits {
            max_closed: 0,
            max_closed_bytes: 0,
            ..Limits::default()
        })
        .unwrap();
        let id = workspace.create((), Activation::Foreground).unwrap();
        assert_eq!(
            workspace
                .close(id, snapshot("/not-retained"))
                .unwrap_err()
                .kind,
            Error::HistoryDisabled
        );
        assert_eq!(workspace.len(), 1);
        workspace.close_without_history(id).unwrap();
        assert_eq!(workspace.recently_closed().len(), 0);
    }

    #[test]
    fn simultaneous_workspace_creation_has_distinct_owners() {
        let handles = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    Workspace::new(Limits::default())
                        .unwrap()
                        .create((), Activation::Foreground)
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 8);
    }
}
