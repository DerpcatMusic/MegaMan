use crate::{
    ArchiveEntryId, ArchiveError, ArchiveSession, Control, EntryKind, ErrorKind, Fingerprint,
    Limits, Result, names, open_regular,
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Commit {
    pub destination: PathBuf,
    pub entries: usize,
    pub bytes: u64,
}

/// Explicit, already-expanded sources. No recursive walk or symlink following is
/// hidden in creation. A directory entry has no native source path.
#[derive(Clone, Debug)]
pub struct CreateSource {
    pub source: Option<PathBuf>,
    pub archive_name: String,
}
impl CreateSource {
    pub fn file(source: impl Into<PathBuf>, archive_name: impl Into<String>) -> Self {
        Self {
            source: Some(source.into()),
            archive_name: archive_name.into(),
        }
    }
    pub fn directory(archive_name: impl Into<String>) -> Self {
        Self {
            source: None,
            archive_name: archive_name.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
    /// None = Stored, Some(0..=9) = Deflate. The default is Deflate level 6.
    pub deflate_level: Option<u8>,
    pub aes256: bool,
}
impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            deflate_level: Some(6),
            aes256: false,
        }
    }
}

/// Publish a new directory atomically on Linux, using no-replace rename. All
/// selected member paths retain their archive-relative prefix. Existing targets
/// are conflicts. No crash-recovery journal or merge/overwrite mode is claimed.
pub fn extract_new(
    session: &ArchiveSession,
    selected: &[ArchiveEntryId],
    destination: &Path,
    password: Option<&[u8]>,
    control: &Control<'_>,
) -> Result<Commit> {
    #[cfg(target_os = "linux")]
    {
        let entries = session.selection(selected)?;
        extract_entries_new(
            session,
            entries,
            destination,
            password,
            control,
            &|_| {},
            None,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (session, selected, destination, password, control);
        Err(ArchiveError::new(
            ErrorKind::AtomicCommitUnsupported,
            "atomic native publication is qualified only on Linux",
        ))
    }
}

pub(crate) fn extract_entries_new(
    session: &ArchiveSession,
    entries: Vec<&crate::ArchiveEntry>,
    destination: &Path,
    password: Option<&[u8]>,
    control: &Control<'_>,
    progress: &dyn Fn(crate::ExtractionPhase),
    policy: Option<&Limits>,
) -> Result<Commit> {
    #[cfg(target_os = "linux")]
    {
        control.check()?;
        session.check_source(control)?;
        let mut stage = native::Stage::new(destination, true)?;
        let result = (|| {
            for entry in &entries {
                if entry.kind == EntryKind::Directory {
                    stage.directory(entry.components.as_ref().unwrap())?;
                }
            }
            progress(crate::ExtractionPhase::Reading);
            let bytes =
                session.read_many(&entries, password, control, |entry, member, limits| {
                    let mut output = stage.file(entry.components.as_ref().unwrap())?;
                    #[cfg(test)]
                    if FAIL_EXTRACTION_WRITE.replace(false) {
                        output
                            .write_all(b"partial")
                            .map_err(|e| ArchiveError::io("injected partial write", e))?;
                        return Err(ArchiveError::io(
                            "injected disk full",
                            std::io::Error::from_raw_os_error(28),
                        ));
                    }
                    let mut effective = limits.clone();
                    if let Some(policy) = policy {
                        effective.member_bytes = effective.member_bytes.min(policy.member_bytes);
                        effective.total_bytes = effective.total_bytes.min(policy.total_bytes);
                        effective.expansion_ratio =
                            effective.expansion_ratio.min(policy.expansion_ratio);
                    }
                    let count = crate::copy_member(
                        member,
                        &mut output,
                        entry.compressed_bytes,
                        &effective,
                        control,
                    )?;
                    output
                        .sync_all()
                        .map_err(|e| ArchiveError::io("sync extracted member", e))?;
                    Ok(count)
                })?;
            session.check_source(control)?;
            progress(crate::ExtractionPhase::Syncing);
            stage.sync_directories()?;
            control.check()?;
            progress(crate::ExtractionPhase::Publishing);
            stage.publish()?;
            Ok(Commit {
                destination: destination.to_owned(),
                entries: entries.len(),
                bytes,
            })
        })();
        stage.finish(result)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            session,
            entries,
            destination,
            password,
            control,
            progress,
            policy,
        );
        Err(ArchiveError::new(
            ErrorKind::AtomicCommitUnsupported,
            "atomic native publication is qualified only on Linux",
        ))
    }
}

