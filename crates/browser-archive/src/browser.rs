//! Read-only, bounded UI mailbox. The worker owns every catalog/session/index.
use crate::{
    ArchiveEntry, ArchiveEntryId, ArchiveError, ArchiveSession, Cancellation, CatalogState,
    Control, EntryKind, ErrorKind, Fingerprint, Limits,
};
use std::{
    borrow::Borrow,
    cmp::Ordering as Compare,
    collections::HashMap,
    fmt, io,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

pub const MAX_BROWSER_PAGE_SIZE: usize = 512;
pub const MAX_BROWSER_PAGE_BYTES: usize = 1 << 20;
const TEXT_BYTES: usize = 1024;
const PREFIX_COMPONENTS: usize = 256;
const PREFIX_BYTES: usize = u16::MAX as usize;
static NEXT_BROWSER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BrowserGeneration(u64);
impl BrowserGeneration {
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Validated archive components. This type is deliberately unrelated to PathBuf.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ArchivePrefix(Arc<[String]>);
impl ArchivePrefix {
    pub fn root() -> Self {
        Self::default()
    }
    pub fn from_components(
        components: Vec<String>,
    ) -> std::result::Result<Self, BrowserSubmitError> {
        Self::validate(&components)?;
        Ok(Self(components.into()))
    }
    fn validate(components: &[String]) -> std::result::Result<(), BrowserSubmitError> {
        let bytes = components
            .iter()
            .try_fold(0usize, |sum, c| sum.checked_add(c.len())?.checked_add(1));
        if components.len() > PREFIX_COMPONENTS || bytes.is_none_or(|n| n > PREFIX_BYTES) {
            return Err(BrowserSubmitError::InvalidPrefix);
        }
        for component in components {
            if component.contains('/') || crate::names::components(component, 1).is_err() {
                return Err(BrowserSubmitError::InvalidPrefix);
            }
        }
        Ok(())
    }
    pub fn components(&self) -> &[String] {
        &self.0
    }
    pub fn parent(&self) -> Option<Self> {
        (!self.0.is_empty()).then(|| {
            Self::from_components(self.0[..self.0.len() - 1].to_vec())
                .expect("a valid prefix has a valid parent")
        })
    }
    fn bytes(&self) -> usize {
        self.0.iter().map(|c| c.len() + size_of::<String>()).sum()
    }
}
impl Borrow<[String]> for ArchivePrefix {
    fn borrow(&self) -> &[String] {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct BrowserOptions {
    pub source: PathBuf,
    pub limits: Limits,
    pub scan_timeout: Duration,
    pub list_timeout: Duration,
    /// Additional derived-tree admission budget, distinct from ZIP metadata.
    pub index_bytes: usize,
    pub index_nodes: usize,
}
impl BrowserOptions {
    pub fn new(source: PathBuf) -> Self {
        Self {
            source,
            limits: Limits::default(),
            scan_timeout: Duration::from_secs(30),
            list_timeout: Duration::from_secs(5),
            index_bytes: 32 << 20,
            index_nodes: 1_000_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListRequest {
    pub request_id: u64,
    pub prefix: ArchivePrefix,
    pub offset: usize,
    pub limit: usize,
}
impl ListRequest {
    pub fn root(request_id: u64, offset: usize, limit: usize) -> Self {
        Self {
            request_id,
            prefix: ArchivePrefix::root(),
            offset,
            limit,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserSubmitError {
    Closed,
    InvalidPrefix,
    NonMonotonicRequest { latest: u64, received: u64 },
    PageSize { requested: usize, maximum: usize },
}
impl fmt::Display for BrowserSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BrowserSubmitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    Zip,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserCatalogState {
    Opening,
    Complete,
    LimitReached,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserErrorKind {
    Archive(ErrorKind),
    PrefixNotFound,
    IndexLimit,
    WorkerStopped,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserError {
    pub kind: BrowserErrorKind,
    pub message: String,
    pub message_truncated: bool,
}
impl BrowserError {
    fn archive(error: &ArchiveError) -> Self {
        Self::new(BrowserErrorKind::Archive(error.kind), &error.to_string())
    }
    fn new(kind: BrowserErrorKind, message: &str) -> Self {
        let (message, message_truncated) = bounded_text(message);
        Self {
            kind,
            message,
            message_truncated,
        }
    }
}
#[derive(Clone, Debug)]
pub struct BrowserCatalog {
    pub state: BrowserCatalogState,
    pub records_seen: usize,
    pub indexed_records: usize,
    pub logical_directories: usize,
    pub source_valid: bool,
    pub error: Option<BrowserError>,
}
impl BrowserCatalog {
    fn opening() -> Self {
        Self {
            state: BrowserCatalogState::Opening,
            records_seen: 0,
            indexed_records: 0,
            logical_directories: 0,
            source_valid: false,
            error: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ArchiveNodeId {
    Record(ArchiveEntryId),
    VirtualDirectory {
        generation: BrowserGeneration,
        ordinal: u64,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserIssueKind {
    UnsafeName,
    AmbiguousName,
    UnsupportedCompression,
    UnsupportedKind,
    PayloadDirectory,
}
#[derive(Clone, Debug)]
pub struct BrowserIssue {
    pub kind: BrowserIssueKind,
    pub message: String,
    pub message_truncated: bool,
}
#[derive(Clone, Debug)]
pub struct BrowserNode {
    pub id: ArchiveNodeId,
    pub display_name: String,
    pub name_truncated: bool,
    pub kind: EntryKind,
    pub record_id: Option<ArchiveEntryId>,
    /// Only validated directory prefixes can be navigated. Invalid rows have None.
    pub navigation: Option<ArchivePrefix>,
    pub synthetic: bool,
    pub compressed_bytes: Option<u64>,
    pub uncompressed_bytes: Option<u64>,
    /// Declared method only. In particular, 99 is an AES wrapper whose metadata
    /// and actual payload codec/authentication have not been validated by browsing.
    pub compression: Option<u16>,
    pub encrypted: bool,
    pub issue: Option<BrowserIssue>,
}
impl BrowserNode {
    fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.display_name.len()
            + self.navigation.as_ref().map_or(0, ArchivePrefix::bytes)
            + self.issue.as_ref().map_or(0, |i| i.message.len())
    }
}
#[derive(Clone, Debug)]
pub struct BrowserPage {
    pub generation: BrowserGeneration,
    pub request_id: u64,
    pub source: Arc<PathBuf>,
    pub format: Option<ArchiveFormat>,
    pub prefix: ArchivePrefix,
    pub offset: usize,
    pub requested_limit: usize,
    /// Count within the known tree. It is not an archive-completeness claim.
    pub total_children: usize,
    pub next_offset: Option<usize>,
    pub nodes: Vec<BrowserNode>,
    pub catalog: BrowserCatalog,
    /// Request/source errors are separate from retained catalog coverage.
    pub error: Option<BrowserError>,
}

struct Pending {
    request: ListRequest,
    cancel: Arc<Cancellation>,
}
struct State {
    current: ListRequest,
    pending: Option<Pending>,
    preparation: Option<Box<crate::jobs::Preparation>>,
    active_cancel: Arc<Cancellation>,
    delivery: Option<Box<BrowserPage>>,
    catalog: BrowserCatalog,
    format: Option<ArchiveFormat>,
    closed: bool,
}
#[cfg(test)]
type PreparationGate = Arc<(Mutex<bool>, Condvar)>;
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: Cancellation,
    source: Arc<PathBuf>,
    generation: BrowserGeneration,
    exited: AtomicBool,
    #[cfg(test)]
    preparation_gate: Mutex<Option<PreparationGate>>,
    #[cfg(test)]
    session_probe: Mutex<std::sync::Weak<ArchiveSession>>,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
    fn empty_page(
        &self,
        request: &ListRequest,
        catalog: BrowserCatalog,
        format: Option<ArchiveFormat>,
    ) -> BrowserPage {
        BrowserPage {
            generation: self.generation,
            request_id: request.request_id,
            source: self.source.clone(),
            format,
            prefix: request.prefix.clone(),
            offset: request.offset,
            requested_limit: request.limit,
            total_children: 0,
            next_offset: None,
            nodes: Vec::new(),
            catalog,
            error: None,
        }
    }
    fn publish(&self, pending: &Pending, page: BrowserPage) -> bool {
        let mut state = self.lock();
        if state.closed
            || state.current.request_id != pending.request.request_id
            || pending.cancel.is_cancelled()
        {
            return false;
        }
        state.catalog = page.catalog.clone();
        state.format = page.format;
        state.delivery = Some(Box::new(page));
        true
    }
}

/// One persistent worker owns scan, catalog, index, sort and reclamation. Constructor,
/// request, poll, cancel and Drop perform no filesystem I/O and never join a worker.
pub struct ArchiveBrowser {
    shared: Arc<Shared>,
}
impl ArchiveBrowser {
    /// Starts root request 0 asynchronously. Source classification is by ZIP bytes,
    /// never by extension. A failed ZIP scan remains an explicit failed catalog.
    pub fn open(options: BrowserOptions) -> io::Result<Self> {
        Self::spawn(options, || {})
    }
    fn spawn(
        options: BrowserOptions,
        before_open: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        if !options.source.is_absolute() || options.source.as_os_str().len() > PREFIX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive source must be an absolute native path of bounded length",
            ));
        }
        if options.limits.path_components > PREFIX_COMPONENTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive browser path-component limit exceeds 256",
            ));
        }
        let generation = NEXT_BROWSER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map(BrowserGeneration)
            .map_err(|_| io::Error::other("archive browser generation exhausted"))?;
        let current = ListRequest::root(0, 0, MAX_BROWSER_PAGE_SIZE);
        let cancel = Arc::new(Cancellation::default());
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                current: current.clone(),
                preparation: None,
                pending: Some(Pending {
                    request: current.clone(),
                    cancel: cancel.clone(),
                }),
                active_cancel: cancel,
                delivery: None,
                catalog: BrowserCatalog::opening(),
                format: None,
                closed: false,
            }),
            wake: Condvar::new(),
            stop: Cancellation::default(),
            source: Arc::new(options.source.clone()),
            generation,
            exited: AtomicBool::new(false),
            #[cfg(test)]
            preparation_gate: Mutex::new(None),
            #[cfg(test)]
            session_probe: Mutex::new(std::sync::Weak::new()),
        });
        shared.lock().delivery = Some(Box::new(shared.empty_page(
            &current,
            BrowserCatalog::opening(),
            None,
        )));
        let worker = shared.clone();
        thread::Builder::new()
            .name("archive-browser".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    before_open();
                    worker_main(&worker, &options);
                }));
                if result.is_err() {
                    let mut state = worker.lock();
                    let preparation = state.preparation.take();
                    drop(state);
                    drop(preparation);
                    let mut state = worker.lock();
                    if !state.closed {
                        let mut catalog = state.catalog.clone();
                        catalog.state = BrowserCatalogState::Failed;
                        catalog.source_valid = false;
                        catalog.error = Some(BrowserError::new(
                            BrowserErrorKind::WorkerStopped,
                            "archive browser worker stopped unexpectedly",
                        ));
                        state.delivery = Some(Box::new(worker.empty_page(
                            &state.current,
                            catalog,
                            state.format,
                        )));
                        state.closed = true;
                    }
                }
                worker.exited.store(true, Ordering::Release);
            })?;
        Ok(Self { shared })
    }
    pub(crate) fn enqueue_preparation(
        &self,
        request: Box<crate::jobs::Preparation>,
    ) -> std::result::Result<(), (crate::ExtractionSubmitError, Box<crate::jobs::Preparation>)>
    {
        let mut state = self.shared.lock();
        let error = if state.closed {
            Some(crate::ExtractionSubmitError::Closed)
        } else if state.catalog.state != BrowserCatalogState::Complete {
            Some(crate::ExtractionSubmitError::NotReady)
        } else if state.preparation.is_some() {
            Some(crate::ExtractionSubmitError::Busy)
        } else {
            None
        };
        if let Some(error) = error {
            return Err((error, request));
        }
        state.preparation = Some(request);
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn generation(&self) -> BrowserGeneration {
        self.shared.generation
    }
    pub fn source(&self) -> &std::path::Path {
        &self.shared.source
    }
    pub fn request_list(
        &self,
        request: ListRequest,
    ) -> std::result::Result<(), BrowserSubmitError> {
        if request.limit == 0 || request.limit > MAX_BROWSER_PAGE_SIZE {
            return Err(BrowserSubmitError::PageSize {
                requested: request.limit,
                maximum: MAX_BROWSER_PAGE_SIZE,
            });
        }
        let mut state = self.shared.lock();
        if state.closed {
            return Err(BrowserSubmitError::Closed);
        }
        if request.request_id <= state.current.request_id {
            return Err(BrowserSubmitError::NonMonotonicRequest {
                latest: state.current.request_id,
                received: request.request_id,
            });
        }
        state.active_cancel.cancel();
        let cancel = Arc::new(Cancellation::default());
        state.active_cancel = cancel.clone();
        state.current = request.clone();
        state.pending = Some(Pending { request, cancel });
        state.delivery = None;
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn poll(&self) -> Option<Box<BrowserPage>> {
        self.shared.lock().delivery.take()
    }
    /// Terminal cancellation; create a new service to reopen. Completion is
    /// cooperative if the worker is blocked in an OS read. No UI-side join.
    pub fn cancel(&self) -> bool {
        let mut state = self.shared.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        self.shared.stop.cancel();
        state.active_cancel.cancel();
        state.pending = None;
        let mut catalog = state.catalog.clone();
        catalog.state = BrowserCatalogState::Cancelled;
        state.delivery = Some(Box::new(self.shared.empty_page(
            &state.current,
            catalog,
            state.format,
        )));
        self.shared.wake.notify_one();
        true
    }
    pub fn is_worker_stopped(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }
}
impl Drop for ArchiveBrowser {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Node {
    Directory(usize),
    Record(usize),
}
struct Directory {
    prefix: ArchivePrefix,
    children: Vec<Node>,
    explicit: Option<usize>,
    sort_name: String,
    sorted: bool,
}
struct Tree {
    directories: Vec<Directory>,
    lookup: HashMap<ArchivePrefix, usize>,
    record_keys: Vec<String>,
    rejected_prefix_records: Vec<bool>,
    indexed_records: usize,
    charged: usize,
    nodes: usize,
}
impl Tree {
    fn empty() -> Self {
        let root = ArchivePrefix::root();
        Self {
            directories: vec![Directory {
                prefix: root.clone(),
                children: Vec::new(),
                explicit: None,
                sort_name: String::new(),
                sorted: false,
            }],
            lookup: HashMap::from([(root, 0)]),
            record_keys: Vec::new(),
            rejected_prefix_records: Vec::new(),
            indexed_records: 0,
            charged: 512,
            nodes: 0,
        }
    }
    fn charge(
        &mut self,
        bytes: usize,
        nodes: usize,
        options: &BrowserOptions,
    ) -> crate::Result<()> {
        self.charged = self.charged.checked_add(bytes).ok_or_else(|| {
            ArchiveError::new(ErrorKind::LimitExceeded, "archive browser index arithmetic")
        })?;
        self.nodes = self.nodes.checked_add(nodes).ok_or_else(|| {
            ArchiveError::new(ErrorKind::LimitExceeded, "archive browser node arithmetic")
        })?;
        if self.charged > options.index_bytes || self.nodes > options.index_nodes {
            return Err(ArchiveError::new(
                ErrorKind::LimitExceeded,
                "archive browser derived-index budget",
            ));
        }
        Ok(())
    }
    fn directory(
        &mut self,
        components: &[String],
        options: &BrowserOptions,
        control: &Control<'_>,
    ) -> crate::Result<usize> {
        // Validate the entire target before creating even an intermediate directory.
        ArchivePrefix::validate(components).map_err(|_| {
            ArchiveError::new(ErrorKind::UnsafePath, "invalid browser navigation prefix")
        })?;
        let mut parent = 0;
        for length in 1..=components.len() {
            control.check()?;
            let prefix = &components[..length];
            if let Some(&existing) = self.lookup.get(prefix) {
                parent = existing;
                continue;
            }
            let bytes = prefix
                .iter()
                .map(|s| s.len() + size_of::<String>())
                .sum::<usize>();
            self.charge(
                512 + bytes + components[length - 1].len().saturating_mul(4),
                1,
                options,
            )?;
            let prefix = ArchivePrefix::from_components(prefix.to_vec()).map_err(|_| {
                ArchiveError::new(ErrorKind::UnsafePath, "invalid browser navigation prefix")
            })?;
            let index = self.directories.len();
            self.directories.push(Directory {
                prefix: prefix.clone(),
                children: Vec::new(),
                explicit: None,
                sort_name: components[length - 1].to_lowercase(),
                sorted: false,
            });
            self.lookup.insert(prefix, index);
            self.directories[parent]
                .children
                .push(Node::Directory(index));
            parent = index;
        }
        Ok(parent)
    }
    fn build(
        &mut self,
        records: &[ArchiveEntry],
        options: &BrowserOptions,
        control: &Control<'_>,
    ) -> crate::Result<()> {
        for (index, entry) in records.iter().enumerate() {
            control.check()?;
            let explicit_directory = entry.kind == EntryKind::Directory
                && entry.issue.is_none()
                && entry.uncompressed_bytes == 0;
            let required_prefix = entry.components.as_deref().map(|path| {
                if explicit_directory {
                    path
                } else {
                    &path[..path.len().saturating_sub(1)]
                }
            });
            let rejected =
                required_prefix.is_some_and(|prefix| ArchivePrefix::validate(prefix).is_err());
            let name = if rejected {
                entry.name.as_str()
            } else {
                entry
                    .components
                    .as_ref()
                    .and_then(|p| p.last())
                    .map_or(entry.name.as_str(), String::as_str)
            };
            self.charge(256 + name.len().saturating_mul(4), 1, options)?;
            self.record_keys.push(name.to_lowercase());
            self.rejected_prefix_records.push(rejected);
            if let Some(path) = entry.components.as_ref().filter(|_| !rejected) {
                if explicit_directory {
                    let directory = self.directory(path, options, control)?;
                    self.directories[directory].explicit = Some(index);
                } else {
                    let parent = self.directory(&path[..path.len() - 1], options, control)?;
                    self.directories[parent].children.push(Node::Record(index));
                }
            } else {
                // Policy-rejected records still have a visible, stable record row.
                self.directories[0].children.push(Node::Record(index));
            }
            self.indexed_records += 1;
        }
        Ok(())
    }
    fn compare(&self, records: &[ArchiveEntry], left: Node, right: Node) -> Compare {
        let keys = |node| match node {
            Node::Directory(index) => (
                false,
                self.directories[index].sort_name.as_str(),
                self.directories[index].prefix.0.last().unwrap().as_str(),
            ),
            Node::Record(index) => (
                records[index].kind != EntryKind::Directory,
                self.record_keys[index].as_str(),
                if self.rejected_prefix_records[index] {
                    records[index].name.as_str()
                } else {
                    records[index]
                        .components
                        .as_ref()
                        .and_then(|p| p.last())
                        .map_or(records[index].name.as_str(), String::as_str)
                },
            ),
        };
        keys(left).cmp(&keys(right)).then_with(|| left.cmp(&right))
    }
    fn sort(
        &mut self,
        index: usize,
        records: &[ArchiveEntry],
        control: &Control<'_>,
    ) -> crate::Result<()> {
        if self.directories[index].sorted {
            return control.check();
        }
        let mut nodes = std::mem::take(&mut self.directories[index].children);
        let result = sort_cancellable(&mut nodes, |a, b| self.compare(records, a, b), control);
        self.directories[index].children = nodes;
        if result.is_ok() {
            self.directories[index].sorted = true;
        }
        result
    }
    fn node(
        &self,
        node: Node,
        records: &[ArchiveEntry],
        generation: BrowserGeneration,
    ) -> BrowserNode {
        match node {
            Node::Directory(index) => {
                let directory = &self.directories[index];
                if let Some(record) = directory.explicit {
                    let mut node = record_node(&records[record]);
                    node.navigation = Some(directory.prefix.clone());
                    node
                } else {
                    let (display_name, name_truncated) =
                        bounded_text(directory.prefix.0.last().unwrap());
                    BrowserNode {
                        id: ArchiveNodeId::VirtualDirectory {
                            generation,
                            ordinal: index as u64,
                        },
                        display_name,
                        name_truncated,
                        kind: EntryKind::Directory,
                        record_id: None,
                        navigation: Some(directory.prefix.clone()),
                        synthetic: true,
                        compressed_bytes: None,
                        uncompressed_bytes: None,
                        compression: None,
                        encrypted: false,
                        issue: None,
                    }
                }
            }
            Node::Record(index) => {
                let mut node = record_node(&records[index]);
                if self.rejected_prefix_records[index] {
                    (node.display_name, node.name_truncated) = bounded_text(&records[index].name);
                    node.issue = Some(BrowserIssue {
                        kind: BrowserIssueKind::UnsafeName,
                        message:
                            "navigation prefix exceeds browser depth/byte bounds or naming policy"
                                .into(),
                        message_truncated: false,
                    });
                }
                node
            }
        }
    }
}

struct WorkerData {
    session: Option<Arc<ArchiveSession>>,
    partial: Vec<ArchiveEntry>,
    tree: Tree,
    catalog: BrowserCatalog,
    format: Option<ArchiveFormat>,
    fingerprint: Option<Fingerprint>,
    source_failed: bool,
}
impl WorkerData {
    fn records(&self) -> &[ArchiveEntry] {
        self.session
            .as_ref()
            .map_or(&self.partial, |session| session.entries())
    }
    fn open(shared: &Shared, options: &BrowserOptions) -> Self {
        let control = Control::new(
            &shared.stop,
            options.scan_timeout,
            options.limits.total_bytes,
        );
        let fingerprint = std::fs::symlink_metadata(&*shared.source)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| Fingerprint::of(&m));
        let mut data = Self {
            session: None,
            partial: Vec::new(),
            tree: Tree::empty(),
            catalog: BrowserCatalog::opening(),
            format: None,
            fingerprint,
            source_failed: false,
        };
        match ArchiveSession::scan(&shared.source, options.limits.clone(), &control) {
            Ok((session, catalog)) => {
                data.format = Some(ArchiveFormat::Zip);
                data.catalog.state = match catalog.state {
                    CatalogState::Complete => BrowserCatalogState::Complete,
                    CatalogState::LimitReached => BrowserCatalogState::LimitReached,
                    CatalogState::Cancelled => BrowserCatalogState::Cancelled,
                    CatalogState::Failed => BrowserCatalogState::Failed,
                };
                data.catalog.records_seen = catalog.entries.len();
                data.catalog.error = catalog.error.as_ref().map(BrowserError::archive);
                data.session = session.map(Arc::new);
                #[cfg(test)]
                if let Some(session) = &data.session {
                    *shared.session_probe.lock().unwrap() = Arc::downgrade(session);
                }
                if data.session.is_none() {
                    data.partial = catalog.entries;
                }
                let mut tree = Tree::empty();
                if let Err(error) = tree.build(data.records(), options, &control) {
                    data.catalog.state = if error.kind == ErrorKind::LimitExceeded {
                        BrowserCatalogState::LimitReached
                    } else if error.kind == ErrorKind::Cancelled {
                        BrowserCatalogState::Cancelled
                    } else {
                        BrowserCatalogState::Failed
                    };
                    data.catalog.error = Some(if error.kind == ErrorKind::LimitExceeded {
                        BrowserError::new(BrowserErrorKind::IndexLimit, &error.to_string())
                    } else {
                        BrowserError::archive(&error)
                    });
                }
                data.catalog.indexed_records = tree.indexed_records;
                data.catalog.logical_directories = tree.directories.len() - 1;
                data.tree = tree;
                data.catalog.source_valid = data.fingerprint.is_some();
            }
            Err(error) => {
                data.catalog.state = BrowserCatalogState::Failed;
                data.catalog.error = Some(BrowserError::archive(&error));
            }
        }
        data
    }
    fn verify_source(&mut self, shared: &Shared, control: &Control<'_>) -> crate::Result<()> {
        if self.source_failed {
            let kind = match self.catalog.error.as_ref().map(|error| &error.kind) {
                Some(BrowserErrorKind::Archive(kind)) => *kind,
                _ => ErrorKind::SourceChanged,
            };
            return Err(ArchiveError::new(
                kind,
                "archive source validation failed; reopen the browser",
            ));
        }
        let result = if let Some(session) = &self.session {
            session.check_source(control)
        } else if let Some(expected) = &self.fingerprint {
            control.check()?;
            match std::fs::symlink_metadata(&*shared.source) {
                Ok(metadata) if metadata.is_file() && Fingerprint::of(&metadata) == *expected => {
                    Ok(())
                }
                _ => Err(ArchiveError::new(
                    ErrorKind::SourceChanged,
                    "archive source changed after partial scan",
                )),
            }
        } else {
            Ok(())
        };
        if let Err(error) = &result
            && !matches!(error.kind, ErrorKind::Cancelled | ErrorKind::Deadline)
        {
            self.catalog.state = BrowserCatalogState::Failed;
            self.catalog.source_valid = false;
            self.catalog.error = Some(BrowserError::archive(error));
            self.source_failed = true;
        }
        result
    }
    fn prepare(
        &self,
        request: &crate::jobs::Preparation,
        generation: BrowserGeneration,
    ) -> crate::Result<crate::jobs::Prepared> {
        if self.catalog.state != BrowserCatalogState::Complete {
            return Err(ArchiveError::new(
                ErrorKind::Malformed,
                "extraction requires complete catalog and browser index",
            ));
        }
        if self
            .tree
            .rejected_prefix_records
            .iter()
            .any(|rejected| *rejected)
        {
            return Err(ArchiveError::new(
                ErrorKind::UnsafePath,
                "browser catalog contains rejected navigation prefixes",
            ));
        }
        let session = self.session.as_ref().ok_or_else(|| {
            ArchiveError::new(ErrorKind::Malformed, "no complete extraction session")
        })?;
        crate::jobs::expand(request, generation, session)
    }
    fn list(
        &mut self,
        shared: &Shared,
        pending: &Pending,
        options: &BrowserOptions,
    ) -> crate::Result<BrowserPage> {
        let control = Control::new(
            &pending.cancel,
            options.list_timeout,
            options.limits.total_bytes,
        );
        self.verify_source(shared, &control)?;
        let request = &pending.request;
        let mut page = shared.empty_page(request, self.catalog.clone(), self.format);
        let Some(&index) = self.tree.lookup.get(request.prefix.components()) else {
            page.error = Some(BrowserError::new(
                BrowserErrorKind::PrefixNotFound,
                "archive prefix is absent from the known catalog",
            ));
            return Ok(page);
        };
        let records = self
            .session
            .as_ref()
            .map_or(self.partial.as_slice(), |session| session.entries());
        self.tree.sort(index, records, &control)?;
        let children = &self.tree.directories[index].children;
        page.total_children = children.len();
        let mut bytes = page.prefix.bytes() + shared.source.as_os_str().len();
        for &node in children.iter().skip(request.offset).take(request.limit) {
            control.check()?;
            let node = self.tree.node(node, records, shared.generation);
            if bytes.saturating_add(node.bytes()) > MAX_BROWSER_PAGE_BYTES {
                break;
            }
            bytes += node.bytes();
            page.nodes.push(node);
        }
        let next = request.offset.saturating_add(page.nodes.len());
        page.next_offset = (next < children.len()).then_some(next);
        // Catch replacement while grouping/sorting as well as before the request.
        self.verify_source(shared, &control)?;
        page.catalog = self.catalog.clone();
        Ok(page)
    }
}

fn worker_main(shared: &Shared, options: &BrowserOptions) {
    if shared.stop.is_cancelled() {
        return;
    }
    let mut data = WorkerData::open(shared, options);
    loop {
        let (pending, preparation) = {
            let mut state = shared.lock();
            while state.pending.is_none() && state.preparation.is_none() && !state.closed {
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if let Some(request) = state.preparation.take() {
                (None, Some(request))
            } else if state.closed {
                break;
            } else {
                (state.pending.take(), None)
            }
        };
        if let Some(request) = preparation {
            #[cfg(test)]
            if let Some(gate) = shared.preparation_gate.lock().unwrap().take() {
                let mut ready = gate.0.lock().unwrap();
                while !*ready {
                    ready = gate.1.wait(ready).unwrap();
                }
            }
            let result = data.prepare(&request, shared.generation);
            request.complete(result);
            continue;
        }
        let pending = pending.unwrap();
        if pending.cancel.is_cancelled() {
            continue;
        }
        match data.list(shared, &pending, options) {
            Ok(page) => {
                shared.publish(&pending, page);
            }
            Err(_) if pending.cancel.is_cancelled() => {}
            Err(error) => {
                let mut page =
                    shared.empty_page(&pending.request, data.catalog.clone(), data.format);
                page.error = Some(BrowserError::archive(&error));
                shared.publish(&pending, page);
            }
        }
    }
    // Session, complete/partial records and derived indexes are dropped here.
}

fn sort_cancellable(
    nodes: &mut Vec<Node>,
    compare: impl Fn(Node, Node) -> Compare,
    control: &Control<'_>,
) -> crate::Result<()> {
    const CHUNK: usize = 256;
    for chunk in nodes.chunks_mut(CHUNK) {
        control.check()?;
        chunk.sort_unstable_by(|a, b| compare(*a, *b));
    }
    if nodes.len() <= CHUNK {
        return control.check();
    }
    let mut scratch = vec![Node::Record(0); nodes.len()];
    let mut width = CHUNK;
    while width < nodes.len() {
        control.check()?;
        for start in (0..nodes.len()).step_by(width.saturating_mul(2)) {
            let middle = start.saturating_add(width).min(nodes.len());
            let end = middle.saturating_add(width).min(nodes.len());
            let (mut left, mut right) = (start, middle);
            for (offset, slot) in scratch[start..end].iter_mut().enumerate() {
                if offset % CHUNK == 0 {
                    control.check()?;
                }
                if left < middle
                    && (right == end || compare(nodes[left], nodes[right]) != Compare::Greater)
                {
                    *slot = nodes[left];
                    left += 1;
                } else {
                    *slot = nodes[right];
                    right += 1;
                }
            }
        }
        std::mem::swap(nodes, &mut scratch);
        width = width.saturating_mul(2);
    }
    control.check()
}

fn record_node(entry: &ArchiveEntry) -> BrowserNode {
    let name = entry
        .components
        .as_ref()
        .and_then(|p| p.last())
        .map_or(entry.name.as_str(), String::as_str);
    let (display_name, name_truncated) = bounded_text(name);
    let issue = if entry.name.len() > PREFIX_BYTES {
        Some((
            BrowserIssueKind::UnsafeName,
            "decoded archive name exceeds 65535 UTF-8 bytes".into(),
        ))
    } else if let Some(message) = &entry.issue {
        Some((
            if message.starts_with("duplicate") {
                BrowserIssueKind::AmbiguousName
            } else {
                BrowserIssueKind::UnsafeName
            },
            message.clone(),
        ))
    } else if matches!(entry.kind, EntryKind::Symlink | EntryKind::Special) {
        Some((
            BrowserIssueKind::UnsupportedKind,
            "links and special archive entries are read-only".into(),
        ))
    } else if entry.kind == EntryKind::Directory && entry.uncompressed_bytes != 0 {
        Some((
            BrowserIssueKind::PayloadDirectory,
            "payload-bearing directory records cannot be extracted".into(),
        ))
    } else if !matches!(entry.compression, 0 | 8 | 99) {
        Some((
            BrowserIssueKind::UnsupportedCompression,
            format!(
                "ZIP compression method {} is unsupported",
                entry.compression
            ),
        ))
    } else {
        None
    };
    BrowserNode {
        id: ArchiveNodeId::Record(entry.id),
        display_name,
        name_truncated,
        kind: entry.kind,
        record_id: Some(entry.id),
        navigation: None,
        synthetic: false,
        compressed_bytes: Some(entry.compressed_bytes),
        uncompressed_bytes: Some(entry.uncompressed_bytes),
        compression: Some(entry.compression),
        encrypted: entry.encrypted,
        issue: issue.map(|(kind, text)| {
            let (message, message_truncated) = bounded_text(&text);
            BrowserIssue {
                kind,
                message,
                message_truncated,
            }
        }),
    }
}
fn bounded_text(text: &str) -> (String, bool) {
    let mut output = String::new();
    for c in text.chars() {
        let escaped = if c.is_control() {
            c.escape_default().to_string()
        } else {
            c.to_string()
        };
        if output.len() + escaped.len() > TEXT_BYTES {
            return (output, true);
        }
        output.push_str(&escaped);
    }
    (output, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Cursor, Write},
        sync::{atomic::AtomicUsize, mpsc},
        time::Instant,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".test-work")
                .join(format!(
                    "browser-unit-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn zip(&self) -> PathBuf {
            let mut bytes = Cursor::new(Vec::new());
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file("a", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"payload").unwrap();
            writer.finish().unwrap();
            let path = self.0.join("source.zip");
            fs::write(&path, bytes.into_inner()).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "archive worker did not settle");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn terminal(browser: &ArchiveBrowser, id: u64) -> Box<BrowserPage> {
        let mut result = None;
        wait_until(|| {
            if let Some(page) = browser.poll() {
                assert_eq!(page.request_id, id);
                if page.catalog.state != BrowserCatalogState::Opening {
                    result = Some(page);
                    return true;
                }
            }
            false
        });
        result.unwrap()
    }
    fn gate() -> Arc<(Mutex<bool>, Condvar)> {
        Arc::new((Mutex::new(false), Condvar::new()))
    }
    fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
    fn await_gate(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let mut ready = gate.0.lock().unwrap();
        while !*ready {
            ready = gate.1.wait(ready).unwrap();
        }
    }

    #[test]
    fn request_storm_has_one_worker_one_pending_slot_and_no_stale_delivery() {
        let fixture = Fixture::new();
        let gate = gate();
        let worker_gate = gate.clone();
        let starts = Arc::new(AtomicUsize::new(0));
        let worker_starts = starts.clone();
        let browser = ArchiveBrowser::spawn(BrowserOptions::new(fixture.zip()), move || {
            worker_starts.fetch_add(1, Ordering::Relaxed);
            await_gate(&worker_gate);
        })
        .unwrap();
        let old = browser.shared.lock().pending.take().unwrap();
        for id in 1..=5000 {
            browser.request_list(ListRequest::root(id, 0, 512)).unwrap();
        }
        {
            let state = browser.shared.lock();
            assert_eq!(state.pending.as_ref().unwrap().request.request_id, 5000);
            assert!(state.delivery.is_none());
        }
        let stale = browser
            .shared
            .empty_page(&old.request, BrowserCatalog::opening(), None);
        assert!(!browser.shared.publish(&old, stale));
        assert!(browser.poll().is_none());
        release(&gate);
        let page = terminal(&browser, 5000);
        assert_eq!(page.nodes.len(), 1);
        assert_eq!(starts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_while_worker_is_blocked_never_joins_on_the_caller() {
        let fixture = Fixture::new();
        let gate = gate();
        let worker_gate = gate.clone();
        let (started_send, started) = mpsc::channel();
        let browser = ArchiveBrowser::spawn(BrowserOptions::new(fixture.zip()), move || {
            started_send.send(()).unwrap();
            await_gate(&worker_gate);
        })
        .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let weak = Arc::downgrade(&browser.shared);
        let (dropped_send, dropped) = mpsc::channel();
        thread::spawn(move || {
            drop(browser);
            dropped_send.send(()).unwrap();
        });
        let returned = dropped.recv_timeout(Duration::from_secs(1));
        release(&gate);
        assert!(returned.is_ok(), "caller waited for blocked archive worker");
        wait_until(|| weak.upgrade().is_none());
    }

    #[test]
    fn terminal_cancel_stops_idle_worker_and_reclaims_the_source_handle() {
        let fixture = Fixture::new();
        let path = fixture.zip();
        let browser = ArchiveBrowser::open(BrowserOptions::new(path.clone())).unwrap();
        terminal(&browser, 0);
        assert!(!browser.is_worker_stopped());
        #[cfg(target_os = "linux")]
        let source_handles = || {
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|e| fs::read_link(e.ok()?.path()).ok())
                .filter(|p| p == &path)
                .count()
        };
        #[cfg(target_os = "linux")]
        assert!(source_handles() > 0);
        assert!(browser.cancel());
        wait_until(|| browser.is_worker_stopped());
        #[cfg(target_os = "linux")]
        assert_eq!(
            source_handles(),
            0,
            "catalog worker retained an archive handle after exit"
        );
    }

    #[test]
    fn incomplete_catalog_has_no_readable_engine_session() {
        let fixture = Fixture::new();
        let mut options = BrowserOptions::new(fixture.zip());
        options.limits.metadata_bytes = 0;
        let gate = gate();
        let worker_gate = gate.clone();
        let browser =
            ArchiveBrowser::spawn(options.clone(), move || await_gate(&worker_gate)).unwrap();
        let data = WorkerData::open(&browser.shared, &options);
        assert!(data.session.is_none());
        assert_eq!(data.catalog.state, BrowserCatalogState::LimitReached);
        drop(data);
        browser.cancel();
        release(&gate);
        wait_until(|| browser.is_worker_stopped());
    }

    #[test]
    fn cancelling_sort_preserves_all_nodes_without_publishing_partial_order() {
        let cancel = Cancellation::default();
        let control = Control::new(&cancel, Duration::from_secs(5), 0);
        let comparisons = AtomicUsize::new(0);
        let mut nodes: Vec<_> = (0..2000).rev().map(Node::Record).collect();
        let error = sort_cancellable(
            &mut nodes,
            |a, b| {
                if comparisons.fetch_add(1, Ordering::Relaxed) == 100 {
                    cancel.cancel();
                }
                a.cmp(&b)
            },
            &control,
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        nodes.sort_unstable();
        assert_eq!(nodes, (0..2000).map(Node::Record).collect::<Vec<_>>());
    }
    #[test]
    fn derived_prefix_validation_is_atomic_and_rejected_records_remain_visible() {
        let options = BrowserOptions::new(PathBuf::from("/unused.zip"));
        let cancel = Cancellation::default();
        let control = Control::new(&cancel, Duration::from_secs(5), 0);
        for components in [
            vec!["d".to_owned(); 257],
            vec!["x".repeat(65535)],
            vec!["safe".into(), "bad/name".into()],
            vec!["..".into()],
            vec!["bad\0name".into()],
        ] {
            let mut tree = Tree::empty();
            let error = tree.directory(&components, &options, &control).unwrap_err();
            assert_eq!(error.kind, ErrorKind::UnsafePath);
            assert_eq!(
                tree.directories.len(),
                1,
                "invalid target partially mutated the tree"
            );
            let record = ArchiveEntry {
                id: ArchiveEntryId {
                    session: 1,
                    ordinal: 0,
                    central_offset: 0,
                },
                raw_name: Vec::new(),
                name: components.join("/"),
                encoding: crate::NameEncoding::Utf8,
                kind: EntryKind::Directory,
                compressed_bytes: 0,
                uncompressed_bytes: 0,
                compression: 0,
                encrypted: false,
                issue: None,
                components: Some(components),
            };
            let mut permissive = options.clone();
            permissive.limits.path_components = 300;
            tree.build(std::slice::from_ref(&record), &permissive, &control)
                .unwrap();
            assert_eq!(tree.indexed_records, 1);
            assert_eq!(tree.directories.len(), 1);
            assert_eq!(tree.directories[0].children, vec![Node::Record(0)]);
            let node = tree.node(Node::Record(0), &[record], BrowserGeneration(1));
            assert!(node.navigation.is_none());
            assert_eq!(node.issue.unwrap().kind, BrowserIssueKind::UnsafeName);
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn accepted_preparation_drains_after_navigation_and_close_then_start_uses_original_lease() {
        let fixture = Fixture::new();
        let browser = ArchiveBrowser::open(BrowserOptions::new(fixture.zip())).unwrap();
        terminal(&browser, 0);
        let probe = browser.shared.session_probe.lock().unwrap().clone();
        let gate = gate();
        *browser.shared.preparation_gate.lock().unwrap() = Some(gate.clone());
        let jobs = crate::ExtractionJobs::new(crate::ExtractionJobOptions::default()).unwrap();
        jobs.prepare(&browser, crate::ExtractionSelection::All)
            .unwrap();
        browser.request_list(ListRequest::root(1, 0, 1)).unwrap();
        browser.request_list(ListRequest::root(2, 0, 1)).unwrap();
        browser.cancel();
        assert!(!browser.is_worker_stopped());
        release(&gate);
        let mut ticket = None;
        wait_until(|| {
            if let Some(page) = jobs.poll() {
                ticket = page.ticket;
            }
            ticket.is_some()
        });
        wait_until(|| browser.is_worker_stopped());
        assert_eq!(
            probe.strong_count(),
            1,
            "only the job actor must retain the session"
        );
        drop(browser);
        jobs.start(ticket.unwrap(), fixture.0.join("out"), None)
            .unwrap();
        let mut done = None;
        wait_until(|| {
            if let Some(page) = jobs.poll()
                && page.state == crate::ExtractionJobState::Terminal
            {
                done = Some(page);
            }
            done.is_some()
        });
        assert!(matches!(
            done.unwrap().outcome,
            Some(crate::ExtractionOutcome::Committed { .. })
        ));
        assert_eq!(
            probe.strong_count(),
            0,
            "session must be reclaimed before terminal delivery"
        );
        assert_eq!(fs::read(fixture.0.join("out/a")).unwrap(), b"payload");
    }

    #[test]
    fn abandoned_ticket_releases_the_only_session_lease_before_terminal_delivery() {
        let fixture = Fixture::new();
        let browser = ArchiveBrowser::open(BrowserOptions::new(fixture.zip())).unwrap();
        terminal(&browser, 0);
        let probe = browser.shared.session_probe.lock().unwrap().clone();
        let jobs = crate::ExtractionJobs::new(crate::ExtractionJobOptions::default()).unwrap();
        jobs.prepare(&browser, crate::ExtractionSelection::All)
            .unwrap();
        let mut ticket = None;
        wait_until(|| {
            if let Some(page) = jobs.poll() {
                ticket = page.ticket;
            }
            ticket.is_some()
        });
        browser.cancel();
        wait_until(|| browser.is_worker_stopped());
        assert_eq!(probe.strong_count(), 1);
        drop(ticket.take());
        let mut done = None;
        wait_until(|| {
            if let Some(page) = jobs.poll()
                && page.state == crate::ExtractionJobState::Terminal
            {
                done = Some(page);
            }
            done.is_some()
        });
        assert!(matches!(
            done.unwrap().outcome,
            Some(crate::ExtractionOutcome::Failed(crate::ExtractionFailure {
                kind: ErrorKind::Cancelled,
                ..
            }))
        ));
        assert_eq!(probe.strong_count(), 0);
        assert!(!fixture.0.join("out").exists());
    }
    #[test]
    fn job_cancellation_during_accepted_preparation_is_separate_from_browser_stop() {
        let fixture = Fixture::new();
        let browser = ArchiveBrowser::open(BrowserOptions::new(fixture.zip())).unwrap();
        terminal(&browser, 0);
        let gate = gate();
        *browser.shared.preparation_gate.lock().unwrap() = Some(gate.clone());
        let jobs = crate::ExtractionJobs::new(crate::ExtractionJobOptions::default()).unwrap();
        let id = jobs
            .prepare(&browser, crate::ExtractionSelection::All)
            .unwrap();
        jobs.cancel(id).unwrap();
        release(&gate);
        let mut done = None;
        wait_until(|| {
            if let Some(page) = jobs.poll()
                && page.state == crate::ExtractionJobState::Terminal
            {
                done = Some(page);
            }
            done.is_some()
        });
        assert!(matches!(
            done.unwrap().outcome,
            Some(crate::ExtractionOutcome::Failed(crate::ExtractionFailure {
                kind: ErrorKind::Cancelled,
                ..
            }))
        ));
        assert!(!browser.is_worker_stopped());
        browser.request_list(ListRequest::root(1, 0, 1)).unwrap();
        assert_eq!(terminal(&browser, 1).nodes.len(), 1);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
}
