use crate::journal::{JournalPhase, Record};
use crate::*;
use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{self, AtFlags, CWD, FileType, Mode, OFlags, RawDir, RenameFlags, ResolveFlags, Stat},
    io::Errno,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    mem::MaybeUninit,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path},
};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);
fn identity(stat: &Stat) -> NativeIdentity {
    NativeIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    id: NativeIdentity,
    mode: u32,
    size: i64,
    modified: (i64, u64),
    changed: (i64, u64),
}
impl From<Stat> for Fingerprint {
    // Stat timestamp integer widths vary across Linux ABIs.
    #[allow(clippy::unnecessary_cast)]
    fn from(s: Stat) -> Self {
        Self {
            id: identity(&s),
            mode: s.st_mode,
            size: s.st_size,
            modified: (s.st_mtime as i64, s.st_mtime_nsec as u64),
            changed: (s.st_ctime as i64, s.st_ctime_nsec as u64),
        }
    }
}
#[allow(clippy::unnecessary_cast)]
fn accessed(stat: &Stat) -> (i64, u64) {
    (stat.st_atime as i64, stat.st_atime_nsec as u64)
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct MetadataSeal {
    fingerprint: Fingerprint,
    accessed: (i64, u64),
}
impl From<Stat> for MetadataSeal {
    fn from(stat: Stat) -> Self {
        Self {
            fingerprint: Fingerprint::from(stat),
            accessed: accessed(&stat),
        }
    }
}
fn metadata_changed(path: &Path) -> OperationError {
    OperationError::new(
        ErrorKind::DestinationChanged,
        "metadata validation",
        path,
        "owned staging identity or sealed metadata changed",
    )
}
fn bound_identity(fd: impl AsFd, expected: NativeIdentity, path: &Path) -> Result<()> {
    if identity(&fs::fstat(fd).map_err(|e| OperationError::io("metadata identity", path, e))?)
        != expected
    {
        return Err(metadata_changed(path));
    }
    Ok(())
}
fn apply_bound_metadata(fd: impl AsFd, entry: &Entry) -> Result<MetadataSeal> {
    let seal = apply_bound_metadata_without_sync(fd.as_fd(), entry, false)?;
    if entry.kind != EntryKind::Symlink {
        fs::fsync(fd.as_fd())
            .map_err(|e| OperationError::io("sync metadata", &entry.relative, e))?;
    }
    Ok(seal)
}
fn apply_bound_metadata_without_sync(
    fd: impl AsFd,
    entry: &Entry,
    finalizing: bool,
) -> Result<MetadataSeal> {
    let path = &entry.relative;
    let times = fs::Timestamps {
        last_access: fs::Timespec {
            tv_sec: entry.accessed.0,
            tv_nsec: entry
                .accessed
                .1
                .try_into()
                .map_err(|_| metadata_changed(path))?,
        },
        last_modification: fs::Timespec {
            tv_sec: entry.fingerprint.modified.0,
            tv_nsec: entry
                .fingerprint
                .modified
                .1
                .try_into()
                .map_err(|_| metadata_changed(path))?,
        },
    };
    if entry.kind == EntryKind::Symlink {
        // Linux >=5.8 permits a nofollow-bound O_PATH symlink descriptor here.
        fs::utimensat(
            fd.as_fd(),
            "",
            &times,
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|e| OperationError::io("symlink metadata times", path, e))?;
    } else {
        if finalizing {
            finalization_fault("finalize root mode", path)?;
        }
        fs::fchmod(
            fd.as_fd(),
            Mode::from_raw_mode(entry.fingerprint.mode & 0o777),
        )
        .map_err(|e| OperationError::io("metadata mode", path, e))?;
        if finalizing {
            finalization_fault("finalize root times", path)?;
        }
        fs::futimens(fd.as_fd(), &times)
            .map_err(|e| OperationError::io("metadata times", path, e))?;
    }
    if finalizing {
        finalization_fault("finalize root readback", path)?;
    }
    read_bound_metadata(fd, entry)
}
fn read_bound_metadata(fd: impl AsFd, entry: &Entry) -> Result<MetadataSeal> {
    let path = &entry.relative;
    let actual =
        fs::fstat(fd.as_fd()).map_err(|e| OperationError::io("metadata readback", path, e))?;
    if accessed(&actual) != entry.accessed
        || Fingerprint::from(actual).modified != entry.fingerprint.modified
        || (entry.kind != EntryKind::Symlink
            && actual.st_mode & 0o7777 != entry.fingerprint.mode & 0o777)
    {
        return Err(OperationError::new(
            ErrorKind::UnsupportedMetadata,
            "metadata readback",
            path,
            "filesystem did not preserve exact basic modes/timestamps",
        ));
    }
    Ok(MetadataSeal::from(actual))
}

// Deterministic syscall-boundary injection exists only in unit-test binaries.
#[cfg(test)]
thread_local! {
    static FINALIZATION_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
fn finalization_fault(phase: &'static str, path: &Path) -> Result<()> {
    #[cfg(test)]
    if FINALIZATION_FAULT.with(|fault| fault.get() == Some(phase)) {
        return Err(OperationError::io(phase, path, Errno::IO));
    }
    let _ = (phase, path);
    Ok(())
}

#[derive(Debug)]
struct Entry {
    relative: PathBuf,
    kind: EntryKind,
    fingerprint: Fingerprint,
    accessed: (i64, u64),
    link: Option<OsString>,
}
#[derive(Debug)]
pub(super) struct Plan {
    source_parent: OwnedFd,
    destination_parent: OwnedFd,
    source_root: Option<OwnedFd>,
    source_leaf: OsString,
    destination_leaf: OsString,
    entries: Vec<Entry>,
    limits: CopyLimits,
}

fn check(cancel: &Cancellation, phase: &'static str, path: &Path) -> Result<()> {
    if cancel.is_cancelled() {
        Err(OperationError::new(
            ErrorKind::Cancelled,
            phase,
            path,
            "cancelled",
        ))
    } else {
        Ok(())
    }
}
fn checked_path(path: &Path) -> Result<()> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
            _ => {
                return Err(OperationError::new(
                    ErrorKind::InvalidPath,
                    "path",
                    path,
                    "absolute normalized native path required; no '.' or '..'",
                ));
            }
        }
    }
    if !path.is_absolute()
        || path.file_name().is_none()
        || normalized.as_os_str() != path.as_os_str()
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(OperationError::new(
            ErrorKind::InvalidPath,
            "path",
            path,
            "absolute normalized non-root path required",
        ));
    }
    Ok(())
}
fn open_parent(path: &Path) -> Result<OwnedFd> {
    fs::openat2(
        CWD,
        path,
        DIR_FLAGS,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| OperationError::io("open parent without symlinks", path, e))
}
fn open_relative(dir: impl AsFd, path: &Path, flags: OFlags, mode: Mode) -> Result<OwnedFd> {
    fs::openat2(
        dir,
        path,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        mode,
        RESOLVE,
    )
    .map_err(|e| OperationError::io("open relative", path, e))
}
fn stat(dir: impl AsFd, name: &Path) -> Result<Stat> {
    fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|e| OperationError::io("stat", name, e))
}
fn absent(dir: impl AsFd, name: &Path) -> Result<()> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(()),
        Ok(_) => Err(OperationError::new(
            ErrorKind::DestinationConflict,
            "destination",
            name,
            "destination already exists; replacement is unsupported",
        )),
        Err(e) => Err(OperationError::io("destination stat", name, e)),
    }
}
fn kind(s: &Stat, path: &Path) -> Result<EntryKind> {
    match FileType::from_raw_mode(s.st_mode) {
        FileType::RegularFile => Ok(EntryKind::File),
        FileType::Directory => Ok(EntryKind::Directory),
        FileType::Symlink => Ok(EntryKind::Symlink),
        _ => Err(OperationError::new(
            ErrorKind::UnsupportedType,
            "plan",
            path,
            "special files are not copied",
        )),
    }
}
fn link(dir: impl AsFd, name: &Path, max: usize) -> Result<OsString> {
    let mut bytes = vec![0; max + 1];
    let len = fs::readlinkat_raw(dir, name, bytes.as_mut_slice())
        .map_err(|e| OperationError::io("read symlink", name, e))?;
    if len > max {
        return Err(OperationError::new(
            ErrorKind::LimitExceeded,
            "read symlink",
            name,
            "symlink target budget exceeded",
        ));
    }
    bytes.truncate(len);
    Ok(OsString::from_vec(bytes))
}
fn same(expected: &Fingerprint, actual: Stat, path: &Path) -> Result<()> {
    if *expected != Fingerprint::from(actual) {
        return Err(OperationError::new(
            ErrorKind::SourceChanged,
            "source validation",
            path,
            "source identity, type, length or modification evidence changed",
        ));
    }
    Ok(())
}