/// Create a new ZIP, then re-read every member and compare SHA-256 with the
/// streaming source digest before no-replace publication. Passwords are borrowed,
/// never logged/retained; the caller owns secret erasure.
pub fn create_new(
    sources: &[CreateSource],
    destination: &Path,
    options: &CreateOptions,
    password: Option<&[u8]>,
    limits: Limits,
    control: &Control<'_>,
) -> Result<Commit> {
    #[cfg(target_os = "linux")]
    {
        control.check()?;
        if sources.len() > limits.entries
            || sources.len().saturating_mul(2048) > limits.metadata_bytes
        {
            return Err(ArchiveError::new(
                ErrorKind::LimitExceeded,
                "creation manifest budget",
            ));
        }
        if options.deflate_level.is_some_and(|level| level > 9) {
            return Err(ArchiveError::new(
                ErrorKind::Unsupported,
                "Deflate level must be 0..=9",
            ));
        }
        if options.aes256 && password.is_none() {
            return Err(ArchiveError::new(
                ErrorKind::CredentialRequired,
                "AES-256 creation needs a password",
            ));
        }
        let mut manifest_bytes = 0usize;
        let mut paths = Vec::new();
        let mut names_seen = std::collections::HashSet::new();
        for source in sources {
            manifest_bytes = manifest_bytes
                .checked_add(2048 + source.archive_name.len().saturating_mul(12))
                .ok_or_else(|| {
                    ArchiveError::new(
                        ErrorKind::LimitExceeded,
                        "creation manifest arithmetic overflow",
                    )
                })?;
            if manifest_bytes > limits.metadata_bytes {
                return Err(ArchiveError::new(
                    ErrorKind::LimitExceeded,
                    "creation manifest-name budget",
                ));
            }
            let components = names::components(&source.archive_name, limits.path_components)?;
            if !names_seen.insert(components.clone()) {
                return Err(ArchiveError::new(
                    ErrorKind::Conflict,
                    "duplicate creation member",
                ));
            }
            paths.push(components);
        }
        names::check_tree(
            paths
                .iter()
                .zip(sources)
                .map(|(path, source)| (path.as_slice(), source.source.is_none())),
            limits.metadata_bytes,
        )?;
        let mut stage = native::Stage::new(destination, false)?;
        let result = (|| {
            let mut output = stage
                .root
                .try_clone()
                .map_err(|e| ArchiveError::io("clone staged ZIP", e))?;
            output
                .seek(SeekFrom::Start(0))
                .map_err(|e| ArchiveError::io("seek staged ZIP", e))?;
            let mut writer = zip::ZipWriter::new(output);
            let mut expected = Vec::new();
            let mut source_states = Vec::new();
            let mut bytes = 0u64;
            for (source, components) in sources.iter().zip(&paths) {
                control.check()?;
                let name = components.join("/");
                let method = if options.deflate_level.is_some() {
                    zip::CompressionMethod::Deflated
                } else {
                    zip::CompressionMethod::Stored
                };
                let mut file_options = zip::write::SimpleFileOptions::default()
                    .compression_method(method)
                    .compression_level(options.deflate_level.map(i64::from))
                    .unix_permissions(0o600);
                if let Some(path) = &source.source {
                    if options.aes256 {
                        file_options = file_options
                            .with_aes_encryption_bytes(zip::AesMode::Aes256, password.unwrap());
                    }
                    let mut input = open_regular(path)?;
                    let before = Fingerprint::of(
                        &input
                            .metadata()
                            .map_err(|e| ArchiveError::io("stat creation source", e))?,
                    );
                    if before.len > limits.member_bytes {
                        return Err(ArchiveError::new(
                            ErrorKind::LimitExceeded,
                            "creation member-byte budget",
                        ));
                    }
                    // Reserve ZIP64 before writing a possibly incompressible file.
                    file_options = file_options
                        .large_file(before.len >= (u32::MAX as u64).saturating_sub(1 << 20));
                    writer
                        .start_file(&name, file_options)
                        .map_err(crate::zip_error)?;
                    let mut digest = Sha256::new();
                    let mut actual = 0u64;
                    let mut buffer = vec![0; 256 << 10];
                    loop {
                        control.check()?;
                        let read = input
                            .read(&mut buffer)
                            .map_err(|e| ArchiveError::io("read creation source", e))?;
                        if read == 0 {
                            break;
                        }
                        actual = actual.checked_add(read as u64).ok_or_else(|| {
                            ArchiveError::new(ErrorKind::LimitExceeded, "creation size overflow")
                        })?;
                        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
                            ArchiveError::new(ErrorKind::LimitExceeded, "creation total overflow")
                        })?;
                        if actual > limits.member_bytes || bytes > limits.total_bytes {
                            return Err(ArchiveError::new(
                                ErrorKind::LimitExceeded,
                                "creation output budget",
                            ));
                        }
                        control.charge(read as u64)?;
                        digest.update(&buffer[..read]);
                        writer
                            .write_all(&buffer[..read])
                            .map_err(|e| ArchiveError::io("write ZIP payload", e))?;
                    }
                    if actual != before.len
                        || Fingerprint::of(
                            &input
                                .metadata()
                                .map_err(|e| ArchiveError::io("restat creation source", e))?,
                        ) != before
                    {
                        return Err(ArchiveError::new(
                            ErrorKind::SourceChanged,
                            "creation source changed while reading",
                        ));
                    }
                    source_states.push((path, before));
                    expected.push((name, Some(<[u8; 32]>::from(digest.finalize()))));
                } else {
                    writer
                        .add_directory(format!("{name}/"), file_options.unix_permissions(0o700))
                        .map_err(crate::zip_error)?;
                    expected.push((format!("{name}/"), None));
                }
            }
            let output = writer.finish().map_err(crate::zip_error)?;
            output
                .sync_all()
                .map_err(|e| ArchiveError::io("sync finished ZIP", e))?;
            drop(output);
            // Keep the parent descriptor alive while this proc-fd path binds
            // verification to the retained parent, even if its pathname moves.
            let verification_path = stage.handle_path();
            let verify = ArchiveSession::open(&verification_path, limits.clone(), control)?;
            if verify.entries().len() != expected.len() {
                return Err(ArchiveError::new(
                    ErrorKind::Integrity,
                    "created ZIP record count mismatch",
                ));
            }
            for (entry, (name, _)) in verify.entries().iter().zip(&expected) {
                if &entry.name != name {
                    return Err(ArchiveError::new(
                        ErrorKind::Integrity,
                        "created ZIP name mismatch",
                    ));
                }
            }
            let verification_entries: Vec<_> = verify.entries().iter().collect();
            verify.read_many(
                &verification_entries,
                password,
                control,
                |entry, member, limits| {
                    let expected_digest =
                        expected[entry.id.ordinal() as usize].1.ok_or_else(|| {
                            ArchiveError::new(
                                ErrorKind::Integrity,
                                "created file/directory mismatch",
                            )
                        })?;
                    let mut sink = HashSink(Sha256::new());
                    let count = crate::copy_member(
                        member,
                        &mut sink,
                        entry.compressed_bytes,
                        limits,
                        control,
                    )?;
                    let actual: [u8; 32] = sink.0.finalize().into();
                    if actual != expected_digest {
                        return Err(ArchiveError::new(
                            ErrorKind::Integrity,
                            "created ZIP source digest mismatch",
                        ));
                    }
                    Ok(count)
                },
            )?;
            drop(verify);
            for (path, before) in source_states {
                let current = std::fs::symlink_metadata(path)
                    .map_err(|e| ArchiveError::io("creation source final stat", e))?;
                if current.file_type().is_symlink() || Fingerprint::of(&current) != before {
                    return Err(ArchiveError::new(
                        ErrorKind::SourceChanged,
                        "creation source changed before commit",
                    ));
                }
            }
            control.check()?;
            stage.publish()?;
            Ok(Commit {
                destination: destination.to_owned(),
                entries: sources.len(),
                bytes,
            })
        })();
        stage.finish(result)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sources, destination, options, password, limits, control);
        Err(ArchiveError::new(
            ErrorKind::AtomicCommitUnsupported,
            "atomic native publication is qualified only on Linux",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static FAIL_EXTRACTION_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn inject_extraction_write_failure() {
    FAIL_EXTRACTION_WRITE.set(true);
}
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn inject_parent_sync_failure() {
    FAIL_PARENT_SYNC.set(true);
}

struct HashSink(Sha256);
impl Write for HashSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use rustix::fs::{self, AtFlags, Mode, OFlags, RenameFlags};
    use std::{
        ffi::{OsStr, OsString},
        os::{fd::AsRawFd, unix::fs::MetadataExt},
    };

    pub(super) struct Stage {
        pub root: File,
        parent: File,
        name: OsString,
        target: OsString,
        path: PathBuf,
        destination: PathBuf,
        directory: bool,
        live: bool,
        dirs: Vec<Vec<String>>,
        files: Vec<Vec<String>>,
        directory_identities: std::collections::HashMap<Vec<String>, (u64, u64)>,
    }
    fn err(context: &str, error: rustix::io::Errno) -> ArchiveError {
        ArchiveError::io(context, error.into())
    }
    fn open_dir(parent: &File, leaf: &OsStr) -> Result<File> {
        Ok(fs::openat(
            parent,
            leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| err("open no-follow staging directory", e))?
        .into())
    }
    impl Stage {
        pub fn new(destination: &Path, directory: bool) -> Result<Self> {
            let target = destination
                .file_name()
                .filter(|leaf| !leaf.is_empty())
                .ok_or_else(|| {
                    ArchiveError::new(ErrorKind::UnsafePath, "destination needs a new leaf name")
                })?
                .to_owned();
            let parent_path = destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent: File = fs::open(
                parent_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| err("bind destination parent", e))?
            .into();
            match fs::statat(&parent, &target, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => {
                    return Err(ArchiveError::new(
                        ErrorKind::Conflict,
                        "destination already exists",
                    ));
                }
                Err(rustix::io::Errno::NOENT) => {}
                Err(e) => return Err(err("check destination", e)),
            }
            let mut random = [0u8; 16];
            File::open("/dev/urandom")
                .and_then(|mut file| file.read_exact(&mut random))
                .map_err(|e| ArchiveError::io("staging name entropy", e))?;
            let name = OsString::from(format!(
                ".megaman-archive-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ));
            let path = parent_path.join(&name);
            let root = if directory {
                fs::mkdirat(&parent, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                    .map_err(|e| err("create private staging directory", e))?;
                match open_dir(&parent, &name) {
                    Ok(file) => file,
                    Err(mut error) => {
                        if fs::unlinkat(&parent, &name, AtFlags::REMOVEDIR).is_err() {
                            error.residue = Some(path);
                        }
                        return Err(error);
                    }
                }
            } else {
                fs::openat(
                    &parent,
                    &name,
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(|e| err("create private staging ZIP", e))?
                .into()
            };
            Ok(Self {
                root,
                parent,
                name,
                target,
                path,
                destination: destination.to_owned(),
                directory,
                live: true,
                dirs: Vec::new(),
                files: Vec::new(),
                directory_identities: std::collections::HashMap::new(),
            })
        }
        fn parent_for(&mut self, components: &[String], create: bool) -> Result<File> {
            let mut parent = self
                .root
                .try_clone()
                .map_err(|e| ArchiveError::io("clone staging directory", e))?;
            for length in 1..=components.len() {
                let leaf = OsStr::new(&components[length - 1]);
                let prefix = &components[..length];
                let mut created = false;
                if create {
                    match fs::mkdirat(&parent, leaf, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                        Ok(()) => {
                            self.dirs.push(prefix.to_vec());
                            created = true;
                        }
                        Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(err("create member parent", e)),
                    }
                }
                parent = open_dir(&parent, leaf)?;
                let metadata = parent
                    .metadata()
                    .map_err(|e| ArchiveError::io("stat staging member directory", e))?;
                let identity = (metadata.dev(), metadata.ino());
                if created {
                    self.directory_identities.insert(prefix.to_vec(), identity);
                } else if self.directory_identities.get(prefix) != Some(&identity) {
                    // A native case/normalization alias is not an existing exact
                    // prefix owned by this transaction. Neither are externally
                    // inserted or replaced directories, even if they are empty.
                    return Err(ArchiveError::new(
                        ErrorKind::Conflict,
                        "staging directory alias or identity change",
                    ));
                }
            }
            Ok(parent)
        }
        pub fn directory(&mut self, components: &[String]) -> Result<()> {
            self.parent_for(components, true).map(|_| ())
        }
        pub fn file(&mut self, components: &[String]) -> Result<File> {
            let (leaf, parents) = components
                .split_last()
                .ok_or_else(|| ArchiveError::new(ErrorKind::UnsafePath, "empty member path"))?;
            let parent = self.parent_for(parents, true)?;
            let output = fs::openat(
                &parent,
                leaf.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|e| err("exclusive no-follow member create", e))?;
            self.files.push(components.to_vec());
            Ok(output.into())
        }
        pub fn sync_directories(&mut self) -> Result<()> {
            let dirs = self.dirs.clone();
            for components in dirs.iter().rev() {
                self.parent_for(components, false)?
                    .sync_all()
                    .map_err(|e| ArchiveError::io("sync staging directory", e))?;
            }
            self.root
                .sync_all()
                .map_err(|e| ArchiveError::io("sync staging root", e))
        }
        pub fn handle_path(&self) -> PathBuf {
            PathBuf::from(format!("/proc/self/fd/{}", self.parent.as_raw_fd())).join(&self.name)
        }
        fn owns_name(&self) -> Result<()> {
            let current = fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| err("stat owned staging root", e))?;
            let owned = self
                .root
                .metadata()
                .map_err(|e| ArchiveError::io("stat owned staging handle", e))?;
            if current.st_dev != owned.dev() || current.st_ino != owned.ino() {
                return Err(ArchiveError::new(
                    ErrorKind::RecoveryRequired,
                    "staging pathname was replaced; refusing to touch replacement",
                ));
            }
            Ok(())
        }
        pub fn publish(&mut self) -> Result<()> {
            self.owns_name()?;
            fs::renameat_with(
                &self.parent,
                &self.name,
                &self.parent,
                &self.target,
                RenameFlags::NOREPLACE,
            )
            .map_err(|e| {
                if matches!(
                    e,
                    rustix::io::Errno::NOSYS
                        | rustix::io::Errno::INVAL
                        | rustix::io::Errno::OPNOTSUPP
                ) {
                    ArchiveError::new(
                        ErrorKind::AtomicCommitUnsupported,
                        "filesystem cannot atomically publish without replacement",
                    )
                } else {
                    err("atomic no-replace publication", e)
                }
            })?;
            self.live = false;
            #[cfg(test)]
            if FAIL_PARENT_SYNC.replace(false) {
                let mut error = ArchiveError::io(
                    "destination published but parent sync failed",
                    std::io::Error::from_raw_os_error(5),
                );
                error.published = Some(self.destination.clone());
                return Err(error);
            }
            self.parent.sync_all().map_err(|e| {
                let mut error = ArchiveError::io("destination published but parent sync failed", e);
                error.published = Some(self.destination.clone());
                error
            })
        }
        fn cleanup(&mut self) -> Result<()> {
            if !self.live {
                return Ok(());
            }
            self.owns_name()?;
            for components in self.files.clone().iter().rev() {
                let (leaf, parents) = components.split_last().unwrap();
                let parent = self.parent_for(parents, false)?;
                fs::unlinkat(&parent, leaf.as_str(), AtFlags::empty())
                    .map_err(|e| err("remove staged member", e))?;
            }
            for components in self.dirs.clone().iter().rev() {
                let (leaf, parents) = components.split_last().unwrap();
                let parent = self.parent_for(parents, false)?;
                fs::unlinkat(&parent, leaf.as_str(), AtFlags::REMOVEDIR)
                    .map_err(|e| err("remove staged directory", e))?;
            }
            self.owns_name()?;
            fs::unlinkat(
                &self.parent,
                &self.name,
                if self.directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            )
            .map_err(|e| err("remove staging root", e))?;
            self.live = false;
            Ok(())
        }
        pub fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
            match result {
                Ok(value) => Ok(value),
                Err(mut error) => {
                    if let Err(cleanup) = self.cleanup() {
                        error.message.push_str(&format!("; cleanup: {cleanup}"));
                        error.residue = Some(self.path.clone());
                    }
                    Err(error)
                }
            }
        }
    }
    impl Drop for Stage {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(".test-work")
                    .join(format!(
                        "native-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn competing_destination_creation_cannot_be_overwritten() {
            let fixture = Fixture::new();
            let destination = fixture.0.join("out");
            let mut stage = Stage::new(&destination, true).unwrap();
            stage
                .file(&["payload".into()])
                .unwrap()
                .write_all(b"new")
                .unwrap();
            std::fs::create_dir(&destination).unwrap();
            let result = stage.publish();
            assert_eq!(result.as_ref().unwrap_err().kind, ErrorKind::Conflict);
            assert!(stage.finish(result).is_err());
            assert!(destination.is_dir());
            assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
        }

        #[test]
        fn failure_after_partial_file_cleans_owned_tree() {
            let fixture = Fixture::new();
            let destination = fixture.0.join("out");
            let mut stage = Stage::new(&destination, true).unwrap();
            stage
                .file(&["dir".into(), "partial".into()])
                .unwrap()
                .write_all(b"partial")
                .unwrap();
            let error = stage
                .finish::<()>(Err(ArchiveError::io(
                    "injected disk full",
                    std::io::Error::from_raw_os_error(28),
                )))
                .unwrap_err();
            assert_eq!(error.io.unwrap().raw_os_error(), Some(28));
            assert!(error.residue.is_none());
            assert!(!destination.exists());
            assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
        }

        #[test]
        fn replacing_staging_name_does_not_delete_replacement() {
            let fixture = Fixture::new();
            let destination = fixture.0.join("out");
            let mut stage = Stage::new(&destination, true).unwrap();
            let moved = fixture.0.join("owned-moved");
            std::fs::rename(&stage.path, &moved).unwrap();
            std::fs::create_dir(&stage.path).unwrap();
            std::fs::write(stage.path.join("keep"), b"external").unwrap();
            let result = stage.publish();
            assert_eq!(
                result.as_ref().unwrap_err().kind,
                ErrorKind::RecoveryRequired
            );
            let error = stage.finish(result).unwrap_err();
            assert!(error.residue.is_some());
            assert_eq!(std::fs::read(stage.path.join("keep")).unwrap(), b"external");
        }

        #[test]
        fn member_parent_symlink_is_never_followed() {
            let fixture = Fixture::new();
            let destination = fixture.0.join("out");
            let outside = fixture.0.join("outside");
            std::fs::create_dir(&outside).unwrap();
            let mut stage = Stage::new(&destination, true).unwrap();
            std::os::unix::fs::symlink(&outside, stage.path.join("link")).unwrap();
            assert!(stage.file(&["link".into(), "escape".into()]).is_err());
            assert!(!outside.join("escape").exists());
            // Unowned unexpected files correctly require recovery; don't recurse
            // through or erase externally introduced objects during cleanup.
            let error = stage
                .finish::<()>(Err(ArchiveError::new(
                    ErrorKind::UnsafeLink,
                    "injected link",
                )))
                .unwrap_err();
            assert!(error.residue.is_some());
        }

        #[test]
        fn existing_directory_prefix_requires_exact_owned_identity() {
            let fixture = Fixture::new();
            let destination = fixture.0.join("out");
            let mut stage = Stage::new(&destination, true).unwrap();
            stage.file(&["owned".into(), "one".into()]).unwrap();
            stage.file(&["owned".into(), "two".into()]).unwrap();
            std::fs::create_dir(stage.path.join("unowned")).unwrap();
            assert_eq!(
                stage
                    .file(&["unowned".into(), "injected".into()])
                    .unwrap_err()
                    .kind,
                ErrorKind::Conflict
            );
            assert!(!stage.path.join("unowned/injected").exists());
            std::fs::remove_dir(stage.path.join("unowned")).unwrap();
            let result =
                stage.finish::<()>(Err(ArchiveError::new(ErrorKind::Cancelled, "test cancel")));
            assert!(result.unwrap_err().residue.is_none());
            assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 0);
        }
    }
}
