//! Blocking ZIP provider. Call these APIs from an application's bounded I/O workers.
//!
//! Entry identities belong to a session, never to a host `archive.zip/member` path.
//! `read_entry` can write unchecked bytes before failing: only its successful return
//! authenticates the complete stream. Use `extract_new` for destination publication.

mod browser;
mod jobs;
mod names;
mod transaction;

pub use browser::*;
pub use jobs::*;

use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, Metadata},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use zip::read::ZipReadOptions;

pub use transaction::{Commit, CreateOptions, CreateSource, create_new, extract_new};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
pub type Result<T> = std::result::Result<T, ArchiveError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Io,
    Malformed,
    UnsafePath,
    UnsafeLink,
    AmbiguousEntry,
    Unsupported,
    CredentialRequired,
    Integrity,
    LimitExceeded,
    SourceChanged,
    StaleEntry,
    Conflict,
    Cancelled,
    Deadline,
    AtomicCommitUnsupported,
    RecoveryRequired,
}

/// Cleanup errors never erase the original failure. A published path means the
/// commit happened even when the parent-directory sync subsequently failed.
#[derive(Debug)]
pub struct ArchiveError {
    pub kind: ErrorKind,
    pub message: String,
    pub io: Option<io::Error>,
    pub residue: Option<PathBuf>,
    pub published: Option<PathBuf>,
}

impl ArchiveError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            io: None,
            residue: None,
            published: None,
        }
    }
    pub(crate) fn io(context: &str, error: io::Error) -> Self {
        let kind = if error.kind() == io::ErrorKind::AlreadyExists {
            ErrorKind::Conflict
        } else {
            ErrorKind::Io
        };
        Self {
            kind,
            message: context.into(),
            io: Some(error),
            residue: None,
            published: None,
        }
    }
}
impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)?;
        if let Some(error) = &self.io {
            write!(f, ": {error}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.io.as_ref().map(|e| e as _)
    }
}
fn malformed(error: impl std::fmt::Display) -> ArchiveError {
    ArchiveError::new(ErrorKind::Malformed, error.to_string())
}
fn decode_error(context: &str, error: io::Error) -> ArchiveError {
    let corrupt = matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
    );
    let mut result = ArchiveError::io(context, error);
    if corrupt {
        result.kind = ErrorKind::Integrity;
    }
    result
}
fn zip_error(error: zip::result::ZipError) -> ArchiveError {
    use zip::result::ZipError;
    if let ZipError::Io(error) = error {
        return decode_error("read ZIP structures or payload", error);
    }
    let kind = match &error {
        ZipError::InvalidPassword => ErrorKind::Integrity,
        ZipError::UnsupportedArchive(message) if message.contains("Password") => {
            ErrorKind::CredentialRequired
        }
        ZipError::UnsupportedArchive(_) => ErrorKind::Unsupported,
        _ => ErrorKind::Malformed,
    };
    ArchiveError::new(kind, error.to_string())
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub directory_bytes: u64,
    pub entries: usize,
    /// Conservative metadata admission cost, including the backend index. This is
    /// not a process RSS guarantee; see README for remaining allocator limits.
    pub metadata_bytes: usize,
    pub member_bytes: u64,
    pub total_bytes: u64,
    pub expansion_ratio: u64,
    pub path_components: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            directory_bytes: 128 << 20,
            entries: 1_000_000,
            metadata_bytes: 64 << 20,
            member_bytes: 16 << 30,
            total_bytes: 64 << 30,
            expansion_ratio: 1000,
            path_components: 256,
        }
    }
}