pub(super) fn plan(
    source: PathBuf,
    destination: PathBuf,
    limits: CopyLimits,
    options: CopyOptions,
    cancel: &Cancellation,
) -> Result<CopyPlan> {
    check(cancel, "plan", &source)?;
    if options.max_restricted_directory_handles > 128 {
        return Err(OperationError::new(
            ErrorKind::UnsupportedMetadata,
            "metadata admission",
            &source,
            "metadata handle budget exceeds hard cap128",
        ));
    }
    checked_path(&source)?;
    checked_path(&destination)?;
    if limits.max_depth > 128
        || limits.buffer_bytes == 0
        || limits.buffer_bytes > (1 << 20)
        || limits.max_symlink_bytes == 0
        || limits.max_symlink_bytes > (64 << 10)
    {
        return Err(OperationError::new(
            ErrorKind::LimitExceeded,
            "limits",
            &source,
            "invalid hard resource bound",
        ));
    }
    let source_parent = open_parent(source.parent().unwrap())?;
    let destination_parent = open_parent(destination.parent().unwrap())?;
    let source_leaf = source.file_name().unwrap().to_os_string();
    let destination_leaf = destination.file_name().unwrap().to_os_string();
    absent(&destination_parent, Path::new(&destination_leaf))?;
    let root_stat = stat(&source_parent, Path::new(&source_leaf))?;
    let root_kind = kind(&root_stat, &source)?;
    if options.metadata == MetadataPolicy::Basic
        && root_kind == EntryKind::Directory
        && root_stat.st_mode & 0o200 == 0
        && options.root_publication == RootPublication::MetadataBeforePublish
    {
        // Payload moves from its private staging parent into the destination
        // parent. Linux requires write permission on the moved directory for
        // that cross-parent '..' update. Keeping it writable would violate the
        // exact pre-publication Basic policy; refuse before any staging write.
        return Err(OperationError::new(
            ErrorKind::UnsupportedMetadata,
            "metadata admission",
            &source,
            "Basic directory roots without owner-write cannot be atomically published with final metadata; read-only children remain supported",
        ));
    }
    let source_root = if root_kind == EntryKind::Directory {
        let fd = open_relative(
            &source_parent,
            Path::new(&source_leaf),
            DIR_FLAGS,
            Mode::empty(),
        )?;
        same(
            &Fingerprint::from(root_stat),
            fs::fstat(&fd).map_err(|e| OperationError::io("fstat", &source, e))?,
            &source,
        )?;
        Some(fd)
    } else {
        None
    };
    let mut native = Plan {
        source_parent,
        destination_parent,
        source_root,
        source_leaf,
        destination_leaf,
        entries: Vec::new(),
        limits,
    };
    let mut summary = PlanSummary {
        source,
        destination,
        entries: 0,
        bytes: 0,
        name_bytes: 0,
        directories: 0,
        symlinks: 0,
    };
    let mut names = 0;
    let root_link = if root_kind == EntryKind::Symlink {
        Some(link(
            &native.source_parent,
            Path::new(&native.source_leaf),
            native.limits.max_symlink_bytes,
        )?)
    } else {
        None
    };
    push_entry(
        &mut native.entries,
        &mut summary,
        &mut names,
        &native.limits,
        Entry {
            relative: PathBuf::new(),
            kind: root_kind,
            fingerprint: Fingerprint::from(root_stat),
            accessed: accessed(&root_stat),
            link: root_link,
        },
    )?;
    if let Some(root) = &native.source_root {
        walk(
            root,
            &PathBuf::new(),
            0,
            &mut Walk {
                entries: &mut native.entries,
                summary: &mut summary,
                names: &mut names,
                limits: &native.limits,
                cancel,
            },
        )?;
        let destination_id =
            identity(&fs::fstat(&native.destination_parent).map_err(|e| {
                OperationError::io("destination identity", &summary.destination, e)
            })?);
        if native.entries.iter().any(|entry| {
            entry.kind == EntryKind::Directory && entry.fingerprint.id == destination_id
        }) {
            return Err(OperationError::new(
                ErrorKind::Containment,
                "plan",
                &summary.destination,
                "destination parent is inside source (including an inode alias)",
            ));
        }
    }
    validate_source(&native, &summary, cancel)?;
    summary.name_bytes = names;
    admit_metadata(&native.entries, options, &summary.source)?;
    Ok(CopyPlan {
        summary,
        options,
        native,
    })
}

fn needs_root_finalizer(entries: &[Entry], options: CopyOptions) -> bool {
    options.metadata == MetadataPolicy::Basic
        && options.root_publication == RootPublication::FinalizeBasicAfterPublish
        && entries[0].kind == EntryKind::Directory
        && entries[0].fingerprint.mode & 0o200 == 0
}
fn admit_metadata(entries: &[Entry], options: CopyOptions, source: &Path) -> Result<()> {
    if options.metadata == MetadataPolicy::Basic
        && entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                entry.kind == EntryKind::Directory
                    && (entry.fingerprint.mode & 0o400 == 0
                        || (*index == 0 && needs_root_finalizer(entries, options)))
            })
            .count()
            > options.max_restricted_directory_handles
    {
        return Err(OperationError::new(
            ErrorKind::UnsupportedMetadata,
            "metadata admission",
            source,
            "required restricted-directory handles exceed Basic policy budget",
        ));
    }
    Ok(())
}

