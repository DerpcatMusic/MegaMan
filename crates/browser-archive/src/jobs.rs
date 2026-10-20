//! One bounded mutation actor. Only workers own expanded manifests/session leases.
use crate::{
    ArchiveBrowser, ArchiveEntryId, ArchiveError, ArchivePrefix, ArchiveSession, BrowserGeneration,
    Cancellation, Control, ErrorKind, Limits,
};
use std::{
    fmt, io,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub const MAX_EXTRACTION_ITEMS: usize = 512;
/// Maximum borrowed credential copied into an extraction job, in bytes.
pub const MAX_EXTRACTION_CREDENTIAL_BYTES: usize = 1024;
const INPUT_BYTES: usize = 1 << 20;
const PATH_BYTES: usize = u16::MAX as usize;
static NEXT_SERVICE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtractionJobId {
    service: u64,
    sequence: u64,
}
impl ExtractionJobId {
    /// Presentation sequence only; action identity also includes a private owner.
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}
#[derive(Clone, Debug)]
pub enum ExtractionItem {
    Record(ArchiveEntryId),
    Folder(ArchivePrefix),
}
#[derive(Clone, Debug)]
pub enum ExtractionSelection {
    All,
    Items {
        generation: BrowserGeneration,
        items: Vec<ExtractionItem>,
    },
}
#[derive(Clone, Debug)]
pub struct ExtractionJobOptions {
    pub limits: Limits,
    pub preparation_timeout: Duration,
    pub execution_timeout: Duration,
    pub manifest_records: usize,
    pub manifest_bytes: usize,
}
impl Default for ExtractionJobOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            preparation_timeout: Duration::from_secs(30),
            execution_timeout: Duration::from_secs(30 * 60),
            manifest_records: 1_000_000,
            manifest_bytes: 32 << 20,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractionSubmitError {
    Busy,
    Closed,
    NotReady,
    InvalidSelection,
    LimitExceeded,
    StaleJob,
    NotPrepared,
    NotTerminal,
    InvalidDestination,
    CredentialTooLong,
}
impl fmt::Display for ExtractionSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ExtractionSubmitError {}

/// Copies at most [`MAX_EXTRACTION_CREDENTIAL_BYTES`] into a fixed heap allocation.
/// No Clone/persistence.
/// Zeroizing erases this allocation on Drop; caller/backend/OS copies are outside
/// that guarantee. Never format the original caller input for diagnostics.
pub struct ExtractionCredential(Zeroizing<Box<[u8]>>);
impl ExtractionCredential {
    pub fn new(bytes: &[u8]) -> Result<Self, ExtractionSubmitError> {
        if bytes.len() > MAX_EXTRACTION_CREDENTIAL_BYTES {
            return Err(ExtractionSubmitError::CredentialTooLong);
        }
        // Finish allocation/shrinking while it contains only zeros. Copy the
        // secret after the fixed buffer is already protected, so conversion
        // from Vec to Box cannot leave a plaintext allocation behind.
        let mut credential = Zeroizing::new(vec![0; bytes.len()].into_boxed_slice());
        credential.copy_from_slice(bytes);
        Ok(Self(credential))
    }
}
impl fmt::Debug for ExtractionCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExtractionCredential([REDACTED])")
    }
}

