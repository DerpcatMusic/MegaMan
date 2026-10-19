#![cfg(target_os = "linux")]
use browser_operations::{journal, *};
use rustix::fs::{self as native, AtFlags, CWD, Timespec, Timestamps};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
fn writable(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        for child in fs::read_dir(path).unwrap() {
            writable(&child.unwrap().path());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        writable(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn options() -> CopyOptions {
    CopyOptions {
        metadata: MetadataPolicy::Basic,
        root_publication: RootPublication::FinalizeBasicAfterPublish,
        ..CopyOptions::default()
    }
}
fn mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn times(path: &Path) {
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
impl Fixture {
    fn new(root_mode: u32, tree: bool) -> Self {
        let f = Self(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures-generated")
                .join(format!(
                    "root-finalization-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                )),
        );
        fs::create_dir_all(f.path("source")).unwrap();
        if tree {
            fs::create_dir(f.path("source/readonly")).unwrap();
            fs::write(f.path("source/readonly/tool"), b"#!/bin/sh\nexit 0\n").unwrap();
            mode(&f.path("source/readonly/tool"), 0o4751);
            times(&f.path("source/readonly/tool"));
            mode(&f.path("source/readonly"), 0o555);
            times(&f.path("source/readonly"));
        }
        mode(&f.path("source"), root_mode);
        times(&f.path("source"));
        f
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn plan(&self) -> CopyPlan {
        plan_copy_new_with_options(
            self.path("source"),
            self.path("destination"),
            CopyLimits::default(),
            options(),
            &Cancellation::default(),
        )
        .unwrap()
    }
    fn journal(&self) -> journal::IntentLog {
        let stage = fs::read_dir(&self.0)
            .unwrap()
            .map(|p| p.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".browser-copy-")
            })
            .unwrap();
        journal::decode(&fs::read(stage.join("intent")).unwrap()).unwrap()
    }
    fn clean(&self) {
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), 2);
    }
}
fn receipt(outcome: CopyOutcome) -> CommitReceipt {
    match outcome {
        CopyOutcome::Committed(r) => r,
        other => panic!("{other:?}"),
    }
}
fn achieved(r: &CommitReceipt) {
    assert_eq!(
        r.metadata,
        MetadataCompletion::Achieved(MetadataPolicy::Basic)
    );
    assert_eq!(r.durability, Durability::Synced);
    assert!(matches!(r.cleanup, Cleanup::Clean));
    assert!(r.warnings.is_empty());
}
#[test]
fn explicit_finalizer_preserves_readonly_root_trees_empty_0400_and_exact_nanoseconds() {
    for (root_mode, tree) in [(0o4555, true), (0o400, false), (0o500, true)] {
        let f = Fixture::new(root_mode, tree);
        let mut published = false;
        let r = receipt(execute_copy_new(f.plan(), &Cancellation::default(), |p| {
            if p.phase == Phase::ContentPublished {
                published = true;
                assert_eq!(
                    fs::metadata(f.path("destination")).unwrap().mode() & 0o7777,
                    0o700
                );
                let log = f.journal();
                let record = log.records.last().unwrap();
                assert_eq!(record.phase, journal::JournalPhase::ContentPublished);
                let desired = record.root_metadata.as_ref().unwrap();
                assert_eq!(desired.mode, root_mode & 0o777);
                assert_eq!(desired.accessed, (1_600_000_000, 123_456_789));
                assert_eq!(desired.modified, (1_600_000_001, 987_654_321));
            }
        }));
        assert!(published);
        achieved(&r);
        let m = fs::metadata(f.path("destination")).unwrap();
        assert_eq!(m.mode() & 0o7777, root_mode & 0o777);
        assert_eq!((m.atime(), m.atime_nsec()), (1_600_000_000, 123_456_789));
        assert_eq!((m.mtime(), m.mtime_nsec()), (1_600_000_001, 987_654_321));
        assert_eq!(
            r.identity,
            NativeIdentity {
                device: m.dev(),
                inode: m.ino()
            }
        );
        if tree {
            assert_eq!(
                fs::read(f.path("destination/readonly/tool")).unwrap(),
                b"#!/bin/sh\nexit 0\n"
            );
            assert_eq!(
                fs::metadata(f.path("destination/readonly/tool"))
                    .unwrap()
                    .mode()
                    & 0o7777,
                0o751
            );
        }
        f.clean();
    }
}
#[test]
fn cancellation_before_publish_cleans_but_late_cancel_completes_mandatory_finalization() {
    for phase in [Phase::MetadataApplied, Phase::ContentPublished] {
        let f = Fixture::new(0o555, true);
        let cancel = Cancellation::default();
        let result = execute_copy_new(f.plan(), &cancel, |p| {
            if p.phase == phase {
                cancel.cancel();
            }
        });
        if phase == Phase::ContentPublished {
            achieved(&receipt(result));
            assert_eq!(
                fs::metadata(f.path("destination")).unwrap().mode() & 0o7777,
                0o555
            );
            f.clean();
        } else {
            assert!(matches!(
                result,
                CopyOutcome::Cancelled {
                    cleanup: Cleanup::Clean
                }
            ));
            assert!(!f.path("destination").exists());
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        }
        assert_eq!(
            fs::read(f.path("source/readonly/tool")).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
    }
}
#[test]
fn replaced_published_root_is_never_chmodded_removed_or_reported_basic_complete() {
    let f = Fixture::new(0o555, true);
    let r = receipt(execute_copy_new(f.plan(), &Cancellation::default(), |p| {
        if p.phase == Phase::ContentPublished {
            fs::rename(f.path("destination"), f.path("displaced")).unwrap();
            fs::create_dir(f.path("destination")).unwrap();
            fs::write(f.path("destination/foreign"), b"unrelated").unwrap();
            mode(&f.path("destination"), 0o711);
        }
    }));
    assert_eq!(
        r.metadata,
        MetadataCompletion::Incomplete {
            requested: MetadataPolicy::Basic
        }
    );
    assert_eq!(r.durability, Durability::Uncertain);
    assert!(matches!(r.cleanup, Cleanup::RecoveryRequired(_)));
    assert_eq!(r.warnings[0].kind, ErrorKind::DestinationChanged);
    assert_eq!(
        fs::read(f.path("destination/foreign")).unwrap(),
        b"unrelated"
    );
    assert_eq!(
        fs::metadata(f.path("destination")).unwrap().mode() & 0o777,
        0o711
    );
    assert_eq!(
        fs::metadata(f.path("displaced")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        f.journal().records.last().unwrap().phase,
        journal::JournalPhase::ContentPublished
    );
}
#[test]
fn source_mode_or_time_change_before_publication_fails_without_output() {
    for changed_mode in [false, true] {
        let f = Fixture::new(0o555, true);
        let result = execute_copy_new(f.plan(), &Cancellation::default(), |p| {
            if p.phase == Phase::MetadataApplied {
                if changed_mode {
                    mode(&f.path("source"), 0o755);
                } else {
                    std::fs::File::open(f.path("source"))
                        .unwrap()
                        .set_modified(std::time::SystemTime::now())
                        .unwrap();
                }
            }
        });
        assert!(matches!(
            result,
            CopyOutcome::Failed {
                error: OperationError {
                    kind: ErrorKind::SourceChanged,
                    ..
                },
                cleanup: Cleanup::Clean
            }
        ));
        assert!(!f.path("destination").exists());
    }
}
#[test]
fn finalizer_root_handle_is_charged_before_any_staging_write() {
    let f = Fixture::new(0o555, true);
    let result = plan_copy_new_with_options(
        f.path("source"),
        f.path("destination"),
        CopyLimits::default(),
        CopyOptions {
            max_restricted_directory_handles: 0,
            ..options()
        },
        &Cancellation::default(),
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::UnsupportedMetadata);
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
}
#[test]
fn readonly_finalization_copy_session_runs_under_umask_and_low_fd_limits() {
    for mask in ["022", "077"] {
        let f = Fixture::new(0o555, true);
        let result = std::process::Command::new("/bin/sh").args(["-c", "umask \"$1\"; ulimit -n 32; exec \"$2\" --exact finalization_process_helper --nocapture", "root-finalization", mask]).arg(std::env::current_exe().unwrap()).env("BROWSER_FINALIZATION_FIXTURE", &f.0).output().unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::metadata(f.path("out/source")).unwrap().mode() & 0o7777,
            0o555
        );
        assert_eq!(
            fs::read(f.path("out/source/readonly/tool")).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
    }
}
#[test]
fn finalization_process_helper() {
    let Some(path) = std::env::var_os("BROWSER_FINALIZATION_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated")
        )
    );
    fs::create_dir(path.join("out")).unwrap();
    let mut session = session::CopySession::new().unwrap();
    session
        .submit(
            session::CopyBatch::new(vec![path.join("source")], path.join("out"))
                .with_options(options()),
        )
        .unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while session.terminal().is_none() {
        session.poll();
        assert!(std::time::Instant::now() < end);
        std::thread::yield_now();
    }
    let result = session.terminal().unwrap();
    assert_eq!(result.stop, session::BatchStop::Complete);
    let session::RootOutcome::Committed(r) = &result.roots[0].outcome else {
        panic!("{result:?}")
    };
    achieved(r);
}

#[test]
fn native_non_utf8_root_paths_survive_finalization_journal_and_receipt() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new(0o555, true);
    let source =
        f.0.join(std::ffi::OsString::from_vec(b"source-\xff".to_vec()));
    let destination =
        f.0.join(std::ffi::OsString::from_vec(b"destination-\xfe".to_vec()));
    fs::rename(f.path("source"), &source).unwrap();
    let plan = plan_copy_new_with_options(
        source.clone(),
        destination.clone(),
        CopyLimits::default(),
        options(),
        &Cancellation::default(),
    )
    .unwrap();
    assert_eq!(plan.summary().source, source);
    let r = receipt(execute_copy_new(plan, &Cancellation::default(), |p| {
        if p.phase == Phase::ContentPublished {
            assert_eq!(
                f.journal().records.last().unwrap().destination_leaf,
                destination.file_name().unwrap()
            );
        }
    }));
    achieved(&r);
    assert_eq!(r.destination, destination);
    assert_eq!(
        fs::read(destination.join("readonly/tool")).unwrap(),
        b"#!/bin/sh\nexit 0\n"
    );
    f.clean();
}

#[test]
fn destination_parent_replacement_keeps_committed_identity_and_never_mutates_new_parent() {
    let f = Fixture::new(0o555, true);
    fs::create_dir(f.path("out")).unwrap();
    let plan = plan_copy_new_with_options(
        f.path("source"),
        f.path("out/copied"),
        CopyLimits::default(),
        options(),
        &Cancellation::default(),
    )
    .unwrap();
    let r = receipt(execute_copy_new(plan, &Cancellation::default(), |p| {
        if p.phase == Phase::ContentPublished {
            fs::rename(f.path("out"), f.path("moved-parent")).unwrap();
            fs::create_dir(f.path("out")).unwrap();
            fs::write(f.path("out/foreign"), b"foreign parent").unwrap();
        }
    }));
    assert_eq!(
        r.metadata,
        MetadataCompletion::Incomplete {
            requested: MetadataPolicy::Basic
        }
    );
    assert_eq!(r.durability, Durability::Uncertain);
    let Cleanup::RecoveryRequired(residue) = r.cleanup else {
        panic!("missing residue")
    };
    assert!(residue.identity.is_some());
    assert_eq!(r.warnings[0].kind, ErrorKind::DestinationChanged);
    assert_eq!(fs::read(f.path("out/foreign")).unwrap(), b"foreign parent");
    assert!(!f.path("out/copied").exists());
    let original = fs::metadata(f.path("moved-parent/copied")).unwrap();
    assert_eq!(
        r.identity,
        NativeIdentity {
            device: original.dev(),
            inode: original.ino()
        }
    );
    assert_eq!(original.mode() & 0o777, 0o700);
}

#[test]
fn process_exit_during_publication_retains_desired_metadata_breadcrumb_without_claiming_completion()
{
    let f = Fixture::new(0o555, true);
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "finalization_crash_helper", "--nocapture"])
        .env("BROWSER_FINALIZATION_CRASH_FIXTURE", &f.0)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::metadata(f.path("destination")).unwrap().mode() & 0o777,
        0o700
    );
    let log = f.journal();
    let record = log.records.last().unwrap();
    assert_eq!(record.phase, journal::JournalPhase::ContentPublished);
    assert_eq!(record.root_metadata.as_ref().unwrap().mode, 0o555);
    assert_eq!(
        record.payload_identity.unwrap().inode,
        fs::metadata(f.path("destination")).unwrap().ino()
    );
}
#[test]
fn finalization_crash_helper() {
    let Some(path) = std::env::var_os("BROWSER_FINALIZATION_CRASH_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated")
        )
    );
    let plan = plan_copy_new_with_options(
        path.join("source"),
        path.join("destination"),
        CopyLimits::default(),
        options(),
        &Cancellation::default(),
    )
    .unwrap();
    execute_copy_new(plan, &Cancellation::default(), |p| {
        if p.phase == Phase::ContentPublished {
            std::process::exit(73);
        }
    });
    panic!("expected ContentPublished boundary");
}
