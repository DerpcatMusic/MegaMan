//! One persistent copy worker; no global application queue or batch atomicity.
//! Mutations are never latest-replaced. A terminal must be acknowledged before
//! another job is accepted. See README for bounded caller-owned input/output.
use crate::*;
use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

pub const MAX_ROOTS: usize = 4096;
pub const MAX_PATH_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_NAME_WORK_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionOwner(u64);
static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn allocate_owner(next: &std::sync::atomic::AtomicU64) -> io::Result<SessionOwner> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    })
    .map(SessionOwner)
    .map_err(|_| io::Error::other("copy session owner identities exhausted"))
}

/// Opaque process-local identity: owner plus checked per-owner sequence.
/// Diagnostic sequence numbers cannot be converted back into action identities.
/// ```compile_fail
/// use browser_operations::session::JobId;
/// let forged: JobId = 1_u64.into();
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobId {
    owner: SessionOwner,
    sequence: u64,
}
impl JobId {
    /// Per-session diagnostic sequence only; never use this to compare, persist
    /// or reconstruct action identities. Compare/retain the complete JobId.
    pub fn get(self) -> u64 {
        self.sequence
    }
}
#[derive(Debug)]
pub struct CopyBatch {
    pub sources: Vec<PathBuf>,
    pub destination_folder: PathBuf,
    pub limits: CopyLimits,
    pub options: CopyOptions,
}
impl CopyBatch {
    pub fn new(sources: Vec<PathBuf>, destination_folder: PathBuf) -> Self {
        Self {
            sources,
            destination_folder,
            limits: CopyLimits::default(),
            options: CopyOptions::default(),
        }
    }
    pub fn with_options(mut self, options: CopyOptions) -> Self {
        self.options = options;
        self
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitErrorKind {
    Busy,
    Closed,
    IdExhausted,
    AdmissionLimit,
}
/// Rejected input ownership is returned, never silently dropped on this call.
#[derive(Debug)]
pub struct SubmitError {
    pub kind: SubmitErrorKind,
    pub batch: CopyBatch,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    WrongJob,
    NotTerminal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchPhase {
    Preflight,
    Planning,
    Copying,
    Verifying,
    BeforeCommit,
    MetadataApplied,
    ContentPublished,
    RootCommitted,
}
#[derive(Clone, Copy, Debug)]
pub struct CopyProgress {
    pub id: JobId,
    pub phase: BatchPhase,
    pub root_index: usize,
    pub roots: usize,
    pub committed_roots: usize,
    pub planned_entries: usize,
    pub planned_bytes: u64,
    pub planned_name_bytes: usize,
    pub copied_bytes: u64,
    pub verified_bytes: u64,
    pub root: Option<Progress>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchStop {
    Complete,
    Failed,
    Cancelled,
    CommittedUncertain,
    RecoveryRequired,
}
#[derive(Debug)]
pub enum RootOutcome {
    NotStarted,
    Committed(CommitReceipt),
    CommittedUncertain(CommitReceipt),
    Cancelled {
        cleanup: Cleanup,
    },
    Failed {
        error: OperationError,
        cleanup: Cleanup,
    },
}
#[derive(Debug)]
pub struct RootReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub outcome: RootOutcome,
}
#[derive(Debug)]
pub struct BatchOutcome {
    pub id: JobId,
    pub stop: BatchStop,
    pub committed_roots: usize,
    pub roots: Vec<RootReport>,
    pub preflight_error: Option<OperationError>,
    /// Accepted plan work, including a later failed/cancelled execution.
    pub charged_entries: usize,
    pub charged_bytes: u64,
    pub charged_name_bytes: usize,
    #[cfg(test)]
    drop_thread: Mutex<Option<std::sync::mpsc::Sender<thread::ThreadId>>>,
}
#[cfg(test)]
impl Drop for BatchOutcome {
    fn drop(&mut self) {
        if let Some(sender) = self.drop_thread.get_mut().unwrap().take() {
            let _ = sender.send(thread::current().id());
        }
    }
}
struct Job {
    id: JobId,
    batch: CopyBatch,
    cancel: Cancellation,
}
struct State {
    next_id: Option<u64>,
    pending: Option<Job>,
    active: Option<(JobId, Cancellation)>,
    progress: Option<CopyProgress>,
    terminal: Option<Arc<BatchOutcome>>,
    acknowledge: bool,
    stop: bool,
}
#[cfg(test)]
type TestHook = Box<dyn FnMut(CopyProgress) + Send>;
struct Shared {
    owner: SessionOwner,
    state: Mutex<State>,
    thread: OnceLock<thread::Thread>,
    #[cfg(test)]
    hook: Mutex<Option<TestHook>>,
}
impl Shared {
    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

pub struct CopySession {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    terminal: Option<Arc<BatchOutcome>>,
}
impl CopySession {
    pub fn new() -> io::Result<Self> {
        let shared = Arc::new(Shared {
            owner: allocate_owner(&NEXT_OWNER)?,
            state: Mutex::new(State {
                next_id: Some(1),
                pending: None,
                active: None,
                progress: None,
                terminal: None,
                acknowledge: false,
                stop: false,
            }),
            thread: OnceLock::new(),
            #[cfg(test)]
            hook: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("browser-copy".into())
            .spawn(move || worker(worker_shared))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            terminal: None,
        })
    }
    /// Memory-only admission. Ownership passes to the worker only on success.
    /// Counts/native bytes are bounded; lexical/duplicate checks run on worker.
    pub fn submit(&mut self, batch: CopyBatch) -> std::result::Result<JobId, SubmitError> {
        let mut state = self.shared.state.lock().expect("copy state mutex poisoned");
        let rejected = if state.stop {
            Some(SubmitErrorKind::Closed)
        } else if state.active.is_some() {
            Some(SubmitErrorKind::Busy)
        } else if state.next_id.is_none() {
            Some(SubmitErrorKind::IdExhausted)
        } else if !admitted(&batch) {
            Some(SubmitErrorKind::AdmissionLimit)
        } else {
            None
        };
        if let Some(kind) = rejected {
            return Err(SubmitError { kind, batch });
        }
        let id = JobId {
            owner: self.shared.owner,
            sequence: state.next_id.unwrap(),
        };
        state.next_id = id.sequence.checked_add(1);
        let cancel = Cancellation::default();
        state.active = Some((id, cancel.clone()));
        state.pending = Some(Job { id, batch, cancel });
        drop(state);
        self.shared.wake();
        Ok(id)
    }
    pub fn cancel(&self, id: JobId) -> std::result::Result<(), ControlError> {
        if id.owner != self.shared.owner {
            return Err(ControlError::WrongJob);
        }
        let state = self.shared.state.lock().expect("copy state mutex poisoned");
        let (_, cancel) = state
            .active
            .as_ref()
            .filter(|(active, _)| *active == id)
            .ok_or(ControlError::WrongJob)?;
        cancel.cancel();
        drop(state);
        self.shared.wake();
        Ok(())
    }
    /// Take latest fixed-size progress and install a borrowed terminal view.
    /// No catalog/path clone or terminal destruction occurs in this call.
    pub fn poll(&mut self) -> Option<CopyProgress> {
        let mut state = self.shared.state.try_lock().ok()?;
        if self.terminal.is_none() && !state.acknowledge {
            self.terminal = state.terminal.clone();
        }
        state.progress.take()
    }
    pub fn terminal(&self) -> Option<&BatchOutcome> {
        self.terminal.as_deref()
    }
    /// Only the matching completed job may release the Busy gate. The worker
    /// consumes this acknowledgement asynchronously before accepting next work.
    pub fn acknowledge(&mut self, id: JobId) -> std::result::Result<(), ControlError> {
        if id.owner != self.shared.owner {
            return Err(ControlError::WrongJob);
        }
        if !self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.id == id)
        {
            return Err(
                if self
                    .shared
                    .state
                    .lock()
                    .unwrap()
                    .active
                    .as_ref()
                    .is_some_and(|(active, _)| *active == id)
                {
                    ControlError::NotTerminal
                } else {
                    ControlError::WrongJob
                },
            );
        }
        // Clear the private UI lease BEFORE allowing worker destruction.
        self.terminal = None;
        let mut state = self.shared.state.lock().expect("copy state mutex poisoned");
        if state
            .terminal
            .as_ref()
            .is_none_or(|terminal| terminal.id != id)
        {
            return Err(ControlError::WrongJob);
        }
        state.acknowledge = true;
        drop(state);
        self.shared.wake();
        Ok(())
    }
}
impl Drop for CopySession {
    fn drop(&mut self) {
        self.terminal = None;
        let mut state = self.shared.state.lock().expect("copy state mutex poisoned");
        state.stop = true;
        if let Some((_, cancel)) = &state.active {
            cancel.cancel();
        }
        drop(state);
        self.shared.wake();
        // Never wait for a potentially blocked read/write/sync syscall.
        drop(self.worker.take());
    }
}

fn native_bytes(path: &Path) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count().saturating_mul(2)
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().as_encoded_bytes().len()
    }
}
fn admitted(batch: &CopyBatch) -> bool {
    if batch.sources.is_empty()
        || batch.sources.len() > MAX_ROOTS
        || batch.limits.max_entries > MAX_ENTRIES
        || batch.limits.max_name_bytes > MAX_NAME_WORK_BYTES
    {
        return false;
    }
    let folder = native_bytes(&batch.destination_folder);
    let mut bytes = folder;
    for source in &batch.sources {
        let leaf = source
            .file_name()
            .map_or(0, |leaf| native_bytes(Path::new(leaf)));
        let Some(total) = bytes
            .checked_add(native_bytes(source))
            .and_then(|n| n.checked_add(folder))
            .and_then(|n| n.checked_add(if cfg!(windows) { 2 } else { 1 }))
            .and_then(|n| n.checked_add(leaf))
        else {
            return false;
        };
        bytes = total;
        if bytes > MAX_PATH_BYTES {
            return false;
        }
    }
    true
}
fn lexical(path: &Path, allow_root: bool) -> bool {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return false;
    }
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(part)
            }
            _ => return false,
        }
    }
    normalized.as_os_str() == path.as_os_str() && (allow_root || path.file_name().is_some())
}
fn preflight(batch: CopyBatch, id: JobId) -> (BatchOutcome, CopyLimits, CopyOptions) {
    let limits = batch.limits;
    let options = batch.options;
    let mut result = BatchOutcome {
        id,
        stop: BatchStop::Complete,
        committed_roots: 0,
        roots: Vec::with_capacity(batch.sources.len()),
        preflight_error: None,
        charged_entries: 0,
        charged_bytes: 0,
        charged_name_bytes: 0,
        #[cfg(test)]
        drop_thread: Mutex::new(None),
    };
    let mut names = BTreeSet::new();
    if !lexical(&batch.destination_folder, true) {
        result.preflight_error = Some(OperationError::new(
            ErrorKind::InvalidPath,
            "batch preflight",
            &batch.destination_folder,
            "destination folder must be absolute and normalized",
        ));
    }
    for source in batch.sources {
        let leaf = source.file_name().map(|name| name.to_os_string());
        let destination = leaf.as_ref().map_or_else(
            || batch.destination_folder.clone(),
            |leaf| batch.destination_folder.join(leaf),
        );
        if result.preflight_error.is_none() {
            if !lexical(&source, false) {
                result.preflight_error = Some(OperationError::new(
                    ErrorKind::InvalidPath,
                    "batch preflight",
                    &source,
                    "source must be absolute, normalized and have a native leaf",
                ));
            } else if !names.insert(leaf.unwrap()) {
                result.preflight_error = Some(OperationError::new(
                    ErrorKind::DestinationConflict,
                    "batch preflight",
                    &destination,
                    "duplicate destination basename in batch",
                ));
            }
        }
        result.roots.push(RootReport {
            source,
            destination,
            outcome: RootOutcome::NotStarted,
        });
    }
    if result.preflight_error.is_some() {
        result.stop = BatchStop::Failed;
    }
    (result, limits, options)
}
fn worker(shared: Arc<Shared>) {
    let _ = shared.thread.set(thread::current());
    loop {
        let (job, retired, stop) = {
            let mut state = shared.state.lock().expect("copy state mutex poisoned");
            let retired = if state.acknowledge || state.stop {
                state.acknowledge = false;
                state.active = None;
                state.progress = None;
                state.terminal.take()
            } else {
                None
            };
            (state.pending.take(), retired, state.stop)
        };
        drop(retired);
        if stop {
            drop(job);
            return;
        }
        let Some(job) = job else {
            thread::park();
            continue;
        };
        let terminal = run_job(&shared, job);
        let mut state = shared.state.lock().expect("copy state mutex poisoned");
        // Worker retains the strong terminal ownership until ack/close. Public
        // callers borrow only; no raw Arc or cloneable terminal handle escapes.
        state.terminal = Some(Arc::new(terminal));
    }
}
fn publish(shared: &Shared, progress: CopyProgress) {
    #[cfg(test)]
    if let Some(hook) = shared.hook.lock().unwrap().as_mut() {
        hook(progress);
    }
    shared
        .state
        .lock()
        .expect("copy state mutex poisoned")
        .progress = Some(progress);
}
fn run_job(shared: &Shared, job: Job) -> BatchOutcome {
    let (mut terminal, limits, options) = preflight(job.batch, job.id);
    let mut progress = CopyProgress {
        id: job.id,
        phase: BatchPhase::Preflight,
        root_index: 0,
        roots: terminal.roots.len(),
        committed_roots: 0,
        planned_entries: 0,
        planned_bytes: 0,
        planned_name_bytes: 0,
        copied_bytes: 0,
        verified_bytes: 0,
        root: None,
    };
    publish(shared, progress);
    if terminal.preflight_error.is_some() {
        return terminal;
    }
    for index in 0..terminal.roots.len() {
        if job.cancel.is_cancelled() {
            terminal.stop = BatchStop::Cancelled;
            break;
        }
        progress.root_index = index;
        progress.root = None;
        progress.phase = BatchPhase::Planning;
        publish(shared, progress);
        let remaining = CopyLimits {
            max_entries: limits
                .max_entries
                .checked_sub(terminal.charged_entries)
                .unwrap(),
            max_bytes: limits
                .max_bytes
                .checked_sub(terminal.charged_bytes)
                .unwrap(),
            max_name_bytes: limits
                .max_name_bytes
                .checked_sub(terminal.charged_name_bytes)
                .unwrap(),
            ..limits.clone()
        };
        let report = &terminal.roots[index];
        let plan = match plan_copy_new_with_options(
            report.source.clone(),
            report.destination.clone(),
            remaining,
            options,
            &job.cancel,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                if error.kind == ErrorKind::Cancelled {
                    terminal.roots[index].outcome = RootOutcome::Cancelled {
                        cleanup: Cleanup::Clean,
                    };
                    terminal.stop = BatchStop::Cancelled;
                } else {
                    terminal.roots[index].outcome = RootOutcome::Failed {
                        error,
                        cleanup: Cleanup::Clean,
                    };
                    terminal.stop = BatchStop::Failed;
                }
                break;
            }
        };
        // The engine admitted this complete plan under the checked remaining
        // limits. Charge it even when execution later fails or cancels.
        let charged = terminal
            .charged_entries
            .checked_add(plan.summary().entries)
            .filter(|n| *n <= limits.max_entries)
            .zip(
                terminal
                    .charged_bytes
                    .checked_add(plan.summary().bytes)
                    .filter(|n| *n <= limits.max_bytes),
            )
            .zip(
                terminal
                    .charged_name_bytes
                    .checked_add(plan.summary().name_bytes)
                    .filter(|n| *n <= limits.max_name_bytes),
            );
        let Some(((entries, bytes), names)) = charged else {
            terminal.roots[index].outcome = RootOutcome::Failed {
                error: OperationError::new(
                    ErrorKind::LimitExceeded,
                    "batch charge",
                    &terminal.roots[index].source,
                    "aggregate plan budget exceeded",
                ),
                cleanup: Cleanup::Clean,
            };
            terminal.stop = BatchStop::Failed;
            break;
        };
        terminal.charged_entries = entries;
        terminal.charged_bytes = bytes;
        terminal.charged_name_bytes = names;
        progress.planned_entries = terminal.charged_entries;
        progress.planned_bytes = terminal.charged_bytes;
        progress.planned_name_bytes = terminal.charged_name_bytes;
        let copied_before = progress.copied_bytes;
        let verified_before = progress.verified_bytes;
        let outcome = execute_copy_new(plan, &job.cancel, |root| {
            progress.phase = match root.phase {
                Phase::Copying => BatchPhase::Copying,
                Phase::Verifying => BatchPhase::Verifying,
                Phase::BeforeCommit => BatchPhase::BeforeCommit,
                Phase::MetadataApplied => BatchPhase::MetadataApplied,
                Phase::ContentPublished => BatchPhase::ContentPublished,
                Phase::Committed => BatchPhase::RootCommitted,
            };
            progress.copied_bytes = copied_before
                .checked_add(root.copied_bytes)
                .expect("engine byte budget invariant");
            progress.verified_bytes = verified_before
                .checked_add(root.verified_bytes)
                .expect("engine byte budget invariant");
            progress.root = Some(root);
            publish(shared, progress);
        });
        terminal.roots[index].outcome = match outcome {
            CopyOutcome::Committed(receipt) => {
                terminal.committed_roots += 1;
                progress.committed_roots = terminal.committed_roots;
                if receipt.durability == Durability::Uncertain
                    || matches!(receipt.metadata, MetadataCompletion::Incomplete { .. })
                {
                    terminal.stop = BatchStop::CommittedUncertain;
                    RootOutcome::CommittedUncertain(receipt)
                } else if matches!(receipt.cleanup, Cleanup::RecoveryRequired(_)) {
                    terminal.stop = BatchStop::RecoveryRequired;
                    RootOutcome::Committed(receipt)
                } else {
                    RootOutcome::Committed(receipt)
                }
            }
            CopyOutcome::Cancelled { cleanup } => {
                terminal.stop = BatchStop::Cancelled;
                RootOutcome::Cancelled { cleanup }
            }
            CopyOutcome::Failed { error, cleanup } => {
                terminal.stop = BatchStop::Failed;
                RootOutcome::Failed { error, cleanup }
            }
        };
        publish(shared, progress);
        if terminal.stop != BatchStop::Complete {
            break;
        }
    }
    terminal
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::sync::atomic::AtomicU64;
    use std::sync::{Condvar, mpsc};
    use std::time::{Duration, Instant};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures-generated")
                .join(format!(
                    "session-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(path.join("out")).unwrap();
            Self(path)
        }
        fn path(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
        fn file(&self, name: impl AsRef<Path>, bytes: &[u8]) -> PathBuf {
            let path = self.path(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }
        fn batch(&self, names: &[&str]) -> CopyBatch {
            CopyBatch::new(
                names.iter().map(|name| self.path(name)).collect(),
                self.path("out"),
            )
        }
        fn clean(&self) {
            assert!(!fs::read_dir(self.path("out")).unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b".browser-copy-")
            }));
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done() {
            assert!(Instant::now() < deadline, "copy worker handshake timeout");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn finish(session: &mut CopySession) {
        until(|| {
            session.poll();
            session.terminal().is_some()
        });
    }
    fn hook(session: &CopySession, action: impl FnMut(CopyProgress) + Send + 'static) {
        *session.shared.hook.lock().unwrap() = Some(Box::new(action));
    }
    fn gate(
        session: &CopySession,
        phase: BatchPhase,
    ) -> (mpsc::Receiver<()>, Arc<(Mutex<bool>, Condvar)>) {
        let (entered, receiver) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = Arc::clone(&release);
        let mut once = false;
        hook(session, move |progress| {
            if !once && progress.phase == phase {
                once = true;
                entered.send(()).unwrap();
                let (lock, wake) = &*worker_release;
                let guard = lock.lock().unwrap();
                drop(wake.wait_while(guard, |ready| !*ready).unwrap());
            }
        });
        (receiver, release)
    }
    fn release(gate: Arc<(Mutex<bool>, Condvar)>) {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }

    #[test]
    fn multi_root_native_copy_matches_independent_python_hash_oracle() {
        let f = Fixture::new();
        let native = std::ffi::OsString::from_vec(b"native-\xff".to_vec());
        f.file(
            "tree/nested/data",
            &(0..140_000).map(|n| (n % 251) as u8).collect::<Vec<_>>(),
        );
        let native_source = f.file(&native, b"native bytes");
        let mut session = CopySession::new().unwrap();
        let id = session
            .submit(CopyBatch::new(
                vec![f.path("tree"), native_source],
                f.path("out"),
            ))
            .unwrap();
        finish(&mut session);
        let terminal = session.terminal().unwrap();
        assert_eq!(terminal.id, id);
        assert_eq!(terminal.stop, BatchStop::Complete);
        assert_eq!(terminal.committed_roots, 2);
        assert!(
            terminal
                .roots
                .iter()
                .all(|root| matches!(root.outcome, RootOutcome::Committed(_)))
        );
        assert_eq!(
            terminal.roots[1]
                .destination
                .file_name()
                .unwrap()
                .as_bytes(),
            native.as_bytes()
        );
        let oracle = std::process::Command::new("python3").args(["-c", "import hashlib,os,sys; r=os.fsencode(sys.argv[1]); pairs=[(r+b'/tree/nested/data',r+b'/out/tree/nested/data'),(r+b'/native-\\xff',r+b'/out/native-\\xff')]; assert all(hashlib.sha256(open(a,'rb').read()).digest()==hashlib.sha256(open(b,'rb').read()).digest() for a,b in pairs)"]).arg(&f.0).status().unwrap();
        assert!(oracle.success());
        f.clean();
    }

    #[test]
    fn duplicate_and_invalid_paths_fail_whole_preflight_without_writes() {
        let f = Fixture::new();
        f.file("one/same", b"one");
        f.file("two/same", b"two");
        for batch in [
            f.batch(&["one/same", "two/same"]),
            CopyBatch::new(vec![f.path("one/../one/same")], f.path("out")),
        ] {
            let mut session = CopySession::new().unwrap();
            session.submit(batch).unwrap();
            finish(&mut session);
            let outcome = session.terminal().unwrap();
            assert_eq!(outcome.stop, BatchStop::Failed);
            assert!(outcome.preflight_error.is_some());
            assert!(
                outcome
                    .roots
                    .iter()
                    .all(|root| matches!(root.outcome, RootOutcome::NotStarted))
            );
            assert_eq!(fs::read_dir(f.path("out")).unwrap().count(), 0);
        }
    }

    #[test]
    fn later_root_failure_preserves_committed_and_marks_remaining_not_started() {
        let f = Fixture::new();
        f.file("a", b"a");
        f.file("c", b"c");
        let mut session = CopySession::new().unwrap();
        session.submit(f.batch(&["a", "missing", "c"])).unwrap();
        finish(&mut session);
        let terminal = session.terminal().unwrap();
        assert_eq!(terminal.stop, BatchStop::Failed);
        assert_eq!(terminal.committed_roots, 1);
        assert!(matches!(
            terminal.roots[0].outcome,
            RootOutcome::Committed(_)
        ));
        assert!(matches!(
            terminal.roots[1].outcome,
            RootOutcome::Failed { .. }
        ));
        assert!(matches!(terminal.roots[2].outcome, RootOutcome::NotStarted));
        assert_eq!(fs::read(f.path("out/a")).unwrap(), b"a");
        assert!(!f.path("out/c").exists());
        f.clean();
    }

    #[test]
    fn aggregate_entry_payload_and_name_budgets_are_shared_across_roots() {
        for kind in 0..3 {
            let f = Fixture::new();
            f.file("a/x", b"abc");
            f.file("b/y", b"def");
            f.file("c/z", b"ghi");
            let mut batch = f.batch(&["a", "b", "c"]);
            match kind {
                0 => batch.limits.max_entries = 3,
                1 => batch.limits.max_bytes = 5,
                _ => batch.limits.max_name_bytes = 1,
            }
            let mut session = CopySession::new().unwrap();
            session.submit(batch).unwrap();
            finish(&mut session);
            let terminal = session.terminal().unwrap();
            assert_eq!(terminal.committed_roots, 1);
            assert_eq!(terminal.stop, BatchStop::Failed);
            assert_eq!(terminal.charged_entries, 2);
            assert_eq!(terminal.charged_bytes, 3);
            assert_eq!(terminal.charged_name_bytes, 1);
            assert!(
                matches!(terminal.roots[1].outcome, RootOutcome::Failed { ref error, .. } if error.kind == ErrorKind::LimitExceeded)
            );
            assert!(matches!(terminal.roots[2].outcome, RootOutcome::NotStarted));
            f.clean();
        }
    }

    #[test]
    fn cancellation_before_during_and_after_commit_respects_irreversible_boundary() {
        for phase in [
            BatchPhase::Preflight,
            BatchPhase::Copying,
            BatchPhase::BeforeCommit,
            BatchPhase::RootCommitted,
        ] {
            let f = Fixture::new();
            f.file("a", &vec![7; 200_000]);
            f.file("b", b"b");
            let mut session = CopySession::new().unwrap();
            let (entered, ready) = gate(&session, phase);
            let id = session.submit(f.batch(&["a", "b"])).unwrap();
            entered.recv_timeout(Duration::from_secs(10)).unwrap();
            session.cancel(id).unwrap();
            release(ready);
            finish(&mut session);
            let terminal = session.terminal().unwrap();
            assert_eq!(terminal.stop, BatchStop::Cancelled);
            if phase == BatchPhase::RootCommitted {
                assert_eq!(terminal.committed_roots, 1);
                assert!(f.path("out/a").exists());
            } else {
                assert_eq!(terminal.committed_roots, 0);
                assert!(!f.path("out/a").exists());
            }
            assert!(matches!(terminal.roots[1].outcome, RootOutcome::NotStarted));
            f.clean();
        }
        let f = Fixture::new();
        f.file("a", b"a");
        let mut session = CopySession::new().unwrap();
        let (entered, ready) = gate(&session, BatchPhase::RootCommitted);
        let id = session.submit(f.batch(&["a"])).unwrap();
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        session.cancel(id).unwrap();
        release(ready);
        finish(&mut session);
        assert_eq!(session.terminal().unwrap().stop, BatchStop::Complete);
        assert_eq!(session.terminal().unwrap().committed_roots, 1);
    }

    #[test]
    fn busy_until_matching_terminal_ack_and_monotonic_ids_never_wrap() {
        let f = Fixture::new();
        f.file("a", b"a");
        f.file("b", b"b");
        let mut session = CopySession::new().unwrap();
        let (entered, ready) = gate(&session, BatchPhase::Preflight);
        let first = session.submit(f.batch(&["a"])).unwrap();
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            session.submit(f.batch(&["b"])).unwrap_err().kind,
            SubmitErrorKind::Busy
        );
        assert_eq!(session.acknowledge(first), Err(ControlError::NotTerminal));
        release(ready);
        finish(&mut session);
        assert_eq!(
            session.submit(f.batch(&["b"])).unwrap_err().kind,
            SubmitErrorKind::Busy
        );
        assert_eq!(
            session.acknowledge(JobId {
                owner: first.owner,
                sequence: first.sequence + 1
            }),
            Err(ControlError::WrongJob)
        );
        assert_eq!(session.terminal().unwrap().id, first);
        let (sender, receiver) = mpsc::channel();
        *session.terminal().unwrap().drop_thread.lock().unwrap() = Some(sender);
        let worker_id = session.worker.as_ref().unwrap().thread().id();
        session.acknowledge(first).unwrap();
        session.poll();
        assert!(
            session.terminal().is_none(),
            "acknowledged terminal must not be reinstalled"
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
            worker_id
        );
        until(|| session.shared.state.lock().unwrap().active.is_none());
        session.shared.state.lock().unwrap().next_id = Some(u64::MAX);
        let last = session.submit(f.batch(&["b"])).unwrap();
        assert_eq!(last.get(), u64::MAX);
        assert_eq!(session.cancel(first), Err(ControlError::WrongJob));
        finish(&mut session);
        session.acknowledge(last).unwrap();
        until(|| session.shared.state.lock().unwrap().active.is_none());
        assert_eq!(
            session.submit(f.batch(&["a"])).unwrap_err().kind,
            SubmitErrorKind::IdExhausted
        );
    }

    #[test]
    fn concurrent_owner_ids_reject_foreign_cancel_and_ack_without_touching_pending_or_terminal() {
        let f = Fixture::new();
        f.file("old", b"old");
        f.file("current", b"current");
        let mut old = CopySession::new().unwrap();
        let old_id = old.submit(f.batch(&["old"])).unwrap();
        finish(&mut old);
        let mut current = CopySession::new().unwrap();
        let (entered, ready) = gate(&current, BatchPhase::Preflight);
        let current_id = current.submit(f.batch(&["current"])).unwrap();
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(old_id.get(), current_id.get());
        assert_ne!(old_id, current_id);
        assert_eq!(current.cancel(old_id), Err(ControlError::WrongJob));
        assert_eq!(current.acknowledge(old_id), Err(ControlError::WrongJob));
        assert_eq!(
            current.submit(f.batch(&["old"])).unwrap_err().kind,
            SubmitErrorKind::Busy
        );
        release(ready);
        finish(&mut current);
        assert_eq!(current.terminal().unwrap().id, current_id);
        assert_eq!(current.terminal().unwrap().stop, BatchStop::Complete);
        assert_eq!(current.cancel(old_id), Err(ControlError::WrongJob));
        assert_eq!(current.acknowledge(old_id), Err(ControlError::WrongJob));
        assert_eq!(current.terminal().unwrap().id, current_id);
        assert_eq!(
            current.submit(f.batch(&["old"])).unwrap_err().kind,
            SubmitErrorKind::Busy
        );
        // Foreign control calls neither cancel the active copy nor release its
        // terminal gate. Only the actual owner's complete identity can ack it.
        assert_eq!(fs::read(f.path("out/current")).unwrap(), b"current");
        current.acknowledge(current_id).unwrap();
        old.acknowledge(old_id).unwrap();
    }

    #[test]
    fn recreated_sessions_never_reuse_owner_identity_for_delayed_callbacks() {
        let f = Fixture::new();
        f.file("old", b"old");
        let old_id = {
            let mut old = CopySession::new().unwrap();
            let id = old.submit(f.batch(&["old"])).unwrap();
            finish(&mut old);
            old.acknowledge(id).unwrap();
            id
        };
        for index in 0..3 {
            let name = format!("new-{index}");
            f.file(&name, b"new");
            let mut current = CopySession::new().unwrap();
            let id = current
                .submit(CopyBatch::new(vec![f.path(&name)], f.path("out")))
                .unwrap();
            assert_eq!(id.get(), old_id.get());
            assert_ne!(id, old_id);
            assert_eq!(current.cancel(old_id), Err(ControlError::WrongJob));
            assert_eq!(current.acknowledge(old_id), Err(ControlError::WrongJob));
            finish(&mut current);
            assert_eq!(current.terminal().unwrap().id, id);
            assert_eq!(current.terminal().unwrap().stop, BatchStop::Complete);
            assert_eq!(current.acknowledge(old_id), Err(ControlError::WrongJob));
            assert_eq!(current.terminal().unwrap().id, id);
            current.acknowledge(id).unwrap();
        }
    }

    #[test]
    fn owner_allocator_exhaustion_does_not_wrap_or_reuse_an_identity() {
        let next = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_owner(&next).unwrap(), SessionOwner(u64::MAX - 1));
        assert!(allocate_owner(&next).is_err());
        assert!(allocate_owner(&next).is_err());
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn recovery_residue_preserves_native_identity_and_stops_remaining_roots() {
        let f = Fixture::new();
        f.file("a", b"ours");
        f.file("b", b"later");
        let mut session = CopySession::new().unwrap();
        let out = f.path("out");
        let mut once = false;
        hook(&session, move |progress| {
            if !once && progress.phase == BatchPhase::BeforeCommit {
                once = true;
                let stage = fs::read_dir(&out)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .as_bytes()
                            .starts_with(b".browser-copy-")
                    })
                    .unwrap();
                fs::rename(stage.join("payload"), stage.join("original-payload")).unwrap();
                fs::write(stage.join("payload"), b"foreign").unwrap();
            }
        });
        session.submit(f.batch(&["a", "b"])).unwrap();
        finish(&mut session);
        let terminal = session.terminal().unwrap();
        assert_eq!(terminal.stop, BatchStop::Failed);
        match &terminal.roots[0].outcome {
            RootOutcome::Failed {
                error,
                cleanup: Cleanup::RecoveryRequired(residue),
            } => {
                assert_eq!(error.kind, ErrorKind::DestinationChanged);
                assert!(residue.identity.is_some());
                assert_eq!(fs::read(residue.path.join("payload")).unwrap(), b"foreign");
                assert_eq!(
                    fs::read(residue.path.join("original-payload")).unwrap(),
                    b"ours"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(terminal.roots[1].outcome, RootOutcome::NotStarted));
        assert!(!f.path("out/a").exists());
        assert!(!f.path("out/b").exists());
    }

    #[test]
    fn actual_native_write_limit_and_post_commit_journal_failures_have_honest_batch_outcomes() {
        for mode in ["write", "journal"] {
            let f = Fixture::new();
            if mode == "write" {
                f.file("a", &vec![11; 131_111]);
            } else {
                f.file("x".repeat(255), b"");
            }
            f.file("b", b"not started");
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", "trap '' XFSZ; ulimit -f 2; exec \"$1\" --exact session::tests::file_size_limit_helper --nocapture", "copy-session-limit"])
                .arg(std::env::current_exe().unwrap()).env("BROWSER_COPY_SESSION_FIXTURE", &f.0).env("BROWSER_COPY_SESSION_MODE", mode).output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!f.path("out/b").exists());
        }
    }

    #[test]
    fn file_size_limit_helper() {
        let Some(root) = std::env::var_os("BROWSER_COPY_SESSION_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        assert!(root.starts_with(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated")
        ));
        let journal = std::env::var("BROWSER_COPY_SESSION_MODE").unwrap() == "journal";
        let leaf = if journal { "x".repeat(255) } else { "a".into() };
        let mut session = CopySession::new().unwrap();
        session
            .submit(CopyBatch::new(
                vec![root.join(&leaf), root.join("b")],
                root.join("out"),
            ))
            .unwrap();
        finish(&mut session);
        let terminal = session.terminal().unwrap();
        assert!(matches!(terminal.roots[1].outcome, RootOutcome::NotStarted));
        if journal {
            assert_eq!(terminal.stop, BatchStop::CommittedUncertain);
            assert_eq!(terminal.committed_roots, 1);
            let RootOutcome::CommittedUncertain(receipt) = &terminal.roots[0].outcome else {
                panic!("{:?}", terminal.roots[0]);
            };
            assert!(
                receipt
                    .warnings
                    .iter()
                    .any(|error| error.raw_os_error == Some(27))
            );
            let Cleanup::RecoveryRequired(residue) = &receipt.cleanup else {
                panic!("missing recovery breadcrumb");
            };
            assert!(residue.identity.is_some());
            assert!(receipt.destination.is_file());
            let journal =
                crate::journal::decode(&fs::read(residue.path.join("intent")).unwrap()).unwrap();
            assert!(journal.torn_tail);
            assert_eq!(
                journal.records.last().unwrap().phase,
                crate::journal::JournalPhase::CommitIntent
            );
        } else {
            assert_eq!(terminal.stop, BatchStop::Failed);
            assert_eq!(terminal.committed_roots, 0);
            assert!(
                matches!(terminal.roots[0].outcome, RootOutcome::Failed { ref error, cleanup: Cleanup::Clean } if error.raw_os_error == Some(27))
            );
            assert!(!root.join("out/a").exists());
        }
        assert!(root.join(&leaf).is_file());
    }

    #[test]
    fn close_does_not_join_blocked_worker_and_terminal_destruction_stays_on_worker() {
        let f = Fixture::new();
        f.file("a", b"a");
        let mut session = CopySession::new().unwrap();
        let (entered, ready) = gate(&session, BatchPhase::Preflight);
        let owner = Arc::downgrade(&session.shared);
        session.submit(f.batch(&["a"])).unwrap();
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(session);
        assert!(owner.upgrade().is_some());
        release(ready);
        until(|| owner.upgrade().is_none());
        assert!(!f.path("out/a").exists());
        let mut session = CopySession::new().unwrap();
        session.submit(f.batch(&["a"])).unwrap();
        finish(&mut session);
        let worker_id = session.worker.as_ref().unwrap().thread().id();
        let (sender, receiver) = mpsc::channel();
        *session.terminal().unwrap().drop_thread.lock().unwrap() = Some(sender);
        let owner = Arc::downgrade(&session.shared);
        drop(session);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
            worker_id
        );
        until(|| owner.upgrade().is_none());
        // Idle park also wakes and exits after close without a polling timeout.
        let session = CopySession::new().unwrap();
        let owner = Arc::downgrade(&session.shared);
        drop(session);
        until(|| owner.upgrade().is_none());
    }
    #[test]
    fn late_cancel_finalizes_published_root_and_leaves_later_batch_roots_not_started() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        fs::create_dir(f.path("readonly")).unwrap();
        fs::set_permissions(f.path("readonly"), fs::Permissions::from_mode(0o555)).unwrap();
        f.file("later", b"later root");
        let mut session = CopySession::new().unwrap();
        let (entered, ready) = gate(&session, BatchPhase::ContentPublished);
        let id = session
            .submit(f.batch(&["readonly", "later"]).with_options(CopyOptions {
                metadata: MetadataPolicy::Basic,
                root_publication: RootPublication::FinalizeBasicAfterPublish,
                ..CopyOptions::default()
            }))
            .unwrap();
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        session.cancel(id).unwrap();
        release(ready);
        finish(&mut session);
        let terminal = session.terminal().unwrap();
        assert_eq!(terminal.stop, BatchStop::Cancelled);
        assert_eq!(terminal.committed_roots, 1);
        let RootOutcome::Committed(receipt) = &terminal.roots[0].outcome else {
            panic!("{terminal:?}")
        };
        assert_eq!(
            receipt.metadata,
            MetadataCompletion::Achieved(MetadataPolicy::Basic)
        );
        assert_eq!(receipt.durability, Durability::Synced);
        assert!(matches!(receipt.cleanup, Cleanup::Clean));
        assert_eq!(
            fs::metadata(f.path("out/readonly"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
        assert!(matches!(terminal.roots[1].outcome, RootOutcome::NotStarted));
        assert!(!f.path("out/later").exists());
        f.clean();
    }
    #[test]
    fn incomplete_published_root_preserves_commit_and_fail_stops_batch_without_touching_replacement()
     {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        fs::create_dir(f.path("readonly")).unwrap();
        fs::set_permissions(f.path("readonly"), fs::Permissions::from_mode(0o555)).unwrap();
        f.file("later", b"later root");
        let mut session = CopySession::new().unwrap();
        let destination = f.path("out/readonly");
        let moved = f.path("out/displaced");
        let mut once = false;
        hook(&session, move |p| {
            if p.phase == BatchPhase::ContentPublished && !once {
                once = true;
                fs::rename(&destination, &moved).unwrap();
                fs::create_dir(&destination).unwrap();
                fs::write(destination.join("foreign"), b"replacement").unwrap();
            }
        });
        session
            .submit(f.batch(&["readonly", "later"]).with_options(CopyOptions {
                metadata: MetadataPolicy::Basic,
                root_publication: RootPublication::FinalizeBasicAfterPublish,
                ..CopyOptions::default()
            }))
            .unwrap();
        finish(&mut session);
        let terminal = session.terminal().unwrap();
        assert_eq!(terminal.stop, BatchStop::CommittedUncertain);
        assert_eq!(terminal.committed_roots, 1);
        let RootOutcome::CommittedUncertain(receipt) = &terminal.roots[0].outcome else {
            panic!("{terminal:?}")
        };
        assert_eq!(
            receipt.metadata,
            MetadataCompletion::Incomplete {
                requested: MetadataPolicy::Basic
            }
        );
        assert_eq!(receipt.durability, Durability::Uncertain);
        assert!(matches!(receipt.cleanup, Cleanup::RecoveryRequired(_)));
        assert!(matches!(terminal.roots[1].outcome, RootOutcome::NotStarted));
        assert_eq!(
            fs::read(f.path("out/readonly/foreign")).unwrap(),
            b"replacement"
        );
        assert!(!f.path("out/later").exists());
    }
}