/// Contains only bounded identity and weak control, never a manifest/session.
/// Abandonment cancels a prepared reservation; accepted start disarms Drop.
pub struct ExtractionTicket {
    id: ExtractionJobId,
    owner: Option<Weak<Shared>>,
}
impl ExtractionTicket {
    pub fn id(&self) -> ExtractionJobId {
        self.id
    }
}
impl fmt::Debug for ExtractionTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExtractionTicket").field(&self.id).finish()
    }
}
impl Drop for ExtractionTicket {
    fn drop(&mut self) {
        if let Some(shared) = self.owner.take().and_then(|owner| owner.upgrade()) {
            shared.cancel(self.id);
        }
    }
}
#[derive(Debug)]
pub struct ExtractionStartError {
    pub kind: ExtractionSubmitError,
    pub ticket: ExtractionTicket,
    pub credential: Option<ExtractionCredential>,
}
impl fmt::Display for ExtractionStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}
impl std::error::Error for ExtractionStartError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractionPhase {
    Reading,
    Syncing,
    Publishing,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractionJobState {
    Preparing,
    Prepared,
    Running(ExtractionPhase),
    Terminal,
}
#[derive(Clone, Debug)]
pub struct ExtractionFailure {
    pub kind: ErrorKind,
    pub io_kind: Option<io::ErrorKind>,
    pub raw_os_error: Option<i32>,
    pub message: String,
    pub message_truncated: bool,
    /// Exact native identities, independent of bounded presentation text.
    pub published: Option<PathBuf>,
    pub residue: Option<PathBuf>,
}
impl From<ArchiveError> for ExtractionFailure {
    fn from(error: ArchiveError) -> Self {
        let text = error.to_string();
        let mut end = text.len().min(1024);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            kind: error.kind,
            io_kind: error.io.as_ref().map(io::Error::kind),
            raw_os_error: error.io.as_ref().and_then(io::Error::raw_os_error),
            message: text[..end].to_owned(),
            message_truncated: end < text.len(),
            published: error.published,
            residue: error.residue,
        }
    }
}
#[derive(Clone, Debug)]
pub enum ExtractionOutcome {
    Committed {
        destination: PathBuf,
        entries: usize,
        bytes: u64,
    },
    Failed(ExtractionFailure),
}
#[derive(Debug)]
pub struct ExtractionJobSnapshot {
    pub id: ExtractionJobId,
    pub revision: u64,
    pub state: ExtractionJobState,
    pub source: Arc<PathBuf>,
    pub records: usize,
    pub declared_bytes: u64,
    /// Unverified decoded work; not a count of committed output bytes.
    pub decoded_bytes: u64,
    /// Becomes nonzero only after the entire selected batch verifies.
    pub verified_records: usize,
    pub ticket: Option<ExtractionTicket>,
    pub outcome: Option<ExtractionOutcome>,
}

pub(crate) struct Prepared {
    pub(crate) session: Arc<ArchiveSession>,
    pub(crate) ids: Vec<ArchiveEntryId>,
    pub(crate) declared_bytes: u64,
}
pub(crate) struct Preparation {
    pub(crate) selection: ExtractionSelection,
    pub(crate) generation: BrowserGeneration,
    pub(crate) options: ExtractionJobOptions,
    pub(crate) cancel: Arc<Cancellation>,
    pub(crate) deadline: Instant,
    id: ExtractionJobId,
    owner: Arc<Shared>,
    answered: bool,
}
impl Preparation {
    pub(crate) fn control(&self) -> Control<'_> {
        Control::new(
            &self.cancel,
            self.deadline.saturating_duration_since(Instant::now()),
            self.options.limits.total_bytes,
        )
    }
    pub(crate) fn complete(mut self, result: crate::Result<Prepared>) {
        self.answered = true;
        self.owner.deliver(self.id, result);
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        if !self.answered {
            self.owner.deliver(
                self.id,
                Err(ArchiveError::new(
                    ErrorKind::RecoveryRequired,
                    "archive preparation worker stopped",
                )),
            );
        }
    }
}
struct Start {
    destination: PathBuf,
    credential: Option<ExtractionCredential>,
}
struct Active {
    id: ExtractionJobId,
    cancel: Arc<Cancellation>,
    counter: Arc<AtomicU64>,
    source: Arc<PathBuf>,
    state: ExtractionJobState,
    records: usize,
    declared_bytes: u64,
    verified_records: usize,
    outcome: Option<ExtractionOutcome>,
    revision: u64,
    polled: Option<(u64, u64)>,
    ticket_issued: bool,
}
struct State {
    active: Option<Active>,
    incoming: Option<crate::Result<Prepared>>,
    start: Option<Start>,
    acknowledged: bool,
    closed: bool,
    sequence: u64,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stopped: AtomicBool,
    service: u64,
    #[cfg(test)]
    phase_hook: Mutex<Option<PhaseHook>>,
}
#[cfg(test)]
type PhaseHook = Arc<dyn Fn(ExtractionPhase, &std::path::Path) + Send + Sync>;
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn deliver(&self, id: ExtractionJobId, result: crate::Result<Prepared>) {
        let mut state = self.lock();
        if !state.closed
            && state.active.as_ref().is_some_and(|a| a.id == id)
            && state.incoming.is_none()
        {
            state.incoming = Some(result);
            self.wake.notify_one();
        }
        // Late/rejected results drop on the delivering browser worker.
    }
    fn cancel(&self, id: ExtractionJobId) -> bool {
        let state = self.lock();
        if let Some(active) = &state.active
            && active.id == id
        {
            if active.state != ExtractionJobState::Terminal {
                active.cancel.cancel();
                self.wake.notify_one();
            }
            true
        } else {
            false
        }
    }
}