fn push_entry(
    entries: &mut Vec<Entry>,
    summary: &mut PlanSummary,
    names: &mut usize,
    limits: &CopyLimits,
    entry: Entry,
) -> Result<()> {
    let path_bytes = entry.relative.as_os_str().as_bytes().len();
    let link_bytes = entry.link.as_ref().map_or(0, |s| s.as_bytes().len());
    *names = names
        .checked_add(path_bytes)
        .and_then(|n| n.checked_add(link_bytes))
        .ok_or_else(|| {
            OperationError::new(
                ErrorKind::LimitExceeded,
                "plan",
                &entry.relative,
                "path budget overflow",
            )
        })?;
    if entries.len() >= limits.max_entries || *names > limits.max_name_bytes {
        return Err(OperationError::new(
            ErrorKind::LimitExceeded,
            "plan",
            &entry.relative,
            "manifest count/path budget exceeded",
        ));
    }
    match entry.kind {
        EntryKind::File => {
            let len = u64::try_from(entry.fingerprint.size).map_err(|_| {
                OperationError::new(
                    ErrorKind::UnsupportedType,
                    "plan",
                    &entry.relative,
                    "negative file length",
                )
            })?;
            summary.bytes = summary
                .bytes
                .checked_add(len)
                .filter(|n| *n <= limits.max_bytes)
                .ok_or_else(|| {
                    OperationError::new(
                        ErrorKind::LimitExceeded,
                        "plan",
                        &entry.relative,
                        "payload byte budget exceeded",
                    )
                })?;
        }
        EntryKind::Directory => summary.directories += 1,
        EntryKind::Symlink => summary.symlinks += 1,
    }
    summary.entries += 1;
    entries.push(entry);
    Ok(())
}
struct Walk<'a> {
    entries: &'a mut Vec<Entry>,
    summary: &'a mut PlanSummary,
    names: &'a mut usize,
    limits: &'a CopyLimits,
    cancel: &'a Cancellation,
}
fn walk(dir: &OwnedFd, relative: &Path, depth: usize, context: &mut Walk<'_>) -> Result<()> {
    let before = Fingerprint::from(
        fs::fstat(dir).map_err(|e| OperationError::io("directory identity", relative, e))?,
    );
    let mut buffer = [MaybeUninit::uninit(); 8192];
    let mut listing = RawDir::new(dir, &mut buffer);
    while let Some(next) = listing.next() {
        check(context.cancel, "plan walk", relative)?;
        let next = next.map_err(|e| OperationError::io("read directory", relative, e))?;
        let name = next.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name = OsString::from_vec(name.to_bytes().to_vec());
        let child = relative.join(&name);
        if depth >= context.limits.max_depth {
            return Err(OperationError::new(
                ErrorKind::LimitExceeded,
                "plan walk",
                &child,
                "depth budget exceeded",
            ));
        }
        let s = stat(dir, Path::new(&name))?;
        let k = kind(&s, &child)?;
        let target = if k == EntryKind::Symlink {
            Some(link(
                dir,
                Path::new(&name),
                context.limits.max_symlink_bytes,
            )?)
        } else {
            None
        };
        push_entry(
            context.entries,
            context.summary,
            context.names,
            context.limits,
            Entry {
                relative: child.clone(),
                kind: k,
                fingerprint: Fingerprint::from(s),
                accessed: accessed(&s),
                link: target,
            },
        )?;
        if k == EntryKind::Directory {
            let child_fd = open_relative(dir, Path::new(&name), DIR_FLAGS, Mode::empty())?;
            same(
                &Fingerprint::from(s),
                fs::fstat(&child_fd)
                    .map_err(|e| OperationError::io("directory identity", &child, e))?,
                &child,
            )?;
            walk(&child_fd, &child, depth + 1, context)?;
        }
    }
    same(
        &before,
        fs::fstat(dir).map_err(|e| OperationError::io("directory validation", relative, e))?,
        relative,
    )
}