#[derive(Default, Debug)]
pub struct Cancellation(AtomicBool);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Share one control across a lineage to charge retries and nested materialization.
/// Deadlines are cooperative; kernel I/O can block beyond them.
pub struct Control<'a> {
    cancel: &'a Cancellation,
    deadline: Instant,
    consumed: AtomicU64,
    observed: Option<Arc<AtomicU64>>,
    budget: u64,
}
impl<'a> Control<'a> {
    pub fn new(cancel: &'a Cancellation, duration: Duration, total_bytes: u64) -> Self {
        Self {
            cancel,
            deadline: Instant::now()
                .checked_add(duration)
                .unwrap_or_else(Instant::now),
            consumed: AtomicU64::new(0),
            observed: None,
            budget: total_bytes,
        }
    }
    pub(crate) fn with_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.observed = Some(counter);
        self
    }
    pub fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(ArchiveError::new(
                ErrorKind::Cancelled,
                "archive operation cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(ArchiveError::new(
                ErrorKind::Deadline,
                "archive deadline reached",
            ));
        }
        Ok(())
    }
    pub fn consumed_bytes(&self) -> u64 {
        self.consumed.load(Ordering::Acquire)
    }
    fn charge(&self, bytes: u64) -> Result<()> {
        let previous = self
            .consumed
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(bytes).filter(|next| *next <= self.budget)
            })
            .map_err(|_| {
                ArchiveError::new(ErrorKind::LimitExceeded, "aggregate decoded-byte budget")
            })?;
        if let Some(observed) = &self.observed {
            observed.fetch_max(previous + bytes, Ordering::Release);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ArchiveEntryId {
    session: u64,
    ordinal: u64,
    central_offset: u64,
}
impl ArchiveEntryId {
    pub fn session(self) -> u64 {
        self.session
    }
    pub fn ordinal(self) -> u64 {
        self.ordinal
    }
    pub fn central_offset(self) -> u64 {
        self.central_offset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Special,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameEncoding {
    Utf8,
    UnicodeExtra,
    Cp437,
    Invalid,
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub id: ArchiveEntryId,
    pub raw_name: Vec<u8>,
    /// Display text only. Never use this as an operation path.
    pub name: String,
    pub encoding: NameEncoding,
    pub kind: EntryKind,
    pub compressed_bytes: u64,
    pub uncompressed_bytes: u64,
    pub compression: u16,
    pub encrypted: bool,
    pub issue: Option<String>,
    components: Option<Vec<String>>,
}

impl ArchiveEntry {
    /// Validated archive-relative components, never a host filesystem path.
    /// Invalid/ambiguous names remain displayable but return None here.
    pub fn path_components(&self) -> Option<&[String]> {
        if self.issue.is_some() {
            None
        } else {
            self.components.as_deref()
        }
    }
}

#[derive(Debug)]
pub enum CatalogState {
    Complete,
    LimitReached,
    Cancelled,
    Failed,
}
pub struct Catalog {
    pub entries: Vec<ArchiveEntry>,
    pub state: CatalogState,
    pub error: Option<ArchiveError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: (i64, i64),
}
impl Fingerprint {
    fn of(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// A complete catalog pins source identity. An incomplete catalog is returned by
/// `scan` for display, but cannot produce a session that reads or extracts.
pub struct ArchiveSession {
    path: PathBuf,
    file: Mutex<File>,
    fingerprint: Fingerprint,
    directory_range: (u64, u64),
    directory_digest: [u8; 32],
    metadata_charged: usize,
    entries: Vec<ArchiveEntry>,
    ambiguous: bool,
    limits: Limits,
}

impl ArchiveSession {
    /// Returns useful incomplete rows on parser, cancellation and limit failures.
    pub fn scan(
        path: &Path,
        limits: Limits,
        control: &Control<'_>,
    ) -> Result<(Option<Self>, Catalog)> {
        control.check()?;
        let file = open_regular(path)?;
        let fingerprint = Fingerprint::of(
            &file
                .metadata()
                .map_err(|e| ArchiveError::io("stat archive", e))?,
        );
        let mut buffer = vec![0; rawzip::MAX_CENTRAL_DIRECTORY_RECORD_SIZE];
        let raw = rawzip::ZipArchive::from_file(
            file.try_clone()
                .map_err(|e| ArchiveError::io("clone archive handle", e))?,
            &mut buffer,
        )
        .map_err(malformed)?;
        let start = raw.directory_offset();
        let end = raw.eocd_offset();
        let session_id = NEXT_SESSION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| {
                ArchiveError::new(
                    ErrorKind::LimitExceeded,
                    "archive session identity exhausted",
                )
            })?;
        let mut catalog = Catalog {
            entries: Vec::new(),
            state: CatalogState::Complete,
            error: None,
        };
        let mut metadata_bytes = 0usize;
        let parsed = (|| {
            if end < start || end - start > limits.directory_bytes {
                return Err(ArchiveError::new(
                    ErrorKind::LimitExceeded,
                    "central-directory byte budget",
                ));
            }
            if raw.entries_hint() > limits.entries as u64 {
                return Err(ArchiveError::new(
                    ErrorKind::LimitExceeded,
                    "directory entry-count budget",
                ));
            }
            let mut entries = raw.entries(&mut buffer);
            while let Some(entry) = entries.next_entry().map_err(malformed)? {
                control.check()?;
                if catalog.entries.len() >= limits.entries {
                    return Err(ArchiveError::new(
                        ErrorKind::LimitExceeded,
                        "directory entry-count budget",
                    ));
                }
                let raw_name = entry.file_path().as_ref().to_vec();
                // Includes conservative overhead for both parsers, maps, extras,
                // path components and retained rows. No trust in entry-count hints.
                let mut extras = entry.extra_fields();
                let extra_bytes: usize = extras.by_ref().map(|(_, bytes)| bytes.len() + 260).sum();
                if !extras.remaining_bytes().is_empty() {
                    return Err(ArchiveError::new(
                        ErrorKind::Malformed,
                        "truncated ZIP extra field",
                    ));
                }
                metadata_bytes = metadata_bytes
                    .checked_add(
                        2048 + raw_name.len().saturating_mul(12)
                            + extra_bytes.saturating_mul(3)
                            + entry.comment().as_bytes().len().saturating_mul(4),
                    )
                    .ok_or_else(|| {
                        ArchiveError::new(ErrorKind::LimitExceeded, "metadata arithmetic overflow")
                    })?;
                if metadata_bytes > limits.metadata_bytes {
                    return Err(ArchiveError::new(
                        ErrorKind::LimitExceeded,
                        "metadata admission budget",
                    ));
                }
                let flags = entry.flags();
                let (name, encoding, mut issue) = names::decode(
                    &raw_name,
                    flags.is_utf8(),
                    entry.extra_fields().map(|(id, bytes)| (id.as_u16(), bytes)),
                );
                let components = match names::components(&name, limits.path_components) {
                    Ok(value) if issue.is_none() => Some(value),
                    Ok(_) => None,
                    Err(error) => {
                        issue = Some(error.message);
                        None
                    }
                };
                let mode = entry.external_attributes() >> 16;
                let kind = match mode & 0o170000 {
                    0o120000 => EntryKind::Symlink,
                    0 | 0o100000 | 0o040000 => {
                        if entry.is_dir() {
                            EntryKind::Directory
                        } else {
                            EntryKind::File
                        }
                    }
                    _ => EntryKind::Special,
                };
                catalog.entries.push(ArchiveEntry {
                    id: ArchiveEntryId {
                        session: session_id,
                        ordinal: catalog.entries.len() as u64,
                        central_offset: entry.central_directory_offset(),
                    },
                    raw_name,
                    name,
                    encoding,
                    kind,
                    compressed_bytes: entry.compressed_size_hint(),
                    uncompressed_bytes: entry.uncompressed_size_hint(),
                    compression: entry.compression_method().as_u16(),
                    encrypted: flags.is_encrypted(),
                    issue,
                    components,
                });
            }
            if catalog.entries.len() as u64 != raw.entries_hint() {
                return Err(ArchiveError::new(
                    ErrorKind::Malformed,
                    "central-directory record count disagrees with footer",
                ));
            }
            Ok(())
        })();
        if let Err(error) = parsed {
            catalog.state = match error.kind {
                ErrorKind::LimitExceeded => CatalogState::LimitReached,
                ErrorKind::Cancelled | ErrorKind::Deadline => CatalogState::Cancelled,
                _ => CatalogState::Failed,
            };
            catalog.error = Some(error);
            return Ok((None, catalog));
        }
        let mut names = HashMap::<String, Vec<usize>>::new();
        for (index, entry) in catalog.entries.iter().enumerate() {
            names
                .entry(entry.name.trim_end_matches('/').into())
                .or_default()
                .push(index);
        }
        let ambiguous = names.values().any(|group| group.len() > 1);
        for group in names.values().filter(|group| group.len() > 1) {
            for &index in group {
                catalog.entries[index].issue = Some("duplicate effective archive path".into());
            }
        }
        let mut file = file;
        let directory_digest = digest_range(&mut file, start, end, control)?;
        let current = Fingerprint::of(
            &file
                .metadata()
                .map_err(|e| ArchiveError::io("stat archive after scan", e))?,
        );
        if current != fingerprint {
            return Err(ArchiveError::new(
                ErrorKind::SourceChanged,
                "archive changed during scan",
            ));
        }
        let session = Self {
            path: path.to_owned(),
            file: Mutex::new(file),
            fingerprint,
            directory_range: (start, end),
            directory_digest,
            metadata_charged: metadata_bytes,
            entries: catalog.entries.clone(),
            ambiguous,
            limits,
        };
        Ok((Some(session), catalog))
    }

    pub fn open(path: &Path, limits: Limits, control: &Control<'_>) -> Result<Self> {
        let (session, catalog) = Self::scan(path, limits, control)?;
        session.ok_or_else(|| {
            catalog
                .error
                .unwrap_or_else(|| ArchiveError::new(ErrorKind::Malformed, "incomplete catalog"))
        })
    }
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn source_path(&self) -> &Path {
        &self.path
    }
    pub fn entry(&self, id: ArchiveEntryId) -> Result<&ArchiveEntry> {
        self.entries
            .get(id.ordinal as usize)
            .filter(|entry| entry.id == id)
            .ok_or_else(|| {
                ArchiveError::new(
                    ErrorKind::StaleEntry,
                    "record belongs to a different archive session",
                )
            })
    }

    pub(crate) fn stable(&self, file: &mut File, control: &Control<'_>) -> Result<()> {
        control.check()?;
        let current = Fingerprint::of(
            &file
                .metadata()
                .map_err(|e| ArchiveError::io("stat archive handle", e))?,
        );
        let path = std::fs::symlink_metadata(&self.path)
            .map_err(|e| ArchiveError::io("stat archive path", e))?;
        if current != self.fingerprint
            || path.file_type().is_symlink()
            || Fingerprint::of(&path) != self.fingerprint
        {
            return Err(ArchiveError::new(
                ErrorKind::SourceChanged,
                "archive source was changed or replaced",
            ));
        }
        let (start, end) = self.directory_range;
        if digest_range(file, start, end, control)? != self.directory_digest {
            return Err(ArchiveError::new(
                ErrorKind::SourceChanged,
                "archive directory changed",
            ));
        }
        Ok(())
    }
    pub fn check_source(&self, control: &Control<'_>) -> Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| ArchiveError::new(ErrorKind::Io, "archive worker lock poisoned"))?;
        self.stable(&mut file, control)
    }

    /// Writes untrusted partial bytes on failure. Only `Ok` confirms complete EOF,
    /// checksum/authentication and source-stability verification.
    pub fn read_entry(
        &self,
        id: ArchiveEntryId,
        destination: &mut dyn Write,
        password: Option<&[u8]>,
        control: &Control<'_>,
    ) -> Result<u64> {
        let entry = self.entry(id)?;
        if self.ambiguous {
            return Err(ArchiveError::new(
                ErrorKind::AmbiguousEntry,
                "archive contains duplicate effective paths",
            ));
        }
        if let Some(issue) = &entry.issue {
            return Err(ArchiveError::new(ErrorKind::UnsafePath, issue.clone()));
        }
        if entry.kind != EntryKind::File {
            return Err(ArchiveError::new(
                ErrorKind::UnsafeLink,
                "only regular members can be read as files",
            ));
        }
        check_size(
            entry.uncompressed_bytes,
            entry.compressed_bytes,
            &self.limits,
        )?;
        if !matches!(entry.compression, 0 | 8 | 99) {
            return Err(ArchiveError::new(
                ErrorKind::Unsupported,
                "ZIP compression method is not enabled",
            ));
        }
        if entry.encrypted && password.is_none() {
            return Err(ArchiveError::new(
                ErrorKind::CredentialRequired,
                "archive member needs a password",
            ));
        }
        self.read_many(&[entry], password, control, |entry, member, limits| {
            copy_member(member, destination, entry.compressed_bytes, limits, control)
        })
    }

    pub(crate) fn read_many(
        &self,
        entries: &[&ArchiveEntry],
        password: Option<&[u8]>,
        control: &Control<'_>,
        mut consume: impl FnMut(&ArchiveEntry, &mut dyn Read, &Limits) -> Result<u64>,
    ) -> Result<u64> {
        if self.ambiguous {
            return Err(ArchiveError::new(
                ErrorKind::AmbiguousEntry,
                "archive has duplicate effective paths",
            ));
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| ArchiveError::new(ErrorKind::Io, "archive worker lock poisoned"))?;
        self.stable(&mut file, control)?;
        let mut archive = zip::ZipArchive::new(&mut *file).map_err(zip_error)?;
        archive.validate_header_consistency().map_err(zip_error)?;
        control.check()?;
        let mut backend = HashMap::new();
        for index in 0..archive.len() {
            control.check()?;
            let offset = archive
                .metadata()
                .entry(index)
                .map_err(zip_error)?
                .central_header_start();
            if backend.insert(offset, index).is_some() {
                return Err(ArchiveError::new(
                    ErrorKind::Malformed,
                    "backend record-offset collision",
                ));
            }
        }
        if backend.len() != self.entries.len() {
            return Err(ArchiveError::new(
                ErrorKind::Malformed,
                "ZIP parsers disagree about record count",
            ));
        }
        let mut total = 0u64;
        for entry in entries {
            control.check()?;
            self.entry(entry.id)?;
            if !matches!(entry.kind, EntryKind::File | EntryKind::Directory) {
                return Err(ArchiveError::new(
                    ErrorKind::UnsafeLink,
                    "only regular files and empty directories can be verified",
                ));
            }
            check_directory_payload(entry)?;
            if let Some(issue) = &entry.issue {
                return Err(ArchiveError::new(ErrorKind::UnsafePath, issue.clone()));
            }
            if !matches!(entry.compression, 0 | 8 | 99) {
                return Err(ArchiveError::new(
                    ErrorKind::Unsupported,
                    "ZIP compression method is not enabled",
                ));
            }
            if entry.encrypted && password.is_none() {
                return Err(ArchiveError::new(
                    ErrorKind::CredentialRequired,
                    "member needs a password",
                ));
            }
            let mut member_limits = self.limits.clone();
            member_limits.total_bytes =
                self.limits.total_bytes.checked_sub(total).ok_or_else(|| {
                    ArchiveError::new(ErrorKind::LimitExceeded, "operation total-byte budget")
                })?;
            check_size(
                entry.uncompressed_bytes,
                entry.compressed_bytes,
                &member_limits,
            )?;
            let index = *backend.get(&entry.id.central_offset).ok_or_else(|| {
                ArchiveError::new(
                    ErrorKind::Malformed,
                    "ZIP parsers disagree about record offsets",
                )
            })?;
            let options = ZipReadOptions::new()
                .password(password)
                .enforce_header_consistency(true);
            let mut member = archive
                .by_index_with_options(index, options)
                .map_err(zip_error)?;
            if member.size() != entry.uncompressed_bytes
                || member.compressed_size() != entry.compressed_bytes
            {
                return Err(ArchiveError::new(
                    ErrorKind::Malformed,
                    "ZIP parsers disagree about member sizes",
                ));
            }
            let actual = if entry.kind == EntryKind::Directory {
                // Empty directory records still need CRC/codec validation and,
                // when encrypted, password and full EOF authentication.
                copy_member(
                    &mut member,
                    &mut io::sink(),
                    entry.compressed_bytes,
                    &member_limits,
                    control,
                )?
            } else {
                consume(entry, &mut member, &member_limits)?
            };
            // Callers must consume through copy_member: the final EOF read is
            // necessary for authentication, and returning early is forbidden.
            let mut eof = [0u8; 1];
            if member
                .read(&mut eof)
                .map_err(|e| decode_error("verify member EOF", e))?
                != 0
                || actual != entry.uncompressed_bytes
            {
                return Err(ArchiveError::new(
                    ErrorKind::Integrity,
                    "member was not consumed to complete validated EOF",
                ));
            }
            total = total.checked_add(actual).ok_or_else(|| {
                ArchiveError::new(ErrorKind::LimitExceeded, "operation size overflow")
            })?;
        }
        drop(archive);
        self.stable(&mut file, control)?;
        Ok(total)
    }

    pub(crate) fn selection(&self, selected: &[ArchiveEntryId]) -> Result<Vec<&ArchiveEntry>> {
        self.selection_mode(selected, true, None)
    }
    pub(crate) fn selection_exact(
        &self,
        selected: &[ArchiveEntryId],
        control: &Control<'_>,
    ) -> Result<Vec<&ArchiveEntry>> {
        self.selection_mode(selected, false, Some(control))
    }
    fn selection_mode(
        &self,
        selected: &[ArchiveEntryId],
        recursive: bool,
        control: Option<&Control<'_>>,
    ) -> Result<Vec<&ArchiveEntry>> {
        let check = || control.map_or(Ok(()), Control::check);
        check()?;
        if self.ambiguous {
            return Err(ArchiveError::new(
                ErrorKind::AmbiguousEntry,
                "archive has duplicate effective paths",
            ));
        }
        for entry in &self.entries {
            check()?;
            if entry.issue.is_some() {
                return Err(ArchiveError::new(
                    ErrorKind::UnsafePath,
                    "extraction requires an archive with entirely valid member names",
                ));
            }
        }
        let mut chosen = HashSet::new();
        for &id in selected {
            check()?;
            let entry = self.entry(id)?;
            chosen.insert(id);
            if recursive
                && entry.kind == EntryKind::Directory
                && let Some(components) = &entry.components
            {
                for candidate in &self.entries {
                    check()?;
                    if candidate
                        .components
                        .as_ref()
                        .is_some_and(|path| path.starts_with(components))
                    {
                        chosen.insert(candidate.id);
                    }
                }
            }
        }
        let mut total = 0u64;
        let mut result = Vec::new();
        for entry in &self.entries {
            check()?;
            if chosen.contains(&entry.id) {
                result.push(entry);
            }
        }
        for entry in &result {
            check()?;
            if let Some(issue) = &entry.issue {
                return Err(ArchiveError::new(ErrorKind::UnsafePath, issue.clone()));
            }
            if matches!(entry.kind, EntryKind::Symlink | EntryKind::Special) {
                return Err(ArchiveError::new(
                    ErrorKind::UnsafeLink,
                    "links and special entries are not extracted",
                ));
            }
            check_directory_payload(entry)?;
            check_size(
                entry.uncompressed_bytes,
                entry.compressed_bytes,
                &self.limits,
            )?;
            total = total.checked_add(entry.uncompressed_bytes).ok_or_else(|| {
                ArchiveError::new(ErrorKind::LimitExceeded, "selected size overflow")
            })?;
            if total > self.limits.total_bytes {
                return Err(ArchiveError::new(
                    ErrorKind::LimitExceeded,
                    "selected total-byte budget",
                ));
            }
        }
        names::check_tree_control(
            result.iter().map(|e| {
                (
                    e.components.as_ref().unwrap().as_slice(),
                    e.kind == EntryKind::Directory,
                )
            }),
            self.limits.metadata_bytes,
            control,
        )?;
        Ok(result)
    }
}

fn digest_range(file: &mut File, start: u64, end: u64, control: &Control<'_>) -> Result<[u8; 32]> {
    file.seek(SeekFrom::Start(start))
        .map_err(|e| ArchiveError::io("seek directory digest", e))?;
    let mut remaining = end
        .checked_sub(start)
        .ok_or_else(|| ArchiveError::new(ErrorKind::Malformed, "invalid directory range"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 << 10];
    while remaining > 0 {
        control.check()?;
        let size = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..size])
            .map_err(|e| ArchiveError::io("read directory digest", e))?;
        hasher.update(&buffer[..size]);
        remaining -= size as u64;
    }
    Ok(hasher.finalize().into())
}

pub(crate) fn open_regular(path: &Path) -> Result<File> {
    #[cfg(target_os = "linux")]
    let file: File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| ArchiveError::io("open regular source", e.into()))?
    .into();
    #[cfg(not(target_os = "linux"))]
    let file = {
        if std::fs::symlink_metadata(path)
            .map_err(|e| ArchiveError::io("stat source", e))?
            .file_type()
            .is_symlink()
        {
            return Err(ArchiveError::new(
                ErrorKind::UnsafeLink,
                "symlink sources are unsupported",
            ));
        }
        File::open(path).map_err(|e| ArchiveError::io("open source", e))?
    };
    if !file
        .metadata()
        .map_err(|e| ArchiveError::io("stat source", e))?
        .is_file()
    {
        return Err(ArchiveError::new(
            ErrorKind::UnsafeLink,
            "source is not a regular file",
        ));
    }
    Ok(file)
}

pub(crate) fn check_size(actual: u64, compressed: u64, limits: &Limits) -> Result<()> {
    if actual > limits.total_bytes {
        return Err(ArchiveError::new(
            ErrorKind::LimitExceeded,
            "operation total-byte budget",
        ));
    }
    if actual > limits.member_bytes {
        return Err(ArchiveError::new(
            ErrorKind::LimitExceeded,
            "single-member byte budget",
        ));
    }
    if actual > (1 << 20) && actual > compressed.saturating_mul(limits.expansion_ratio) {
        return Err(ArchiveError::new(
            ErrorKind::LimitExceeded,
            "member expansion-ratio budget",
        ));
    }
    Ok(())
}

fn check_directory_payload(entry: &ArchiveEntry) -> Result<()> {
    if entry.kind == EntryKind::Directory && entry.uncompressed_bytes != 0 {
        return Err(ArchiveError::new(
            ErrorKind::Malformed,
            "payload-bearing directory records are unsupported",
        ));
    }
    Ok(())
}

pub(crate) fn copy_member(
    source: &mut dyn Read,
    destination: &mut dyn Write,
    compressed: u64,
    limits: &Limits,
    control: &Control<'_>,
) -> Result<u64> {
    let mut total = 0u64;
    let mut buffer = vec![0; 256 << 10];
    loop {
        control.check()?;
        let read = source
            .read(&mut buffer)
            .map_err(|e| decode_error("decode/check member integrity", e))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| ArchiveError::new(ErrorKind::LimitExceeded, "decoded size overflow"))?;
        check_size(total, compressed, limits)?;
        control.charge(read as u64)?;
        destination
            .write_all(&buffer[..read])
            .map_err(|e| ArchiveError::io("write member", e))?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct CancellingReader<'a> {
        cancel: &'a Cancellation,
        first: bool,
    }
    impl Read for CancellingReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                output[0] = b'x';
                self.cancel.cancel();
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }
    #[test]
    fn cancellation_after_partial_decode_is_not_successful_eof() {
        let cancel = Cancellation::default();
        let control = Control::new(&cancel, Duration::from_secs(1), 10);
        let mut input = CancellingReader {
            cancel: &cancel,
            first: true,
        };
        let mut output = Vec::new();
        let error =
            copy_member(&mut input, &mut output, 1, &Limits::default(), &control).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(output, b"x");
        assert_eq!(control.consumed_bytes(), 1);
    }

    #[test]
    fn streaming_total_limit_is_checked_before_emitting_each_chunk() {
        struct Fragmented(io::Cursor<Vec<u8>>);
        impl Read for Fragmented {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let bound = output.len().min(3);
                self.0.read(&mut output[..bound])
            }
        }
        let cancel = Cancellation::default();
        let control = Control::new(&cancel, Duration::from_secs(1), 100);
        let mut source = Fragmented(io::Cursor::new(b"0123456789".to_vec()));
        let mut output = Vec::new();
        let error = copy_member(
            &mut source,
            &mut output,
            10,
            &Limits {
                total_bytes: 5,
                ..Limits::default()
            },
            &control,
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::LimitExceeded);
        assert_eq!(output, b"012");
        assert!(output.len() <= 5);
    }
}
