//! Worker-only copy-to-new-destination transactions. See README.md for guarantees and limits.
#![forbid(unsafe_code)]

use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(target_os = "linux")]
pub mod fsops;
#[cfg(target_os = "linux")]
pub mod journal;
#[cfg(target_os = "linux")]
mod linux;
pub mod session;

#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Explicit metadata behavior, independent from resource limits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MetadataPolicy {
    #[default]
    Private,
    Basic,
}
/// Explicit visibility contract for Basic directory-root metadata.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RootPublication {
    /// All final metadata precedes publication; non-owner-writable roots are rejected.
    #[default]
    MetadataBeforePublish,
    /// A non-owner-writable Basic root is published as 0700, then finalized on its bound FD.
    /// Final root permissions/times are not atomically visible. Late cancellation cannot
    /// abandon mandatory finalization, and failures remain committed/uncertain.
    FinalizeBasicAfterPublish,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyOptions {
    pub metadata: MetadataPolicy,
    pub root_publication: RootPublication,
    /// Basic retains these bound directory handles for safe permission repair.
    /// Admission rejects more required pins before any staging write. Hard cap128.
    pub max_restricted_directory_handles: usize,
}
impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            metadata: MetadataPolicy::Private,
            root_publication: RootPublication::MetadataBeforePublish,
            max_restricted_directory_handles: 64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CopyLimits {
    pub max_entries: usize,
    pub max_bytes: u64,
    /// Charged for every manifest relative path and copied symlink target.
    pub max_name_bytes: usize,
    /// Hard ceiling 128; planning holds at most depth + a constant number of FDs.
    pub max_depth: usize,
    /// 1..=1 MiB. Execution uses two buffers of this size for independent readback.
    pub buffer_bytes: usize,
    pub max_symlink_bytes: usize,
}
impl Default for CopyLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_bytes: 1 << 30,
            max_name_bytes: 16 << 20,
            max_depth: 64,
            buffer_bytes: 64 << 10,
            max_symlink_bytes: 16 << 10,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidPath,
    UnsupportedPlatform,
    UnsupportedType,
    LimitExceeded,
    DestinationConflict,
    Containment,
    SourceChanged,
    DestinationChanged,
    Cancelled,
    AtomicCommitUnsupported,
    VerificationFailed,
    InvalidJournal,
    UnsupportedMetadata,
    Io,
}
#[derive(Clone, Debug)]
pub struct OperationError {
    pub kind: ErrorKind,
    pub phase: &'static str,
    pub path: PathBuf,
    pub message: String,
    pub io_kind: Option<io::ErrorKind>,
    pub raw_os_error: Option<i32>,
}
impl std::fmt::Display for OperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} ({:?})", self.phase, self.message, self.path)
    }
}
impl std::error::Error for OperationError {}
impl OperationError {
    fn new(kind: ErrorKind, phase: &'static str, path: impl Into<PathBuf>, message: &str) -> Self {
        Self {
            kind,
            phase,
            path: path.into(),
            message: message.into(),
            io_kind: None,
            raw_os_error: None,
        }
    }
    #[cfg(target_os = "linux")]
    fn io(phase: &'static str, path: impl Into<PathBuf>, error: impl Into<io::Error>) -> Self {
        let error = error.into();
        Self {
            kind: ErrorKind::Io,
            phase,
            path: path.into(),
            message: error.to_string(),
            io_kind: Some(error.kind()),
            raw_os_error: error.raw_os_error(),
        }
    }
}
pub type Result<T> = std::result::Result<T, OperationError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NativeIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug)]
pub struct PlanSummary {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub entries: usize,
    pub bytes: u64,
    /// Exact manifest relative-path plus symlink-target charge used by planning.
    pub name_bytes: usize,
    pub directories: usize,
    pub symlinks: usize,
}