/// Keep at operation-center scope. Drop cancels without joining. A terminal
/// result must be explicitly acknowledged before another job can be admitted.
pub struct ExtractionJobs {
    shared: Arc<Shared>,
    options: ExtractionJobOptions,
}
impl ExtractionJobs {
    pub fn new(options: ExtractionJobOptions) -> io::Result<Self> {
        let service = NEXT_SERVICE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("extraction service identity exhausted"))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                active: None,
                incoming: None,
                start: None,
                acknowledged: false,
                closed: false,
                sequence: 0,
            }),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            service,
            #[cfg(test)]
            phase_hook: Mutex::new(None),
        });
        let worker = shared.clone();
        let limits = options.clone();
        thread::Builder::new()
            .name("archive-extraction".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_main(&worker, &limits)
                }));
                let mut state = worker.lock();
                // Any unconsumed transfer/credential is reclaimed on this worker.
                let incoming = state.incoming.take();
                let command = state.start.take();
                if result.is_err() {
                    terminal(
                        &mut state,
                        Err(ArchiveError::new(
                            ErrorKind::RecoveryRequired,
                            "archive extraction worker stopped unexpectedly",
                        )),
                    );
                }
                state.closed = true;
                drop(state);
                drop(incoming);
                drop(command);
                worker.stopped.store(true, Ordering::Release);
            })?;
        Ok(Self { shared, options })
    }
    pub fn prepare(
        &self,
        browser: &ArchiveBrowser,
        selection: ExtractionSelection,
    ) -> Result<ExtractionJobId, ExtractionSubmitError> {
        if let ExtractionSelection::Items { items, .. } = &selection {
            if items.is_empty() {
                return Err(ExtractionSubmitError::InvalidSelection);
            }
            if items.len() > MAX_EXTRACTION_ITEMS {
                return Err(ExtractionSubmitError::LimitExceeded);
            }
            let bytes = items.iter().try_fold(0usize, |n, item| {
                n.checked_add(match item {
                    ExtractionItem::Record(_) => size_of::<ExtractionItem>(),
                    ExtractionItem::Folder(prefix) => {
                        size_of::<ExtractionItem>()
                            + prefix
                                .components()
                                .iter()
                                .map(|s| s.len() + size_of::<String>())
                                .sum::<usize>()
                    }
                })
            });
            if items.len() > MAX_EXTRACTION_ITEMS || bytes.is_none_or(|n| n > INPUT_BYTES) {
                return Err(ExtractionSubmitError::LimitExceeded);
            }
        }
        let mut state = self.shared.lock();
        if state.closed {
            return Err(ExtractionSubmitError::Closed);
        }
        if state.active.is_some() {
            return Err(ExtractionSubmitError::Busy);
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or(ExtractionSubmitError::LimitExceeded)?;
        let id = ExtractionJobId {
            service: self.shared.service,
            sequence: state.sequence,
        };
        let cancel = Arc::new(Cancellation::default());
        let request = Preparation {
            selection,
            generation: browser.generation(),
            options: self.options.clone(),
            cancel: cancel.clone(),
            deadline: Instant::now()
                .checked_add(self.options.preparation_timeout)
                .unwrap_or_else(Instant::now),
            id,
            owner: self.shared.clone(),
            answered: false,
        };
        // Browser admission does no I/O, and never locks this service in reverse.
        if let Err((error, mut rejected)) = browser.enqueue_preparation(Box::new(request)) {
            rejected.answered = true;
            return Err(error);
        }
        state.active = Some(Active {
            id,
            cancel,
            counter: Arc::new(AtomicU64::new(0)),
            source: Arc::new(browser.source().to_owned()),
            state: ExtractionJobState::Preparing,
            records: 0,
            declared_bytes: 0,
            verified_records: 0,
            outcome: None,
            revision: 0,
            polled: None,
            ticket_issued: false,
        });
        self.shared.wake.notify_one();
        Ok(id)
    }
    pub fn start(
        &self,
        mut ticket: ExtractionTicket,
        destination: PathBuf,
        credential: Option<ExtractionCredential>,
    ) -> Result<(), ExtractionStartError> {
        let kind = if !destination.is_absolute() || destination.as_os_str().len() > PATH_BYTES {
            Some(ExtractionSubmitError::InvalidDestination)
        } else if !ticket
            .owner
            .as_ref()
            .is_some_and(|owner| Weak::ptr_eq(owner, &Arc::downgrade(&self.shared)))
        {
            Some(ExtractionSubmitError::StaleJob)
        } else {
            let mut state = self.shared.lock();
            if state.closed {
                Some(ExtractionSubmitError::Closed)
            } else if let Some(active) = &mut state.active {
                if active.id != ticket.id {
                    Some(ExtractionSubmitError::StaleJob)
                } else if active.state != ExtractionJobState::Prepared || state.start.is_some() {
                    Some(ExtractionSubmitError::NotPrepared)
                } else {
                    ticket.owner = None;
                    state.start = Some(Start {
                        destination,
                        credential,
                    });
                    self.shared.wake.notify_one();
                    return Ok(());
                }
            } else {
                Some(ExtractionSubmitError::StaleJob)
            }
        };
        Err(ExtractionStartError {
            kind: kind.unwrap(),
            ticket,
            credential,
        })
    }
    pub fn poll(&self) -> Option<ExtractionJobSnapshot> {
        let mut state = self.shared.lock();
        let active = state.active.as_mut()?;
        let decoded = active.counter.load(Ordering::Acquire);
        if active.polled == Some((active.revision, decoded)) {
            return None;
        }
        if active
            .polled
            .is_some_and(|(_, previous)| previous != decoded)
        {
            active.revision += 1;
        }
        active.polled = Some((active.revision, decoded));
        let ticket = if active.state == ExtractionJobState::Prepared && !active.ticket_issued {
            active.ticket_issued = true;
            Some(ExtractionTicket {
                id: active.id,
                owner: Some(Arc::downgrade(&self.shared)),
            })
        } else {
            None
        };
        Some(ExtractionJobSnapshot {
            id: active.id,
            revision: active.revision,
            state: active.state,
            source: active.source.clone(),
            records: active.records,
            declared_bytes: active.declared_bytes,
            decoded_bytes: decoded,
            verified_records: active.verified_records,
            ticket,
            outcome: active.outcome.clone(),
        })
    }
    pub fn cancel(&self, id: ExtractionJobId) -> Result<(), ExtractionSubmitError> {
        if self.shared.cancel(id) {
            Ok(())
        } else {
            Err(ExtractionSubmitError::StaleJob)
        }
    }
    pub fn acknowledge(&self, id: ExtractionJobId) -> Result<(), ExtractionSubmitError> {
        let mut state = self.shared.lock();
        let active = state
            .active
            .as_ref()
            .ok_or(ExtractionSubmitError::StaleJob)?;
        if active.id != id {
            return Err(ExtractionSubmitError::StaleJob);
        }
        if active.state != ExtractionJobState::Terminal {
            return Err(ExtractionSubmitError::NotTerminal);
        }
        state.acknowledged = true;
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn is_worker_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::Acquire)
    }
}
impl Drop for ExtractionJobs {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.closed = true;
        if let Some(active) = &state.active {
            active.cancel.cancel();
        }
        self.shared.wake.notify_one();
    }
}
fn terminal(state: &mut State, result: crate::Result<crate::Commit>) {
    if let Some(active) = &mut state.active {
        active.state = ExtractionJobState::Terminal;
        active.revision += 1;
        active.outcome = Some(match result {
            Ok(commit) => {
                active.verified_records = commit.entries;
                ExtractionOutcome::Committed {
                    destination: commit.destination,
                    entries: commit.entries,
                    bytes: commit.bytes,
                }
            }
            Err(error) => ExtractionOutcome::Failed(error.into()),
        });
    }
}
fn worker_main(shared: &Shared, options: &ExtractionJobOptions) {
    let mut prepared: Option<Prepared> = None;
    loop {
        let mut state = shared.lock();
        if state.closed {
            break;
        }
        if state.acknowledged {
            debug_assert!(prepared.is_none());
            state.active.take();
            state.acknowledged = false;
        }
        if let Some(result) = state.incoming.take() {
            match result {
                Ok(manifest) => {
                    let active = state.active.as_mut().unwrap();
                    active.records = manifest.ids.len();
                    active.declared_bytes = manifest.declared_bytes;
                    active.state = ExtractionJobState::Prepared;
                    active.revision += 1;
                    prepared = Some(manifest);
                }
                Err(error) => terminal(&mut state, Err(error)),
            }
        }
        let cancelled = state
            .active
            .as_ref()
            .is_some_and(|a| a.cancel.is_cancelled() && a.state != ExtractionJobState::Terminal);
        if cancelled {
            // Preparation may still be running. Wait for its result before terminal
            // acknowledgement can admit a new job; no late lease is orphaned.
            if state.active.as_ref().unwrap().state == ExtractionJobState::Preparing {
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                drop(state);
                continue;
            }
            let command = state.start.take();
            drop(state);
            drop(command);
            drop(prepared.take());
            let mut state = shared.lock();
            terminal(
                &mut state,
                Err(ArchiveError::new(
                    ErrorKind::Cancelled,
                    "archive extraction cancelled",
                )),
            );
            continue;
        }
        if let Some(command) = state.start.take() {
            let manifest = prepared.take().unwrap();
            let active = state.active.as_mut().unwrap();
            active.state = ExtractionJobState::Running(ExtractionPhase::Reading);
            active.revision += 1;
            let cancel = active.cancel.clone();
            let counter = active.counter.clone();
            drop(state);
            let control = Control::new(
                &cancel,
                options.execution_timeout,
                options.limits.total_bytes,
            )
            .with_counter(counter);
            let result = (|| {
                let entries = manifest.session.selection_exact(&manifest.ids, &control)?;
                crate::transaction::extract_entries_new(
                    &manifest.session,
                    entries,
                    &command.destination,
                    command.credential.as_ref().map(|c| c.0.as_ref()),
                    &control,
                    &|phase| {
                        let mut state = shared.lock();
                        if let Some(active) = &mut state.active {
                            active.state = ExtractionJobState::Running(phase);
                            active.revision += 1;
                            if phase != ExtractionPhase::Reading {
                                active.verified_records = active.records;
                            }
                        }
                        drop(state);
                        #[cfg(test)]
                        if let Some(hook) = shared.phase_hook.lock().unwrap().clone() {
                            hook(phase, &command.destination);
                        }
                    },
                    Some(&options.limits),
                )
            })();
            drop(command);
            drop(manifest);
            terminal(&mut shared.lock(), result);
            continue;
        }
        state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        drop(state);
    }
    // Worker-local session/manifest, plus unconsumed transport, reclaim here.
}