fn source_parent<'a>(
    plan: &'a Plan,
    entry: &Entry,
) -> Result<(Option<OwnedFd>, &'a OwnedFd, OsString)> {
    if entry.relative.as_os_str().is_empty() {
        return Ok((None, &plan.source_parent, plan.source_leaf.clone()));
    }
    let root = plan.source_root.as_ref().unwrap();
    let parent = entry.relative.parent().unwrap();
    let opened = if parent.as_os_str().is_empty() {
        None
    } else {
        Some(open_relative(root, parent, DIR_FLAGS, Mode::empty())?)
    };
    Ok((
        opened,
        root,
        entry.relative.file_name().unwrap().to_os_string(),
    ))
}
fn source_stat(plan: &Plan, entry: &Entry) -> Result<Stat> {
    let (opened, fallback, name) = source_parent(plan, entry)?;
    stat(opened.as_ref().unwrap_or(fallback), Path::new(&name))
}
fn bound_parent(path: &Path, fd: &OwnedFd, kind: ErrorKind) -> Result<()> {
    let reopened = open_parent(path).map_err(|_| {
        OperationError::new(
            kind,
            "parent validation",
            path,
            "bound parent path no longer resolves safely",
        )
    })?;
    let actual =
        fs::fstat(&reopened).map_err(|e| OperationError::io("parent identity", path, e))?;
    let expected = fs::fstat(fd).map_err(|e| OperationError::io("parent identity", path, e))?;
    if identity(&actual) != identity(&expected) {
        return Err(OperationError::new(
            kind,
            "parent validation",
            path,
            "parent identity changed",
        ));
    }
    Ok(())
}
fn validate_source(plan: &Plan, summary: &PlanSummary, cancel: &Cancellation) -> Result<()> {
    bound_parent(
        summary.source.parent().unwrap(),
        &plan.source_parent,
        ErrorKind::SourceChanged,
    )?;
    for entry in &plan.entries {
        check(cancel, "validate source", &entry.relative)?;
        same(
            &entry.fingerprint,
            source_stat(plan, entry)?,
            &entry.relative,
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct OwnedEntry {
    relative: PathBuf,
    kind: EntryKind,
    identity: NativeIdentity,
}
struct Stage {
    parent: OwnedFd,
    name: OsString,
    dir: OwnedFd,
    identity: NativeIdentity,
    owned: Vec<OwnedEntry>,
    path: PathBuf,
    cleaned: bool,
    operation_id: [u8; 16],
    destination_leaf: OsString,
    journal: Option<(File, NativeIdentity)>,
    basic: bool,
    root_metadata: Option<journal::RootMetadata>,
    pins: BTreeMap<usize, OwnedFd>,
}
impl Stage {
    fn new(
        plan: &Plan,
        destination: &Path,
        options: CopyOptions,
    ) -> std::result::Result<Self, Box<(OperationError, Cleanup)>> {
        let mut created = None;
        let result = (|| {
            let parent = open_relative(
                &plan.destination_parent,
                Path::new("."),
                DIR_FLAGS,
                Mode::empty(),
            )?;
            let mut random = [0; 16];
            let mut position = 0;
            while position < random.len() {
                let count = rustix::rand::getrandom(
                    &mut random[position..],
                    rustix::rand::GetRandomFlags::empty(),
                )
                .map_err(|e| OperationError::io("staging entropy", destination, e))?;
                if count == 0 {
                    return Err(OperationError::new(
                        ErrorKind::Io,
                        "staging entropy",
                        destination,
                        "no random bytes returned",
                    ));
                }
                position += count;
            }
            let name = OsString::from(format!(
                ".browser-copy-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ));
            fs::mkdirat(&parent, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                .map_err(|e| OperationError::io("create staging", destination, e))?;
            created = Some(destination.parent().unwrap().join(&name));
            // Keep private stage ownership bound to this parent even if its textual path moves.
            let dir = open_relative(&parent, Path::new(&name), DIR_FLAGS, Mode::empty())?;
            let id = identity(
                &fs::fstat(&dir)
                    .map_err(|e| OperationError::io("staging identity", destination, e))?,
            );
            if options.metadata == MetadataPolicy::Basic {
                fs::fchmod(&dir, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                    .map_err(|e| OperationError::io("private staging mode", destination, e))?;
            }
            Ok(Self {
                parent,
                name: name.clone(),
                dir,
                identity: id,
                owned: Vec::new(),
                path: destination.parent().unwrap().join(name),
                cleaned: false,
                operation_id: random,
                destination_leaf: plan.destination_leaf.clone(),
                journal: None,
                basic: options.metadata == MetadataPolicy::Basic,
                root_metadata: needs_root_finalizer(&plan.entries, options).then(|| {
                    let entry = &plan.entries[0];
                    journal::RootMetadata {
                        mode: entry.fingerprint.mode & 0o777,
                        accessed: (entry.accessed.0, entry.accessed.1 as u32),
                        modified: (
                            entry.fingerprint.modified.0,
                            entry.fingerprint.modified.1 as u32,
                        ),
                    }
                }),
                pins: BTreeMap::new(),
            })
        })();
        let mut stage = result.map_err(|error: OperationError| {
            let cleanup = match created {
                None => Cleanup::Clean,
                Some(path) => Cleanup::RecoveryRequired(Residue {
                    path,
                    identity: None,
                    error: error.clone(),
                }),
            };
            Box::new((error, cleanup))
        })?;
        if let Err(error) = stage.init_journal() {
            let cleanup = stage.cleanup();
            return Err(Box::new((error, cleanup)));
        }
        Ok(stage)
    }
    fn init_journal(&mut self) -> Result<()> {
        let fd = open_relative(
            &self.dir,
            Path::new("intent"),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )?;
        let id = identity(
            &fs::fstat(&fd).map_err(|e| OperationError::io("journal identity", &self.path, e))?,
        );
        self.journal = Some((File::from(fd), id));
        self.append_journal(JournalPhase::Staging, None)?;
        fs::fsync(&self.dir)
            .map_err(|e| OperationError::io("sync staging record", &self.path, e))?;
        fs::fsync(&self.parent)
            .map_err(|e| OperationError::io("sync staging parent", &self.path, e))?;
        Ok(())
    }
    fn append_journal(
        &mut self,
        phase: JournalPhase,
        payload_identity: Option<NativeIdentity>,
    ) -> Result<()> {
        let bytes = journal::encode(&Record {
            operation_id: self.operation_id,
            phase,
            destination_leaf: self.destination_leaf.clone(),
            payload_identity,
            root_metadata: self.root_metadata.clone(),
        })?;
        let (file, _) = self.journal.as_mut().unwrap();
        file.write_all(&bytes)
            .map_err(|e| OperationError::io("write commit journal", &self.path, e))?;
        file.sync_all()
            .map_err(|e| OperationError::io("sync commit journal", &self.path, e))
    }
    fn parent_for(&self, relative: &Path) -> Result<Option<OwnedFd>> {
        let parent = relative.parent().unwrap();
        if parent.as_os_str().is_empty() {
            Ok(None)
        } else {
            Ok(Some(open_relative(
                &self.dir,
                parent,
                DIR_FLAGS,
                Mode::empty(),
            )?))
        }
    }
    fn record(&mut self, path: PathBuf, kind: EntryKind) -> Result<()> {
        let id = identity(&stat(&self.dir, &path)?);
        self.owned.push(OwnedEntry {
            relative: path,
            kind,
            identity: id,
        });
        Ok(())
    }
    fn validate_anchor(&self) -> Result<()> {
        let journal_id = self.journal.as_ref().unwrap().1;
        if identity(&stat(&self.parent, Path::new(&self.name))?) != self.identity
            || identity(&stat(&self.dir, Path::new("intent"))?) != journal_id
        {
            return Err(OperationError::new(
                ErrorKind::DestinationChanged,
                "commit anchor",
                &self.path,
                "staging root or journal replaced",
            ));
        }
        Ok(())
    }
    fn apply_metadata(
        &mut self,
        plan: &Plan,
        sealed: &[Fingerprint],
        cancel: &Cancellation,
    ) -> Result<Vec<MetadataSeal>> {
        let mut output = self
            .owned
            .iter()
            .map(|owned| stat(&self.dir, &owned.relative).map(MetadataSeal::from))
            .collect::<Result<Vec<_>>>()?;
        for index in (0..self.owned.len()).rev() {
            let owned = &self.owned[index];
            let entry = &plan.entries[index];
            check(cancel, "apply metadata", &owned.relative)?;
            if Fingerprint::from(stat(&self.dir, &owned.relative)?) != sealed[index] {
                return Err(metadata_changed(&owned.relative));
            }
            let flags = if entry.kind == EntryKind::Symlink {
                OFlags::PATH
            } else if entry.kind == EntryKind::Directory {
                DIR_FLAGS
            } else {
                OFlags::RDONLY
            };
            let fd = open_relative(&self.dir, &owned.relative, flags, Mode::empty())?;
            bound_identity(&fd, owned.identity, &owned.relative)?;
            if Fingerprint::from(
                fs::fstat(&fd).map_err(|e| {
                    OperationError::io("metadata sealed binding", &owned.relative, e)
                })?,
            ) != sealed[index]
            {
                return Err(metadata_changed(&owned.relative));
            }
            if index == 0 && self.root_metadata.is_some() {
                // Admission charged this retained ordinary FD. Keep root0700
                // until cross-parent rename; children already have final metadata.
                self.pins.insert(index, fd);
            } else if entry.kind == EntryKind::Directory && entry.fingerprint.mode & 0o400 == 0 {
                // Retain before fchmod: even a later timestamp/sync error must
                // leave a bound handle capable of restoring owner access.
                self.pins.insert(index, fd);
                output[index] = apply_bound_metadata(self.pins.get(&index).unwrap(), entry)?;
            } else {
                output[index] = apply_bound_metadata(&fd, entry)?;
            }
        }
        Ok(output)
    }
    fn restore_directories(&self, expected: Option<&[MetadataSeal]>) -> Result<()> {
        // Preorder restoration gives each next parent owner search/write access.
        for (index, owned) in self
            .owned
            .iter()
            .enumerate()
            .filter(|(_, owned)| owned.kind == EntryKind::Directory)
        {
            let actual = stat(&self.dir, &owned.relative)?;
            if identity(&actual) != owned.identity {
                return Err(metadata_changed(&owned.relative));
            }
            if expected.is_some_and(|seals| seals[index] != MetadataSeal::from(actual)) {
                return Err(metadata_changed(&owned.relative));
            }
            if let Some(fd) = self.pins.get(&index) {
                bound_identity(fd, owned.identity, &owned.relative)?;
                fs::fchmod(fd, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|e| {
                    OperationError::io("restore owned directory access", &owned.relative, e)
                })?;
            } else {
                let fd = open_relative(&self.dir, &owned.relative, DIR_FLAGS, Mode::empty())?;
                bound_identity(&fd, owned.identity, &owned.relative)?;
                fs::fchmod(&fd, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|e| {
                    OperationError::io("restore owned directory access", &owned.relative, e)
                })?;
            }
        }
        Ok(())
    }
    fn validate_metadata_seals(&self, seals: &[MetadataSeal]) -> Result<()> {
        for (owned, seal) in self.owned.iter().zip(seals) {
            let actual = MetadataSeal::from(stat(&self.dir, &owned.relative)?);
            let mut expected = seal.clone();
            if owned.kind == EntryKind::Directory {
                expected.fingerprint.mode = (expected.fingerprint.mode & !0o7777) | 0o700;
                expected.fingerprint.changed = actual.fingerprint.changed; // intentional fchmod
            }
            if expected != actual {
                return Err(metadata_changed(&owned.relative));
            }
        }
        Ok(())
    }
    fn reapply_directories(&self, plan: &Plan, cancel: &Cancellation) -> Result<()> {
        for (index, owned) in self
            .owned
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, owned)| owned.kind == EntryKind::Directory)
        {
            check(cancel, "final directory metadata", &owned.relative)?;
            if index == 0 && self.root_metadata.is_some() {
                continue;
            }
            if let Some(fd) = self.pins.get(&index) {
                bound_identity(fd, owned.identity, &owned.relative)?;
                // Check the live pathname too: never use an old detached pin to
                // bless a foreign replacement at the destination location.
                if identity(&stat(&self.dir, &owned.relative)?) != owned.identity {
                    return Err(metadata_changed(&owned.relative));
                }
                apply_bound_metadata(fd, &plan.entries[index])?;
            } else {
                let fd = open_relative(&self.dir, &owned.relative, DIR_FLAGS, Mode::empty())?;
                bound_identity(&fd, owned.identity, &owned.relative)?;
                apply_bound_metadata(&fd, &plan.entries[index])?;
            }
        }
        fs::fsync(&self.dir).map_err(|e| OperationError::io("sync metadata staging", &self.path, e))
    }
    fn finalize_published_root(
        &self,
        plan: &Plan,
        destination: &Path,
        published: NativeIdentity,
    ) -> Result<()> {
        self.validate_published_binding(plan, destination, published)?;
        apply_bound_metadata_without_sync(self.pins.get(&0).unwrap(), &plan.entries[0], true)
            .map_err(|mut error| {
                error.path = destination.to_path_buf();
                error
            })?;
        self.validate_published_binding(plan, destination, published)
    }
    fn validate_published_binding(
        &self,
        plan: &Plan,
        destination: &Path,
        published: NativeIdentity,
    ) -> Result<()> {
        bound_parent(
            destination.parent().unwrap(),
            &plan.destination_parent,
            ErrorKind::DestinationChanged,
        )?;
        if identity(&stat(
            &plan.destination_parent,
            Path::new(&plan.destination_leaf),
        )?) != published
        {
            return Err(OperationError::new(
                ErrorKind::DestinationChanged,
                "finalize published binding",
                destination,
                "published root was replaced or removed",
            ));
        }
        bound_identity(self.pins.get(&0).unwrap(), published, destination)
    }
    fn cleanup(&mut self) -> Cleanup {
        let result = match self.clean() {
            Ok(()) => Cleanup::Clean,
            Err(error) => Cleanup::RecoveryRequired(Residue {
                path: self.path.clone(),
                identity: Some(self.identity),
                error,
            }),
        };
        // Explicit terminal cleanup is attempted once. Drop must not change a reported residue.
        self.cleaned = true;
        result
    }
    fn clean(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        if self.basic {
            self.restore_directories(None)?;
        }
        while let Some(entry) = self.owned.last() {
            let parent = self.parent_for(&entry.relative)?;
            let parent = parent.as_ref().unwrap_or(&self.dir);
            let name = Path::new(entry.relative.file_name().unwrap());
            let actual = stat(parent, name)?;
            if identity(&actual) != entry.identity {
                return Err(OperationError::new(
                    ErrorKind::DestinationChanged,
                    "cleanup",
                    &entry.relative,
                    "staging object replaced; refusing to remove it",
                ));
            }
            fs::unlinkat(
                parent,
                name,
                if entry.kind == EntryKind::Directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            )
            .map_err(|e| OperationError::io("cleanup", &entry.relative, e))?;
            self.owned.pop();
        }
        if let Some((_, id)) = &self.journal {
            if identity(&stat(&self.dir, Path::new("intent"))?) != *id {
                return Err(OperationError::new(
                    ErrorKind::DestinationChanged,
                    "cleanup journal",
                    &self.path,
                    "journal replaced; refusing to remove it",
                ));
            }
            fs::unlinkat(&self.dir, "intent", AtFlags::empty())
                .map_err(|e| OperationError::io("remove journal", &self.path, e))?;
            self.journal = None;
        }
        if identity(&stat(&self.parent, Path::new(&self.name))?) != self.identity {
            return Err(OperationError::new(
                ErrorKind::DestinationChanged,
                "cleanup",
                &self.path,
                "staging root replaced; refusing to remove it",
            ));
        }
        fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR)
            .map_err(|e| OperationError::io("remove staging", &self.path, e))?;
        fs::fsync(&self.parent)
            .map_err(|e| OperationError::io("sync staging cleanup", &self.path, e))?;
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.clean();
        }
    }
}

pub(super) fn execute(
    plan: CopyPlan,
    cancel: &Cancellation,
    mut progress: impl FnMut(Progress),
) -> CopyOutcome {
    let CopyPlan {
        summary,
        native: plan,
        options,
    } = plan;
    let prepare = (|| {
        check(cancel, "execute", &summary.source)?;
        validate_source(&plan, &summary, cancel)?;
        bound_parent(
            summary.destination.parent().unwrap(),
            &plan.destination_parent,
            ErrorKind::DestinationChanged,
        )?;
        absent(&plan.destination_parent, Path::new(&plan.destination_leaf))?;
        Ok(())
    })();
    if let Err(error) = prepare {
        return failure(error, Cleanup::Clean);
    }
    let mut stage = match Stage::new(&plan, &summary.destination, options) {
        Ok(stage) => stage,
        Err(error) => {
            let (error, cleanup) = *error;
            return failure(error, cleanup);
        }
    };
    let mut status = Progress {
        phase: Phase::Copying,
        completed_entries: 0,
        total_entries: summary.entries,
        copied_bytes: 0,
        verified_bytes: 0,
        total_bytes: summary.bytes,
    };
    let mut buffer = vec![0; plan.limits.buffer_bytes];
    let mut verify_buffer = vec![0; plan.limits.buffer_bytes];
    let result = (|| {
        for entry in &plan.entries {
            check(cancel, "copy", &entry.relative)?;
            same(
                &entry.fingerprint,
                source_stat(&plan, entry)?,
                &entry.relative,
            )?;
            let output_path = payload_path(entry);
            let parent = stage.parent_for(&output_path)?;
            let parent = parent.as_ref().unwrap_or(&stage.dir);
            let leaf = Path::new(output_path.file_name().unwrap());
            match entry.kind {
                EntryKind::Directory => {
                    fs::mkdirat(parent, leaf, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                        .map_err(|e| OperationError::io("create directory", &output_path, e))?;
                    stage.record(output_path.clone(), entry.kind)?;
                    if stage.basic {
                        let dir =
                            open_relative(&stage.dir, &output_path, DIR_FLAGS, Mode::empty())?;
                        fs::fchmod(&dir, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(|e| {
                            OperationError::io("private directory mode", &output_path, e)
                        })?;
                    }
                }
                EntryKind::Symlink => {
                    let (opened, fallback, name) = source_parent(&plan, entry)?;
                    let target = link(
                        opened.as_ref().unwrap_or(fallback),
                        Path::new(&name),
                        plan.limits.max_symlink_bytes,
                    )?;
                    if Some(&target) != entry.link.as_ref() {
                        return Err(OperationError::new(
                            ErrorKind::SourceChanged,
                            "copy symlink",
                            &entry.relative,
                            "symlink target changed",
                        ));
                    }
                    fs::symlinkat(&target, parent, leaf)
                        .map_err(|e| OperationError::io("copy symlink", &output_path, e))?;
                    stage.record(output_path, entry.kind)?;
                }
                EntryKind::File => {
                    let fd = open_relative(
                        parent,
                        leaf,
                        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                        Mode::RUSR | Mode::WUSR,
                    )?;
                    stage.record(output_path.clone(), entry.kind)?;
                    if stage.basic {
                        fs::fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(|e| {
                            OperationError::io("private file mode", &output_path, e)
                        })?;
                    }
                    let mut output = File::from(fd);
                    let mut input = source_file(&plan, entry)?;
                    let mut copied = 0u64;
                    loop {
                        check(cancel, "copy file", &entry.relative)?;
                        let count = input
                            .read(&mut buffer)
                            .map_err(|e| OperationError::io("read source", &entry.relative, e))?;
                        if count == 0 {
                            break;
                        }
                        copied = copied
                            .checked_add(count as u64)
                            .filter(|n| *n <= entry.fingerprint.size as u64)
                            .ok_or_else(|| {
                                OperationError::new(
                                    ErrorKind::SourceChanged,
                                    "copy file",
                                    &entry.relative,
                                    "source grew",
                                )
                            })?;
                        output
                            .write_all(&buffer[..count])
                            .map_err(|e| OperationError::io("write copy", &entry.relative, e))?;
                        status.copied_bytes += count as u64;
                        progress(status);
                    }
                    if copied != entry.fingerprint.size as u64 {
                        return Err(OperationError::new(
                            ErrorKind::SourceChanged,
                            "copy file",
                            &entry.relative,
                            "source shortened",
                        ));
                    }
                    same(
                        &entry.fingerprint,
                        fs::fstat(&input).map_err(|e| {
                            OperationError::io("source final metadata", &entry.relative, e)
                        })?,
                        &entry.relative,
                    )?;
                    output
                        .sync_all()
                        .map_err(|e| OperationError::io("sync file", &entry.relative, e))?;
                }
            }
            status.completed_entries += 1;
            progress(status);
        }
        status.phase = Phase::Verifying;
        status.completed_entries = 0;
        progress(status);
        for entry in &plan.entries {
            check(cancel, "verify", &entry.relative)?;
            let output_path = payload_path(entry);
            if entry.kind == EntryKind::File {
                let mut source = source_file(&plan, entry)?;
                let mut output = File::from(open_relative(
                    &stage.dir,
                    &output_path,
                    OFlags::RDONLY,
                    Mode::empty(),
                )?);
                let mut remaining = entry.fingerprint.size as u64;
                while remaining > 0 {
                    check(cancel, "verify", &entry.relative)?;
                    let count = remaining.min(buffer.len() as u64) as usize;
                    source.read_exact(&mut buffer[..count]).map_err(|e| {
                        OperationError::io("verify source read", &entry.relative, e)
                    })?;
                    output
                        .read_exact(&mut verify_buffer[..count])
                        .map_err(|e| OperationError::io("verify copy read", &entry.relative, e))?;
                    if buffer[..count] != verify_buffer[..count] {
                        return Err(OperationError::new(
                            ErrorKind::VerificationFailed,
                            "verify",
                            &entry.relative,
                            "payload readback differs from source",
                        ));
                    }
                    remaining -= count as u64;
                    status.verified_bytes += count as u64;
                    progress(status);
                }
                if source
                    .read(&mut buffer[..1])
                    .map_err(|e| OperationError::io("verify EOF", &entry.relative, e))?
                    != 0
                    || output
                        .read(&mut buffer[..1])
                        .map_err(|e| OperationError::io("verify EOF", &entry.relative, e))?
                        != 0
                {
                    return Err(OperationError::new(
                        ErrorKind::SourceChanged,
                        "verify",
                        &entry.relative,
                        "unexpected extra payload",
                    ));
                }
                same(
                    &entry.fingerprint,
                    fs::fstat(&source)
                        .map_err(|e| OperationError::io("verify metadata", &entry.relative, e))?,
                    &entry.relative,
                )?;
            }
            status.completed_entries += 1;
            progress(status);
        }
        validate_source(&plan, &summary, cancel)?;
        for owned in stage
            .owned
            .iter()
            .rev()
            .filter(|e| e.kind == EntryKind::Directory)
        {
            let dir = open_relative(&stage.dir, &owned.relative, DIR_FLAGS, Mode::empty())?;
            fs::fsync(&dir)
                .map_err(|e| OperationError::io("sync directory", &owned.relative, e))?;
        }
        fs::fsync(&stage.dir).map_err(|e| OperationError::io("sync staging", &stage.path, e))?;
        let sealed: Vec<_> = stage
            .owned
            .iter()
            .map(|entry| stat(&stage.dir, &entry.relative).map(Fingerprint::from))
            .collect::<Result<_>>()?;
        let published = identity(&stat(&stage.dir, Path::new("payload"))?);
        stage.append_journal(JournalPhase::Verified, Some(published))?;
        stage.append_journal(JournalPhase::CommitIntent, Some(published))?;
        status.phase = Phase::BeforeCommit;
        progress(status);
        check(cancel, "commit", &summary.destination)?;
        validate_source(&plan, &summary, cancel)?;
        bound_parent(
            summary.destination.parent().unwrap(),
            &plan.destination_parent,
            ErrorKind::DestinationChanged,
        )?;
        stage.validate_anchor()?;
        for (owned, seal) in stage.owned.iter().zip(&sealed) {
            if Fingerprint::from(stat(&stage.dir, &owned.relative)?) != *seal {
                return Err(OperationError::new(
                    ErrorKind::DestinationChanged,
                    "commit validation",
                    &owned.relative,
                    "staging object replaced",
                ));
            }
        }
        if stage.basic {
            let metadata_seals = stage.apply_metadata(&plan, &sealed, cancel)?;
            status.phase = Phase::MetadataApplied;
            progress(status);
            check(cancel, "metadata commit", &summary.destination)?;
            stage.restore_directories(Some(&metadata_seals))?;
            stage.validate_metadata_seals(&metadata_seals)?;
            // No callbacks from here through final metadata reapplication and
            // atomic publication. Directory repair intentionally changed ctime.
            stage.reapply_directories(&plan, cancel)?;
            validate_source(&plan, &summary, cancel)?;
            bound_parent(
                summary.destination.parent().unwrap(),
                &plan.destination_parent,
                ErrorKind::DestinationChanged,
            )?;
            stage.validate_anchor()?;
            if identity(&stat(&stage.dir, Path::new("payload"))?) != published {
                return Err(OperationError::new(
                    ErrorKind::DestinationChanged,
                    "metadata commit",
                    &summary.destination,
                    "payload root replaced",
                ));
            }
        }
        fs::renameat_with(
            &stage.dir,
            "payload",
            &plan.destination_parent,
            &plan.destination_leaf,
            RenameFlags::NOREPLACE,
        )
        .map_err(|e| {
            let mut error = OperationError::io("atomic publish", &summary.destination, e);
            error.kind = if e == Errno::EXIST || e == Errno::NOTEMPTY {
                ErrorKind::DestinationConflict
            } else if e == Errno::NOSYS
                || e == Errno::INVAL
                || e == Errno::OPNOTSUPP
                || e == Errno::XDEV
            {
                ErrorKind::AtomicCommitUnsupported
            } else {
                ErrorKind::Io
            };
            error
        })?;
        // Irreversible boundary: never remove the published destination, including late cancellation.
        stage.owned.clear();
        Ok(published)
    })();
    match result {
        Err(error) => {
            // Source validation is finished on this failure path. Release its
            // bound descriptors before repair so metadata pinning/EMFILE does
            // not consume cleanup's small descriptor headroom.
            drop(plan);
            let cleanup = stage.cleanup();
            failure(error, cleanup)
        }
        Ok(id) => {
            let mut warnings = Vec::new();
            let mut metadata = MetadataCompletion::Achieved(options.metadata);
            let mut journal_valid = true;
            if stage.root_metadata.is_some() {
                // Content is already committed. Never consult cancellation again:
                // root0700 is a working state, not an acceptable intentional result.
                let record = finalization_fault("record content publication", &summary.destination)
                    .and_then(|()| stage.append_journal(JournalPhase::ContentPublished, Some(id)));
                if let Err(error) = record {
                    journal_valid = false;
                    warnings.push(error);
                }
                status.phase = Phase::ContentPublished;
                progress(status);
                match stage.finalize_published_root(&plan, &summary.destination, id) {
                    Ok(()) => {
                        if journal_valid
                            && let Err(error) = finalization_fault(
                                "record finalized metadata",
                                &summary.destination,
                            )
                            .and_then(|()| {
                                stage.append_journal(JournalPhase::MetadataFinalized, Some(id))
                            })
                        {
                            journal_valid = false;
                            warnings.push(error);
                        }
                    }
                    Err(error) => {
                        metadata = MetadataCompletion::Incomplete {
                            requested: options.metadata,
                        };
                        warnings.push(error);
                    }
                }
            }
            if stage.root_metadata.is_some()
                && let Err(error) = finalization_fault("sync finalized root", &summary.destination)
                    .and_then(|()| {
                        fs::fsync(stage.pins.get(&0).unwrap()).map_err(|e| {
                            OperationError::io("sync finalized root", &summary.destination, e)
                        })
                    })
            {
                warnings.push(error);
            }
            // Even an incomplete published root needs a parent-sync attempt.
            let sync = if stage.root_metadata.is_some() {
                finalization_fault("sync committed parent", &summary.destination)
            } else {
                Ok(())
            };
            if let Err(error) = sync.and_then(|()| {
                fs::fsync(&plan.destination_parent).map_err(|e| {
                    OperationError::io("sync committed parent", &summary.destination, e)
                })
            }) {
                warnings.push(error);
            }
            if stage.root_metadata.is_some() && matches!(metadata, MetadataCompletion::Achieved(_))
            {
                let validation = stage
                    .validate_published_binding(&plan, &summary.destination, id)
                    .and_then(|()| {
                        read_bound_metadata(stage.pins.get(&0).unwrap(), &plan.entries[0])
                            .map(|_| ())
                    });
                if let Err(mut error) = validation {
                    error.path = summary.destination.clone();
                    metadata = MetadataCompletion::Incomplete {
                        requested: options.metadata,
                    };
                    warnings.push(error);
                }
            }
            if journal_valid
                && warnings.is_empty()
                && matches!(metadata, MetadataCompletion::Achieved(_))
                && let Err(error) = stage.append_journal(JournalPhase::Committed, Some(id))
            {
                warnings.push(error);
            }
            let durability = if warnings.is_empty() {
                Durability::Synced
            } else {
                Durability::Uncertain
            };
            let cleanup = if let Some(error) = warnings.first() {
                // Keep the only durable breadcrumb when publication succeeded but recording/sync
                // did not. Never turn an already-published object into a clean failure.
                stage.cleaned = true;
                Cleanup::RecoveryRequired(Residue {
                    path: stage.path.clone(),
                    identity: Some(stage.identity),
                    error: error.clone(),
                })
            } else {
                stage.cleanup()
            };
            status.phase = Phase::Committed;
            progress(status);
            CopyOutcome::Committed(CommitReceipt {
                metadata,
                operation_id: stage.operation_id,
                destination: summary.destination,
                identity: id,
                entries: summary.entries,
                bytes: summary.bytes,
                durability,
                warnings,
                cleanup,
            })
        }
    }
}
fn source_file(plan: &Plan, entry: &Entry) -> Result<File> {
    let (opened, fallback, name) = source_parent(plan, entry)?;
    // NONBLOCK prevents a raced-in FIFO/device open from hanging before fstat rejects it.
    let fd = open_relative(
        opened.as_ref().unwrap_or(fallback),
        Path::new(&name),
        OFlags::RDONLY | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    same(
        &entry.fingerprint,
        fs::fstat(&fd)
            .map_err(|e| OperationError::io("open source identity", &entry.relative, e))?,
        &entry.relative,
    )?;
    Ok(File::from(fd))
}
fn payload_path(entry: &Entry) -> PathBuf {
    if entry.relative.as_os_str().is_empty() {
        PathBuf::from("payload")
    } else {
        Path::new("payload").join(&entry.relative)
    }
}
fn failure(error: OperationError, cleanup: Cleanup) -> CopyOutcome {
    if error.kind == ErrorKind::Cancelled {
        CopyOutcome::Cancelled { cleanup }
    } else {
        CopyOutcome::Failed { error, cleanup }
    }
}

#[cfg(test)]
mod metadata_cleanup_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures-generated")
                .join(format!(
                    "metadata-stage-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("source"), b"source").unwrap();
            Self(path)
        }
        fn plan(&self) -> CopyPlan {
            plan_copy_new_with_options(
                self.0.join("source"),
                self.0.join("destination"),
                CopyLimits::default(),
                CopyOptions {
                    metadata: MetadataPolicy::Basic,
                    ..CopyOptions::default()
                },
                &Cancellation::default(),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn owned_tree(stage: &mut Stage, root_mode: u32, child_mode: u32) {
        for (index, (path, mode)) in [("payload", root_mode), ("payload/child", child_mode)]
            .into_iter()
            .enumerate()
        {
            fs::mkdirat(&stage.dir, path, Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap();
            stage
                .record(PathBuf::from(path), EntryKind::Directory)
                .unwrap();
            let fd = open_relative(&stage.dir, Path::new(path), DIR_FLAGS, Mode::empty()).unwrap();
            stage.pins.insert(index, fd);
            // Restrict only after both handles/descendants have been created.
            let _ = mode;
        }
        let fd = open_relative(
            &stage.dir,
            Path::new("payload/child/file"),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        stage
            .record(PathBuf::from("payload/child/file"), EntryKind::File)
            .unwrap();
        fs::fchmod(&fd, Mode::empty()).unwrap();
        fs::fchmod(stage.pins.get(&1).unwrap(), Mode::from_raw_mode(child_mode)).unwrap();
        fs::fchmod(stage.pins.get(&0).unwrap(), Mode::from_raw_mode(root_mode)).unwrap();
        if child_mode & 0o400 != 0 {
            stage.pins.remove(&1);
        }
        if root_mode & 0o400 != 0 {
            stage.pins.remove(&0);
        }
    }
    #[test]
    fn cleanup_repairs_owned_000_and_0555_directories_using_bound_capped_pins() {
        // A normal unprivileged process cannot read a 000 SOURCE tree. Exercise
        // the real owned-stage repair directly instead of pretending it can.
        for (root_mode, child_mode) in [(0, 0), (0, 0o555), (0o555, 0)] {
            let fixture = Fixture::new();
            let plan = fixture.plan();
            let mut stage =
                Stage::new(&plan.native, &plan.summary.destination, plan.options).unwrap();
            owned_tree(&mut stage, root_mode, child_mode);
            assert!(matches!(stage.cleanup(), Cleanup::Clean));
            assert!(!stage.path.exists());
            assert_eq!(std::fs::read(fixture.0.join("source")).unwrap(), b"source");
        }
    }
    #[test]
    fn repair_never_chmods_a_foreign_replacement_even_when_old_pin_is_000() {
        let fixture = Fixture::new();
        let plan = fixture.plan();
        let mut stage = Stage::new(&plan.native, &plan.summary.destination, plan.options).unwrap();
        owned_tree(&mut stage, 0, 0);
        fs::renameat(&stage.dir, "payload", &stage.dir, "old-payload").unwrap();
        fs::mkdirat(&stage.dir, "payload", Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap();
        let foreign =
            open_relative(&stage.dir, Path::new("payload"), DIR_FLAGS, Mode::empty()).unwrap();
        fs::fchmod(&foreign, Mode::from_raw_mode(0o555)).unwrap();
        assert!(matches!(stage.cleanup(), Cleanup::RecoveryRequired(_)));
        assert_eq!(fs::fstat(&foreign).unwrap().st_mode & 0o777, 0o555);
        assert_eq!(
            fs::fstat(stage.pins.get(&0).unwrap()).unwrap().st_mode & 0o777,
            0
        );
        // Fixture-only cleanup of the old known owned handles, after assertions.
        for fd in stage.pins.values() {
            fs::fchmod(fd, Mode::from_raw_mode(0o700)).unwrap();
        }
    }
    #[test]
    fn basic_pin_admission_counts_restricted_directories_before_stage_creation() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("tree")).unwrap();
        let mut plan = plan_copy_new(
            fixture.0.join("tree"),
            fixture.0.join("destination"),
            CopyLimits::default(),
            &Cancellation::default(),
        )
        .unwrap();
        // Model a manifest produced by a privileged reader; this test does not
        // claim an unprivileged process can enumerate a no-read source.
        plan.native.entries[0].fingerprint.mode &= !0o400;
        let options = CopyOptions {
            metadata: MetadataPolicy::Basic,
            max_restricted_directory_handles: 0,
            ..CopyOptions::default()
        };
        assert_eq!(
            admit_metadata(&plan.native.entries, options, &plan.summary.source)
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedMetadata
        );
        assert!(!fixture.0.join("destination").exists());
        assert!(!std::fs::read_dir(&fixture.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".browser-copy-")
        }));
    }
    #[test]
    fn published_finalization_faults_never_roll_back_and_sync_failures_keep_achieved_metadata() {
        use std::os::unix::fs::PermissionsExt;
        for phase in [
            "finalize root mode",
            "finalize root times",
            "finalize root readback",
            "record content publication",
            "record finalized metadata",
            "sync finalized root",
            "sync committed parent",
        ] {
            let fixture = Fixture::new();
            std::fs::remove_file(fixture.0.join("source")).unwrap();
            std::fs::create_dir(fixture.0.join("source")).unwrap();
            std::fs::set_permissions(
                fixture.0.join("source"),
                std::fs::Permissions::from_mode(0o555),
            )
            .unwrap();
            let plan = plan_copy_new_with_options(
                fixture.0.join("source"),
                fixture.0.join("destination"),
                CopyLimits::default(),
                CopyOptions {
                    metadata: MetadataPolicy::Basic,
                    root_publication: RootPublication::FinalizeBasicAfterPublish,
                    ..CopyOptions::default()
                },
                &Cancellation::default(),
            )
            .unwrap();
            FINALIZATION_FAULT.with(|fault| fault.set(Some(phase)));
            let outcome = execute_copy_new(plan, &Cancellation::default(), |_| {});
            FINALIZATION_FAULT.with(|fault| fault.set(None));
            let CopyOutcome::Committed(receipt) = outcome else {
                panic!("{phase}: {outcome:?}")
            };
            assert!(fixture.0.join("destination").is_dir(), "{phase}");
            assert_eq!(receipt.durability, Durability::Uncertain, "{phase}");
            assert_eq!(receipt.warnings.len(), 1, "{phase}");
            assert_eq!(receipt.warnings[0].phase, phase);
            let Cleanup::RecoveryRequired(residue) = receipt.cleanup else {
                panic!("{phase}")
            };
            let log =
                journal::decode(&std::fs::read(residue.path.join("intent")).unwrap()).unwrap();
            assert!(!log.torn_tail);
            assert!(
                log.records
                    .iter()
                    .all(|r| r.root_metadata.as_ref().unwrap().mode == 0o555)
            );
            if phase.starts_with("finalize root") {
                assert_eq!(
                    receipt.metadata,
                    MetadataCompletion::Incomplete {
                        requested: MetadataPolicy::Basic
                    }
                );
                assert_eq!(
                    log.records.last().unwrap().phase,
                    JournalPhase::ContentPublished
                );
            } else {
                assert_eq!(
                    receipt.metadata,
                    MetadataCompletion::Achieved(MetadataPolicy::Basic)
                );
                assert_eq!(
                    std::fs::metadata(fixture.0.join("destination"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o7777,
                    0o555
                );
                if phase == "record content publication" {
                    assert_eq!(
                        log.records.last().unwrap().phase,
                        JournalPhase::CommitIntent
                    );
                } else if phase == "record finalized metadata" {
                    assert_eq!(
                        log.records.last().unwrap().phase,
                        JournalPhase::ContentPublished
                    );
                } else {
                    assert_eq!(
                        log.records.last().unwrap().phase,
                        JournalPhase::MetadataFinalized
                    );
                }
            }
        }
    }
}