/// Owns bound parent/root handles. Consumed by execution; never reconstruct paths from display text.
#[derive(Debug)]
pub struct CopyPlan {
    summary: PlanSummary,
    options: CopyOptions,
    #[cfg(target_os = "linux")]
    native: linux::Plan,
}
impl CopyPlan {
    pub fn summary(&self) -> &PlanSummary {
        &self.summary
    }
    pub fn options(&self) -> CopyOptions {
        self.options
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Copying,
    Verifying,
    BeforeCommit,
    MetadataApplied,
    /// Content was irreversibly published; mandatory root finalization is still pending.
    ContentPublished,
    Committed,
}
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub completed_entries: usize,
    pub total_entries: usize,
    pub copied_bytes: u64,
    pub verified_bytes: u64,
    pub total_bytes: u64,
}
#[derive(Clone, Debug)]
pub struct Residue {
    /// Last known path. Native identity is authoritative if a parent was moved externally.
    pub path: PathBuf,
    /// None means staging creation succeeded but its handle/identity could not be obtained.
    /// Such a residue must never be deleted automatically from the textual path alone.
    pub identity: Option<NativeIdentity>,
    pub error: OperationError,
}
#[derive(Clone, Debug)]
pub enum Cleanup {
    Clean,
    RecoveryRequired(Residue),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
    Synced,
    Uncertain,
}
/// Metadata achievement is independent of later durability/journal failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataCompletion {
    Achieved(MetadataPolicy),
    Incomplete { requested: MetadataPolicy },
}
#[derive(Clone, Debug)]
pub struct CommitReceipt {
    /// Achieved only after exact metadata readback and final published binding validation.
    pub metadata: MetadataCompletion,
    pub operation_id: [u8; 16],
    pub destination: PathBuf,
    pub identity: NativeIdentity,
    pub entries: usize,
    pub bytes: u64,
    /// All payload data/directories and the final destination parent were synced for Synced.
    pub durability: Durability,
    pub warnings: Vec<OperationError>,
    pub cleanup: Cleanup,
}
#[derive(Clone, Debug)]
pub enum CopyOutcome {
    Committed(CommitReceipt),
    Cancelled {
        cleanup: Cleanup,
    },
    Failed {
        error: OperationError,
        cleanup: Cleanup,
    },
}

/// Blocking worker API. Planning never creates destination output and never returns a partial plan.
pub fn plan_copy_new(
    source: PathBuf,
    destination: PathBuf,
    limits: CopyLimits,
    cancel: &Cancellation,
) -> Result<CopyPlan> {
    plan_copy_new_with_options(source, destination, limits, CopyOptions::default(), cancel)
}

/// Explicit policy variant. Basic metadata support is qualified separately from
/// content copying; unsupported modes/kernel/filesystems fail before publication.
pub fn plan_copy_new_with_options(
    source: PathBuf,
    destination: PathBuf,
    limits: CopyLimits,
    options: CopyOptions,
    cancel: &Cancellation,
) -> Result<CopyPlan> {
    #[cfg(target_os = "linux")]
    {
        linux::plan(source, destination, limits, options, cancel)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, limits, options, cancel);
        Err(OperationError::new(
            ErrorKind::UnsupportedPlatform,
            "plan",
            destination,
            "only Linux is implemented",
        ))
    }
}

/// Blocking worker API. Progress is synchronous and bounded (no internal queue); callbacks must not
/// block or panic. Cancellation after the atomic commit boundary returns Committed.
pub fn execute_copy_new(
    plan: CopyPlan,
    cancel: &Cancellation,
    progress: impl FnMut(Progress),
) -> CopyOutcome {
    #[cfg(target_os = "linux")]
    {
        linux::execute(plan, cancel, progress)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cancel, progress);
        CopyOutcome::Failed {
            error: OperationError::new(
                ErrorKind::UnsupportedPlatform,
                "execute",
                plan.summary.destination,
                "only Linux is implemented",
            ),
            cleanup: Cleanup::Clean,
        }
    }
}