pub(crate) fn expand(
    request: &Preparation,
    generation: BrowserGeneration,
    session: &Arc<ArchiveSession>,
) -> crate::Result<Prepared> {
    let control = request.control();
    control.check()?;
    if generation != request.generation {
        return Err(ArchiveError::new(
            ErrorKind::StaleEntry,
            "foreign browser generation",
        ));
    }
    session.check_source(&control)?;
    if session
        .directory_range
        .1
        .saturating_sub(session.directory_range.0)
        > request.options.limits.directory_bytes
    {
        return Err(ArchiveError::new(
            ErrorKind::LimitExceeded,
            "extraction central-directory byte budget",
        ));
    }
    let records = session.entries();
    if records.len() > request.options.limits.entries
        || session.metadata_charged > request.options.limits.metadata_bytes
    {
        return Err(ArchiveError::new(
            ErrorKind::LimitExceeded,
            "retained catalog exceeds extraction metadata policy",
        ));
    }
    for entry in records {
        control.check()?;
        if entry
            .path_components()
            .is_some_and(|path| path.len() > request.options.limits.path_components)
        {
            return Err(ArchiveError::new(
                ErrorKind::LimitExceeded,
                "retained catalog exceeds extraction path policy",
            ));
        }
    }
    let choice_bytes = records
        .len()
        .checked_mul(size_of::<bool>())
        .filter(|bytes| *bytes <= request.options.manifest_bytes)
        .ok_or_else(|| {
            ArchiveError::new(
                ErrorKind::LimitExceeded,
                "extraction selection-index admission budget",
            )
        })?;
    let mut chosen = vec![false; records.len()];
    match &request.selection {
        ExtractionSelection::All => chosen.fill(true),
        ExtractionSelection::Items {
            generation: selected_generation,
            items,
        } => {
            if *selected_generation != generation {
                return Err(ArchiveError::new(
                    ErrorKind::StaleEntry,
                    "selection belongs to a different browser generation",
                ));
            }
            for item in items {
                control.check()?;
                match item {
                    ExtractionItem::Record(id) => {
                        session.entry(*id)?;
                        chosen[id.ordinal() as usize] = true;
                    }
                    ExtractionItem::Folder(prefix) => {
                        let mut found = prefix.components().is_empty();
                        for (index, entry) in records.iter().enumerate() {
                            control.check()?;
                            if let Some(path) = entry.path_components()
                                && path.starts_with(prefix.components())
                            {
                                // A regular file with the same path is not a logical directory.
                                if path.len() > prefix.components().len()
                                    || entry.kind == crate::EntryKind::Directory
                                {
                                    chosen[index] = true;
                                    found = true;
                                }
                            }
                        }
                        if !found {
                            return Err(ArchiveError::new(
                                ErrorKind::UnsafePath,
                                "selected logical directory is absent",
                            ));
                        }
                    }
                }
            }
        }
    }
    let mut ids = Vec::new();
    let mut declared_bytes = 0u64;
    for (entry, chosen) in records.iter().zip(chosen) {
        control.check()?;
        if !chosen {
            continue;
        }
        // Conservative expanded-ID/hash/preflight admission, not exact RSS.
        if ids.len() >= request.options.manifest_records
            || choice_bytes.saturating_add(ids.len().saturating_add(1).saturating_mul(128))
                > request.options.manifest_bytes
            || ids.len() >= request.options.limits.entries
        {
            return Err(ArchiveError::new(
                ErrorKind::LimitExceeded,
                "extraction manifest admission budget",
            ));
        }
        crate::check_size(
            entry.uncompressed_bytes,
            entry.compressed_bytes,
            &request.options.limits,
        )?;
        if entry
            .path_components()
            .is_some_and(|p| p.len() > request.options.limits.path_components)
        {
            return Err(ArchiveError::new(
                ErrorKind::LimitExceeded,
                "extraction path-component budget",
            ));
        }
        if !matches!(entry.compression, 0 | 8 | 99) {
            return Err(ArchiveError::new(
                ErrorKind::Unsupported,
                "selected ZIP compression method is unsupported",
            ));
        }
        declared_bytes = declared_bytes
            .checked_add(entry.uncompressed_bytes)
            .filter(|n| *n <= request.options.limits.total_bytes)
            .ok_or_else(|| {
                ArchiveError::new(ErrorKind::LimitExceeded, "extraction total-byte budget")
            })?;
        ids.push(entry.id);
    }
    // Global invalid/duplicate-name rejection happens even for an empty or safe
    // selection; no malformed child is silently omitted by folder expansion.
    let entries = session.selection_exact(&ids, &control)?;
    crate::names::check_tree_control(
        entries.iter().map(|entry| {
            (
                entry.components.as_ref().unwrap().as_slice(),
                entry.kind == crate::EntryKind::Directory,
            )
        }),
        request
            .options
            .limits
            .metadata_bytes
            .min(session.limits().metadata_bytes),
        Some(&control),
    )?;
    session.check_source(&control)?;
    Ok(Prepared {
        session: session.clone(),
        ids,
        declared_bytes,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Cursor, Write},
        sync::mpsc,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".test-work")
                .join(format!(
                    "job-native-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn browser(&self) -> ArchiveBrowser {
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            zip.start_file(
                "a",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(b"payload").unwrap();
            let path = self.0.join("source.zip");
            fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
            let browser = ArchiveBrowser::open(crate::BrowserOptions::new(path)).unwrap();
            wait(|| {
                browser
                    .poll()
                    .filter(|p| p.catalog.state == crate::BrowserCatalogState::Complete)
            });
            browser
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn wait<T>(mut take: impl FnMut() -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = take() {
                return value;
            }
            assert!(Instant::now() < end, "job worker timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn ticket(jobs: &ExtractionJobs) -> ExtractionTicket {
        wait(|| jobs.poll().and_then(|p| p.ticket))
    }
    fn done(jobs: &ExtractionJobs) -> ExtractionJobSnapshot {
        wait(|| {
            jobs.poll()
                .filter(|p| p.state == ExtractionJobState::Terminal)
        })
    }
    fn error(snapshot: ExtractionJobSnapshot) -> ExtractionFailure {
        match snapshot.outcome.unwrap() {
            ExtractionOutcome::Failed(error) => error,
            _ => panic!("unexpected commit"),
        }
    }

    #[test]
    fn progress_is_bounded_coalesced_and_cancel_during_sync_never_publishes() {
        let f = Fixture::new();
        let browser = f.browser();
        let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
        let id = jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        let (arrived_send, arrived) = mpsc::sync_channel(1);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(move |phase, _| {
            if phase == ExtractionPhase::Syncing {
                arrived_send.send(()).unwrap();
                let mut ready = worker_gate.0.lock().unwrap();
                while !*ready {
                    ready = worker_gate.1.wait(ready).unwrap();
                }
            }
        }));
        jobs.start(ticket(&jobs), f.0.join("out"), None).unwrap();
        arrived.recv_timeout(Duration::from_secs(10)).unwrap();
        let progress = jobs.poll().unwrap();
        assert_eq!(
            progress.state,
            ExtractionJobState::Running(ExtractionPhase::Syncing)
        );
        assert_eq!(
            (
                progress.records,
                progress.verified_records,
                progress.decoded_bytes
            ),
            (1, 1, 7)
        );
        assert!(
            jobs.poll().is_none(),
            "unchanged progress was queued repeatedly"
        );
        browser
            .request_list(crate::ListRequest::root(1, 0, 1))
            .unwrap();
        browser.cancel();
        jobs.cancel(id).unwrap();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_one();
        let failure = error(done(&jobs));
        assert_eq!(failure.kind, ErrorKind::Cancelled);
        assert!(failure.published.is_none() && failure.residue.is_none());
        assert!(!f.0.join("out").exists());
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }

    #[test]
    fn post_publication_sync_failure_preserves_exact_native_published_identity() {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let browser = f.browser();
        let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
        *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(|phase, _| {
            if phase == ExtractionPhase::Publishing {
                crate::transaction::inject_parent_sync_failure();
            }
        }));
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        let destination = f.0.join(std::ffi::OsString::from_vec(b"out-\xff".to_vec()));
        jobs.start(ticket(&jobs), destination.clone(), None)
            .unwrap();
        let failure = error(done(&jobs));
        assert_eq!(failure.kind, ErrorKind::Io);
        assert_eq!(failure.raw_os_error, Some(5));
        assert_eq!(failure.published, Some(destination.clone()));
        assert!(failure.residue.is_none());
        assert_eq!(fs::read(destination.join("a")).unwrap(), b"payload");
    }

    #[test]
    fn partial_disk_full_cleans_output_and_external_residue_is_reported_without_deleting_it() {
        for residue in [false, true] {
            let f = Fixture::new();
            let browser = f.browser();
            let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
            *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(move |phase, destination| {
                if phase == ExtractionPhase::Reading {
                    if residue {
                        let stage = fs::read_dir(destination.parent().unwrap())
                            .unwrap()
                            .filter_map(Result::ok)
                            .find(|e| {
                                e.file_name()
                                    .to_string_lossy()
                                    .starts_with(".megaman-archive-")
                            })
                            .unwrap()
                            .path();
                        fs::write(stage.join("external"), b"keep").unwrap();
                    }
                    crate::transaction::inject_extraction_write_failure();
                }
            }));
            jobs.prepare(&browser, ExtractionSelection::All).unwrap();
            jobs.start(ticket(&jobs), f.0.join("out"), None).unwrap();
            let failure = error(done(&jobs));
            assert_eq!(failure.kind, ErrorKind::Io);
            assert_eq!(failure.raw_os_error, Some(28));
            assert!(failure.published.is_none());
            assert!(!f.0.join("out").exists());
            if residue {
                let path = failure.residue.unwrap();
                assert_eq!(path.parent(), Some(f.0.as_path()));
                assert_eq!(fs::read(path.join("external")).unwrap(), b"keep");
            } else {
                assert!(failure.residue.is_none());
                assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
            }
        }
    }

    #[test]
    fn racing_destination_before_publish_is_never_replaced_and_late_cancel_cannot_undo_success() {
        for conflict in [false, true] {
            let f = Fixture::new();
            let browser = f.browser();
            let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
            *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(move |phase, destination| {
                if conflict && phase == ExtractionPhase::Publishing {
                    fs::create_dir(destination).unwrap();
                    fs::write(destination.join("sentinel"), b"keep").unwrap();
                }
            }));
            let id = jobs.prepare(&browser, ExtractionSelection::All).unwrap();
            jobs.start(ticket(&jobs), f.0.join("out"), None).unwrap();
            let snapshot = done(&jobs);
            if conflict {
                assert_eq!(error(snapshot).kind, ErrorKind::Conflict);
                assert_eq!(fs::read(f.0.join("out/sentinel")).unwrap(), b"keep");
            } else {
                assert!(matches!(
                    snapshot.outcome,
                    Some(ExtractionOutcome::Committed { .. })
                ));
                jobs.cancel(id).unwrap();
                assert!(jobs.poll().is_none());
                assert_eq!(fs::read(f.0.join("out/a")).unwrap(), b"payload");
            }
        }
    }
    #[test]
    fn accepted_running_job_survives_browser_close_and_releases_the_session_off_caller() {
        let f = Fixture::new();
        let browser = f.browser();
        let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
        let (arrived_send, arrived) = mpsc::sync_channel(1);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(move |phase, _| {
            if phase == ExtractionPhase::Reading {
                arrived_send.send(()).unwrap();
                let mut ready = worker_gate.0.lock().unwrap();
                while !*ready {
                    ready = worker_gate.1.wait(ready).unwrap();
                }
            }
        }));
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        jobs.start(ticket(&jobs), f.0.join("out"), None).unwrap();
        arrived.recv_timeout(Duration::from_secs(10)).unwrap();
        browser
            .request_list(crate::ListRequest::root(1, 0, 1))
            .unwrap();
        browser.cancel();
        wait(|| browser.is_worker_stopped().then_some(()));
        drop(browser);
        assert!(matches!(
            jobs.poll().unwrap().state,
            ExtractionJobState::Running(_)
        ));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_one();
        assert!(matches!(
            done(&jobs).outcome,
            Some(ExtractionOutcome::Committed { .. })
        ));
        assert_eq!(fs::read(f.0.join("out/a")).unwrap(), b"payload");
    }

    #[test]
    fn dropping_job_service_while_blocked_returns_without_join_and_cleans_on_worker() {
        let f = Fixture::new();
        let browser = f.browser();
        let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
        let (arrived_send, arrived) = mpsc::sync_channel(1);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(move |phase, _| {
            if phase == ExtractionPhase::Reading {
                arrived_send.send(()).unwrap();
                let mut ready = worker_gate.0.lock().unwrap();
                while !*ready {
                    ready = worker_gate.1.wait(ready).unwrap();
                }
            }
        }));
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        jobs.start(
            ticket(&jobs),
            f.0.join("out"),
            Some(ExtractionCredential::new(b"secret").unwrap()),
        )
        .unwrap();
        arrived.recv_timeout(Duration::from_secs(10)).unwrap();
        let shared = jobs.shared.clone();
        let (sent, dropped) = mpsc::sync_channel(1);
        thread::spawn(move || {
            drop(jobs);
            sent.send(()).unwrap();
        });
        let returned = dropped.recv_timeout(Duration::from_secs(1));
        assert!(!shared.stopped.load(Ordering::Acquire));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_one();
        assert!(returned.is_ok(), "job Drop joined a blocked worker");
        wait(|| shared.stopped.load(Ordering::Acquire).then_some(()));
        assert!(shared.lock().start.is_none());
        assert!(shared.lock().incoming.is_none());
        assert!(!f.0.join("out").exists());
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }

    #[test]
    fn replacement_after_staging_begins_fails_source_check_without_publishing() {
        let f = Fixture::new();
        let browser = f.browser();
        let jobs = ExtractionJobs::new(ExtractionJobOptions::default()).unwrap();
        let source = browser.source().to_owned();
        *jobs.shared.phase_hook.lock().unwrap() = Some(Arc::new(move |phase, _| {
            if phase == ExtractionPhase::Reading {
                let replacement = source.with_extension("replacement");
                fs::copy(&source, &replacement).unwrap();
                fs::rename(replacement, &source).unwrap();
            }
        }));
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        jobs.start(ticket(&jobs), f.0.join("out"), None).unwrap();
        let failure = error(done(&jobs));
        assert_eq!(failure.kind, ErrorKind::SourceChanged);
        assert!(failure.published.is_none() && failure.residue.is_none());
        assert!(!f.0.join("out").exists());
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }
}
