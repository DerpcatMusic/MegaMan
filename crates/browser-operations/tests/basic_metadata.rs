#![cfg(target_os = "linux")]
use browser_operations::*;
use rustix::fs::{self as native, AtFlags, CWD, Timespec, Timestamps};
use std::{
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
fn writable_tree(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        for child in fs::read_dir(path).unwrap() {
            writable_tree(&child.unwrap().path());
        }
    }
}
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures-generated")
            .join(format!(
                "metadata-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn file(&self, name: &str, bytes: &[u8]) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn plan(&self) -> CopyPlan {
        plan_copy_new_with_options(
            self.path("source"),
            self.path("destination"),
            CopyLimits::default(),
            basic(),
            &Cancellation::default(),
        )
        .unwrap()
    }
    fn clean(&self) {
        assert!(!fs::read_dir(&self.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".browser-copy-")
        }));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        writable_tree(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn basic() -> CopyOptions {
    CopyOptions {
        metadata: MetadataPolicy::Basic,
        ..CopyOptions::default()
    }
}
fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn set_times(path: &Path) {
    native::utimensat(
        CWD,
        path,
        &Timestamps {
            last_access: Timespec {
                tv_sec: 1_600_000_000,
                tv_nsec: 123_456_789,
            },
            last_modification: Timespec {
                tv_sec: 1_600_000_001,
                tv_nsec: 987_654_321,
            },
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}
fn assert_times(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert_eq!(
        (metadata.atime(), metadata.atime_nsec()),
        (1_600_000_000, 123_456_789)
    );
    assert_eq!(
        (metadata.mtime(), metadata.mtime_nsec()),
        (1_600_000_001, 987_654_321)
    );
}
fn committed(outcome: CopyOutcome) -> CommitReceipt {
    match outcome {
        CopyOutcome::Committed(receipt) => receipt,
        other => panic!("{other:?}"),
    }
}
fn stage(f: &Fixture) -> PathBuf {
    fs::read_dir(&f.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .as_bytes()
                .starts_with(b".browser-copy-")
        })
        .unwrap()
}

#[test]
fn basic_preserves_executable_bits_readonly_directories_and_nanosecond_times_without_special_bits()
{
    let f = Fixture::new();
    f.file("source/readonly/tool", b"#!/bin/sh\nexit 0\n");
    set_mode(&f.path("source/readonly/tool"), 0o4751);
    set_times(&f.path("source/readonly/tool"));
    set_mode(&f.path("source/readonly"), 0o2555);
    set_times(&f.path("source/readonly"));
    set_mode(&f.path("source"), 0o1755);
    set_times(&f.path("source"));
    let receipt = committed(execute_copy_new(f.plan(), &Cancellation::default(), |_| {}));
    assert_eq!(
        receipt.metadata,
        MetadataCompletion::Achieved(MetadataPolicy::Basic)
    );
    assert_eq!(receipt.durability, Durability::Synced);
    assert!(matches!(receipt.cleanup, Cleanup::Clean));
    assert_eq!(
        fs::metadata(f.path("destination/readonly/tool"))
            .unwrap()
            .mode()
            & 0o7777,
        0o751
    );
    assert_eq!(
        fs::metadata(f.path("destination/readonly")).unwrap().mode() & 0o7777,
        0o555
    );
    assert_eq!(
        fs::metadata(f.path("destination")).unwrap().mode() & 0o7777,
        0o755
    );
    for name in [
        "destination",
        "destination/readonly",
        "destination/readonly/tool",
    ] {
        assert_times(&f.path(name));
    }
    assert_eq!(
        fs::read(f.path("destination/readonly/tool")).unwrap(),
        b"#!/bin/sh\nexit 0\n"
    );
    f.clean();
}

#[test]
fn symlink_timestamps_apply_to_link_without_touching_outside_target() {
    let f = Fixture::new();
    f.file("outside", b"outside");
    set_times(&f.path("outside"));
    symlink(f.path("outside"), f.path("source")).unwrap();
    set_times(&f.path("source"));
    let before = fs::metadata(f.path("outside")).unwrap();
    committed(execute_copy_new(f.plan(), &Cancellation::default(), |_| {}));
    assert_times(&f.path("destination"));
    assert!(
        fs::symlink_metadata(f.path("destination"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(f.path("destination")).unwrap(),
        f.path("outside")
    );
    let after = fs::metadata(f.path("outside")).unwrap();
    assert_eq!(
        (
            before.atime(),
            before.atime_nsec(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec()
        ),
        (
            after.atime(),
            after.atime_nsec(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        )
    );
    f.clean();
}

#[test]
fn cancellation_after_metadata_repairs_readonly_owned_dirs_and_leaves_source_unchanged() {
    let f = Fixture::new();
    f.file("source/readonly/file", b"ours");
    set_mode(&f.path("source/readonly"), 0o555);
    set_mode(&f.path("source"), 0o700);
    let cancel = Cancellation::default();
    let mut applied = false;
    let outcome = execute_copy_new(f.plan(), &cancel, |progress| {
        if progress.phase == Phase::MetadataApplied {
            applied = true;
            let root = stage(&f);
            assert_eq!(
                fs::metadata(root.join("payload")).unwrap().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(root.join("payload/readonly")).unwrap().mode() & 0o777,
                0o555
            );
            cancel.cancel();
        }
    });
    assert!(applied);
    assert!(matches!(
        outcome,
        CopyOutcome::Cancelled {
            cleanup: Cleanup::Clean
        }
    ));
    assert!(!f.path("destination").exists());
    f.clean();
    assert_eq!(
        fs::metadata(f.path("source")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(f.path("source/readonly")).unwrap().mode() & 0o777,
        0o555
    );
}

#[test]
fn metadata_callback_replacement_is_not_chmodded_or_published_and_residue_is_honest() {
    let f = Fixture::new();
    f.file("source/dir/file", b"ours");
    set_mode(&f.path("source"), 0o755);
    let outcome = execute_copy_new(f.plan(), &Cancellation::default(), |progress| {
        if progress.phase == Phase::MetadataApplied {
            let root = stage(&f);
            fs::rename(root.join("payload"), root.join("original-payload")).unwrap();
            fs::create_dir(root.join("payload")).unwrap();
            set_mode(&root.join("payload"), 0o555);
            fs::write(root.join("foreign-sentinel"), b"foreign").unwrap();
        }
    });
    match outcome {
        CopyOutcome::Failed {
            error,
            cleanup: Cleanup::RecoveryRequired(residue),
        } => {
            assert_eq!(error.kind, ErrorKind::DestinationChanged);
            assert!(residue.identity.is_some());
            assert_eq!(
                fs::metadata(residue.path.join("payload")).unwrap().mode() & 0o777,
                0o555
            );
            assert_eq!(
                fs::read(residue.path.join("foreign-sentinel")).unwrap(),
                b"foreign"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(!f.path("destination").exists());
}

#[test]
fn source_mode_and_time_changes_after_metadata_are_detected_before_commit() {
    for change_mode in [true, false] {
        let f = Fixture::new();
        f.file("source", b"ours");
        let outcome = execute_copy_new(f.plan(), &Cancellation::default(), |progress| {
            if progress.phase == Phase::MetadataApplied {
                if change_mode {
                    set_mode(&f.path("source"), 0o700);
                } else {
                    set_times(&f.path("source"));
                }
            }
        });
        assert!(
            matches!(outcome, CopyOutcome::Failed { ref error, cleanup: Cleanup::Clean } if error.kind == ErrorKind::SourceChanged)
        );
        assert!(!f.path("destination").exists());
        f.clean();
    }
}

#[test]
fn existing_rename_race_never_overwrites_a_competing_destination_after_metadata() {
    let f = Fixture::new();
    f.file("source", b"ours");
    set_mode(&f.path("source"), 0o755);
    let outcome = execute_copy_new(f.plan(), &Cancellation::default(), |progress| {
        if progress.phase == Phase::MetadataApplied {
            f.file("destination", b"foreign");
        }
    });
    assert!(
        matches!(outcome, CopyOutcome::Failed { ref error, cleanup: Cleanup::Clean } if error.kind == ErrorKind::DestinationConflict)
    );
    assert_eq!(fs::read(f.path("destination")).unwrap(), b"foreign");
    f.clean();
}

#[test]
fn basic_options_flow_through_copy_session_and_ignore_normal_umask_for_final_modes() {
    let f = Fixture::new();
    f.file("source/tool", b"executable");
    set_mode(&f.path("source/tool"), 0o751);
    set_times(&f.path("source/tool"));
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "umask 077; exec \"$1\" --exact metadata_umask_helper --nocapture",
            "copy-metadata-umask",
        ])
        .arg(std::env::current_exe().unwrap())
        .env("BROWSER_COPY_METADATA_FIXTURE", &f.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::metadata(f.path("out/source/tool")).unwrap().mode() & 0o7777,
        0o751
    );
    assert_times(&f.path("out/source/tool"));
}
#[test]
fn metadata_umask_helper() {
    let Some(root) = std::env::var_os("BROWSER_COPY_METADATA_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.starts_with(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated")
        )
    );
    fs::create_dir(root.join("out")).unwrap();
    let mut session = session::CopySession::new().unwrap();
    session
        .submit(
            session::CopyBatch::new(vec![root.join("source")], root.join("out"))
                .with_options(basic()),
        )
        .unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        session.poll();
        if session.terminal().is_some() {
            break;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        session.terminal().unwrap().stop,
        session::BatchStop::Complete
    );
}

#[test]
fn unsupported_basic_handle_budget_fails_admission_without_staging() {
    let f = Fixture::new();
    f.file("source", b"source");
    let error = plan_copy_new_with_options(
        f.path("source"),
        f.path("destination"),
        CopyLimits::default(),
        CopyOptions {
            metadata: MetadataPolicy::Basic,
            max_restricted_directory_handles: 129,
            ..CopyOptions::default()
        },
        &Cancellation::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::UnsupportedMetadata);
    assert!(!f.path("destination").exists());
    f.clean();
}

#[test]
fn basic_wide_readonly_copy_uses_bounded_descriptors_under_native_limit() {
    let f = Fixture::new();
    for index in 0..80 {
        f.file(&format!("source/d{index}/file"), b"payload");
        set_mode(&f.path(&format!("source/d{index}")), 0o555);
    }
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -n 32; exec \"$1\" --exact metadata_fd_helper --nocapture",
            "copy-metadata-fd",
        ])
        .arg(std::env::current_exe().unwrap())
        .env("BROWSER_COPY_METADATA_FIXTURE", &f.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_dir(f.path("destination")).unwrap().count(), 80);
    f.clean();
}
#[test]
fn metadata_fd_helper() {
    let Some(root) = std::env::var_os("BROWSER_COPY_METADATA_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.starts_with(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated")
        )
    );
    let plan = plan_copy_new_with_options(
        root.join("source"),
        root.join("destination"),
        CopyLimits::default(),
        basic(),
        &Cancellation::default(),
    )
    .unwrap();
    committed(execute_copy_new(plan, &Cancellation::default(), |_| {}));
}

#[test]
fn basic_non_owner_writable_directory_roots_are_refused_before_staging_and_private_still_works() {
    for mode in [0o400, 0o555] {
        let f = Fixture::new();
        fs::create_dir(f.path("source")).unwrap();
        if mode == 0o555 {
            f.file("source/child", b"read-only root payload");
        }
        set_mode(&f.path("source"), mode);
        let error = plan_copy_new_with_options(
            f.path("source"),
            f.path("destination"),
            CopyLimits::default(),
            basic(),
            &Cancellation::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnsupportedMetadata);
        assert_eq!(error.phase, "metadata admission");
        assert!(!f.path("destination").exists());
        f.clean();
        assert_eq!(fs::metadata(f.path("source")).unwrap().mode() & 0o777, mode);
        // Compatibility copying remains explicitly Private, rather than
        // silently pretending Basic preserved a writable-root downgrade.
        let plan = plan_copy_new(
            f.path("source"),
            f.path("destination"),
            CopyLimits::default(),
            &Cancellation::default(),
        )
        .unwrap();
        let receipt = committed(execute_copy_new(plan, &Cancellation::default(), |_| {}));
        assert_eq!(
            receipt.metadata,
            MetadataCompletion::Achieved(MetadataPolicy::Private)
        );
        assert_eq!(
            fs::metadata(f.path("destination")).unwrap().mode() & 0o777,
            0o700
        );
        f.clean();
    }
}

#[test]
fn supported_writable_root_can_contain_owner_readable_non_searchable_readonly_child() {
    let f = Fixture::new();
    fs::create_dir_all(f.path("source/empty-child")).unwrap();
    set_mode(&f.path("source"), 0o755);
    set_mode(&f.path("source/empty-child"), 0o400);
    set_times(&f.path("source/empty-child"));
    let receipt = committed(execute_copy_new(f.plan(), &Cancellation::default(), |_| {}));
    assert_eq!(
        receipt.metadata,
        MetadataCompletion::Achieved(MetadataPolicy::Basic)
    );
    assert_eq!(
        fs::metadata(f.path("destination/empty-child"))
            .unwrap()
            .mode()
            & 0o777,
        0o400
    );
    assert_times(&f.path("destination/empty-child"));
    f.clean();
}
