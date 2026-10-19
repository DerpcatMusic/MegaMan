#![cfg(target_os = "linux")]
use browser_operations::*;
use std::{
    ffi::OsString,
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures-generated")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
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
    fn plan(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> CopyPlan {
        plan_copy_new(
            self.path(source),
            self.path(destination),
            CopyLimits::default(),
            &Cancellation::default(),
        )
        .unwrap()
    }
    fn no_stage(&self) {
        assert!(!fs::read_dir(&self.0).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".browser-copy-")
        }));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn committed(outcome: CopyOutcome) -> CommitReceipt {
    match outcome {
        CopyOutcome::Committed(receipt) => {
            assert!(matches!(receipt.cleanup, Cleanup::Clean));
            receipt
        }
        other => panic!("{other:?}"),
    }
}
fn failed_clean(outcome: CopyOutcome) -> OperationError {
    match outcome {
        CopyOutcome::Failed {
            error,
            cleanup: Cleanup::Clean,
        } => error,
        other => panic!("{other:?}"),
    }
}
fn cancelled_clean(outcome: CopyOutcome) {
    assert!(
        matches!(
            outcome,
            CopyOutcome::Cancelled {
                cleanup: Cleanup::Clean
            }
        ),
        "{outcome:?}"
    );
}

#[test]
fn copies_and_verifies_tree_native_names_empty_files_links_and_hardlinks() {
    let f = Fixture::new();
    let payload: Vec<_> = (0..131_117).map(|i| (i % 251) as u8).collect();
    f.file("source/nested/payload", &payload);
    f.file("source/empty", b"");
    fs::create_dir(f.path("source/empty-dir")).unwrap();
    let native = OsString::from_vec(b"native-\xff".to_vec());
    f.file(Path::new("source").join(&native), b"native");
    fs::hard_link(f.path("source/nested/payload"), f.path("source/hardlink")).unwrap();
    symlink("../missing", f.path("source/nested/broken")).unwrap();
    symlink("..", f.path("source/nested/cycle")).unwrap();
    symlink(f.path("outside"), f.path("source/absolute")).unwrap();
    f.file("outside/secret", b"outside");
    let plan = f.plan("source", "destination");
    assert_eq!(plan.summary().symlinks, 3);
    assert!(!f.path("destination").exists());
    f.no_stage();
    let mut previous = 0;
    let receipt = committed(execute_copy_new(plan, &Cancellation::default(), |p| {
        assert!(p.copied_bytes >= previous);
        previous = p.copied_bytes;
        if p.phase != Phase::Committed {
            assert!(!f.path("destination").exists());
        }
    }));
    assert_eq!(receipt.bytes, (payload.len() * 2 + 6) as u64);
    assert_eq!(receipt.durability, Durability::Synced);
    assert_eq!(
        fs::read(f.path("destination/nested/payload")).unwrap(),
        payload
    );
    assert_eq!(fs::read(f.path("destination/hardlink")).unwrap(), payload);
    assert_eq!(
        fs::read(f.path(Path::new("destination").join(native))).unwrap(),
        b"native"
    );
    assert!(f.path("destination/empty-dir").is_dir());
    assert_eq!(
        fs::read_link(f.path("destination/nested/broken")).unwrap(),
        Path::new("../missing")
    );
    assert_eq!(
        fs::read_link(f.path("destination/nested/cycle")).unwrap(),
        Path::new("..")
    );
    assert_eq!(
        fs::read_link(f.path("destination/absolute")).unwrap(),
        f.path("outside")
    );
    assert_eq!(fs::read(f.path("outside/secret")).unwrap(), b"outside");
    f.no_stage();
}

#[test]
fn copies_single_file_and_symlink_without_resolving_target() {
    let f = Fixture::new();
    f.file("source", b"hello");
    committed(execute_copy_new(
        f.plan("source", "copy"),
        &Cancellation::default(),
        |_| {},
    ));
    assert_eq!(fs::read(f.path("copy")).unwrap(), b"hello");
    symlink("missing-target", f.path("link")).unwrap();
    committed(execute_copy_new(
        f.plan("link", "link-copy"),
        &Cancellation::default(),
        |_| {},
    ));
    assert_eq!(
        fs::read_link(f.path("link-copy")).unwrap(),
        Path::new("missing-target")
    );
    assert_eq!(fs::read(f.path("source")).unwrap(), b"hello");
    let native_destination = OsString::from_vec(b"destination-\xfe".to_vec());
    committed(execute_copy_new(
        f.plan("source", &native_destination),
        &Cancellation::default(),
        |_| {},
    ));
    assert_eq!(fs::read(f.path(native_destination)).unwrap(), b"hello");
    f.no_stage();
}

#[test]
fn destination_race_never_overwrites_competitor() {
    let f = Fixture::new();
    f.file("source", b"ours");
    let plan = f.plan("source", "destination");
    let error = failed_clean(execute_copy_new(plan, &Cancellation::default(), |p| {
        if p.phase == Phase::BeforeCommit {
            f.file("destination", b"competitor");
        }
    }));
    assert_eq!(error.kind, ErrorKind::DestinationConflict);
    assert_eq!(fs::read(f.path("destination")).unwrap(), b"competitor");
    assert_eq!(fs::read(f.path("source")).unwrap(), b"ours");
    f.no_stage();
}

#[test]
fn existing_directory_file_and_dangling_link_are_conflicts_without_output() {
    let f = Fixture::new();
    f.file("source", b"source");
    f.file("existing-file", b"keep");
    fs::create_dir(f.path("existing-dir")).unwrap();
    symlink("missing", f.path("existing-link")).unwrap();
    for name in ["existing-file", "existing-dir", "existing-link"] {
        let error = plan_copy_new(
            f.path("source"),
            f.path(name),
            CopyLimits::default(),
            &Cancellation::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::DestinationConflict);
    }
    assert_eq!(fs::read(f.path("existing-file")).unwrap(), b"keep");
    f.no_stage();
}

#[test]
fn cancellation_at_each_precommit_phase_is_clean_and_source_remains() {
    let f = Fixture::new();
    let payload = vec![42; 131_111];
    f.file("source", &payload);
    for when in [Phase::Copying, Phase::Verifying, Phase::BeforeCommit] {
        let plan = f.plan("source", "destination");
        let cancel = Cancellation::default();
        cancelled_clean(execute_copy_new(plan, &cancel, |p| {
            if p.phase == when {
                cancel.cancel();
            }
        }));
        assert!(!f.path("destination").exists());
        assert_eq!(fs::read(f.path("source")).unwrap(), payload);
        f.no_stage();
    }
    let plan = f.plan("source", "destination");
    let cancel = Cancellation::default();
    cancel.cancel();
    cancelled_clean(execute_copy_new(plan, &cancel, |_| {
        panic!("already cancelled")
    }));
    f.no_stage();
    assert_eq!(
        plan_copy_new(
            f.path("source"),
            f.path("destination"),
            CopyLimits::default(),
            &cancel
        )
        .unwrap_err()
        .kind,
        ErrorKind::Cancelled
    );
}

#[test]
fn cancellation_after_publication_reports_committed_without_rollback() {
    let f = Fixture::new();
    f.file("source", b"keep committed");
    let cancel = Cancellation::default();
    committed(execute_copy_new(
        f.plan("source", "destination"),
        &cancel,
        |p| {
            if p.phase == Phase::Committed {
                cancel.cancel();
            }
        },
    ));
    assert!(cancel.is_cancelled());
    assert_eq!(fs::read(f.path("destination")).unwrap(), b"keep committed");
    f.no_stage();
}

#[test]
fn manifest_rejects_mutation_replacement_and_symlink_swap_after_planning() {
    let f = Fixture::new();
    f.file("source", b"original");
    let plan = f.plan("source", "destination");
    f.file("source", b"modified");
    assert_eq!(
        failed_clean(execute_copy_new(plan, &Cancellation::default(), |_| {})).kind,
        ErrorKind::SourceChanged
    );
    let plan = f.plan("source", "destination");
    fs::rename(f.path("source"), f.path("old-source")).unwrap();
    f.file("source", b"modified");
    assert_eq!(
        failed_clean(execute_copy_new(plan, &Cancellation::default(), |_| {})).kind,
        ErrorKind::SourceChanged
    );
    let plan = f.plan("source", "destination");
    fs::remove_file(f.path("source")).unwrap();
    symlink("old-source", f.path("source")).unwrap();
    assert_eq!(
        failed_clean(execute_copy_new(plan, &Cancellation::default(), |_| {})).kind,
        ErrorKind::SourceChanged
    );
    assert!(!f.path("destination").exists());
    f.no_stage();
}

#[test]
fn mutation_during_copy_fails_without_publishing() {
    let f = Fixture::new();
    f.file("source", &vec![9; 131_111]);
    let mut changed = false;
    let error = failed_clean(execute_copy_new(
        f.plan("source", "destination"),
        &Cancellation::default(),
        |p| {
            if p.phase == Phase::Copying && p.copied_bytes > 0 && !changed {
                f.file("source", b"changed concurrently");
                changed = true;
            }
        },
    ));
    assert_eq!(error.kind, ErrorKind::SourceChanged);
    assert!(!f.path("destination").exists());
    f.no_stage();
}

#[test]
fn parent_replacement_cannot_redirect_writes_and_cleanup_uses_bound_handle() {
    let f = Fixture::new();
    f.file("source", b"payload");
    fs::create_dir(f.path("target")).unwrap();
    fs::create_dir(f.path("outside")).unwrap();
    let error = failed_clean(execute_copy_new(
        f.plan("source", "target/destination"),
        &Cancellation::default(),
        |p| {
            if p.phase == Phase::BeforeCommit {
                fs::rename(f.path("target"), f.path("moved-parent")).unwrap();
                symlink("outside", f.path("target")).unwrap();
            }
        },
    ));
    assert_eq!(error.kind, ErrorKind::DestinationChanged);
    assert_eq!(fs::read_dir(f.path("outside")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(f.path("moved-parent")).unwrap().count(), 0);
    assert_eq!(fs::read(f.path("source")).unwrap(), b"payload");
}

#[test]
fn containment_parent_symlinks_parent_components_and_special_files_are_refused() {
    let f = Fixture::new();
    f.file("source/sub/file", b"data");
    let error = plan_copy_new(
        f.path("source"),
        f.path("source/sub/copy"),
        CopyLimits::default(),
        &Cancellation::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Containment);
    symlink("source", f.path("alias")).unwrap();
    for (source, target) in [
        (f.path("alias/sub/file"), f.path("copy")),
        (f.path("source/sub/file"), f.path("alias/copy")),
    ] {
        assert!(
            plan_copy_new(
                source,
                target,
                CopyLimits::default(),
                &Cancellation::default()
            )
            .is_err()
        );
    }
    for source in [
        f.path("source/../source"),
        f.path("source/"),
        PathBuf::from("relative"),
    ] {
        assert_eq!(
            plan_copy_new(
                source,
                f.path("copy"),
                CopyLimits::default(),
                &Cancellation::default()
            )
            .unwrap_err()
            .kind,
            ErrorKind::InvalidPath
        );
    }
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.path("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR,
        0,
    )
    .unwrap();
    assert_eq!(
        plan_copy_new(
            f.path("fifo"),
            f.path("copy"),
            CopyLimits::default(),
            &Cancellation::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::UnsupportedType
    );
    f.no_stage();
}

#[test]
fn hard_resource_limits_fail_before_staging() {
    let f = Fixture::new();
    f.file("source/a/b/file", b"payload");
    symlink("long-target", f.path("source/link")).unwrap();
    for limits in [
        CopyLimits {
            max_entries: 1,
            ..CopyLimits::default()
        },
        CopyLimits {
            max_bytes: 1,
            ..CopyLimits::default()
        },
        CopyLimits {
            max_name_bytes: 1,
            ..CopyLimits::default()
        },
        CopyLimits {
            max_depth: 1,
            ..CopyLimits::default()
        },
        CopyLimits {
            buffer_bytes: 0,
            ..CopyLimits::default()
        },
        CopyLimits {
            max_symlink_bytes: 1,
            ..CopyLimits::default()
        },
    ] {
        assert_eq!(
            plan_copy_new(
                f.path("source"),
                f.path("copy"),
                limits,
                &Cancellation::default()
            )
            .unwrap_err()
            .kind,
            ErrorKind::LimitExceeded
        );
        assert!(!f.path("copy").exists());
        f.no_stage();
    }
}

#[test]
fn source_open_failure_cleans_staging_without_false_success() {
    let f = Fixture::new();
    f.file("source", b"unreadable");
    fs::set_permissions(f.path("source"), fs::Permissions::from_mode(0o0)).unwrap();
    let plan = f.plan("source", "copy");
    let outcome = execute_copy_new(plan, &Cancellation::default(), |_| {});
    fs::set_permissions(f.path("source"), fs::Permissions::from_mode(0o600)).unwrap();
    let error = failed_clean(outcome);
    assert_eq!(error.io_kind, Some(std::io::ErrorKind::PermissionDenied));
    assert_eq!(fs::read(f.path("source")).unwrap(), b"unreadable");
    assert!(!f.path("copy").exists());
    f.no_stage();
}

#[test]
fn staging_mutation_before_commit_is_refused_and_foreign_replacement_not_deleted() {
    let f = Fixture::new();
    f.file("source", b"ours");
    let outcome = execute_copy_new(
        f.plan("source", "destination"),
        &Cancellation::default(),
        |p| {
            if p.phase == Phase::BeforeCommit {
                let stage = fs::read_dir(&f.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .as_encoded_bytes()
                            .starts_with(b".browser-copy-")
                    })
                    .unwrap();
                fs::rename(stage.join("payload"), stage.join("original-payload")).unwrap();
                fs::write(stage.join("payload"), b"foreign").unwrap();
            }
        },
    );
    match outcome {
        CopyOutcome::Failed {
            error,
            cleanup: Cleanup::RecoveryRequired(residue),
        } => {
            assert_eq!(error.kind, ErrorKind::DestinationChanged);
            assert_eq!(fs::read(residue.path.join("payload")).unwrap(), b"foreign");
            assert_eq!(
                fs::read(residue.path.join("original-payload")).unwrap(),
                b"ours"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(!f.path("destination").exists());
    assert_eq!(fs::read(f.path("source")).unwrap(), b"ours");
}

#[test]
fn copied_payload_readback_detects_corruption_and_checks_commit_intent_identity() {
    use browser_operations::journal::{JournalPhase, MAX_LOG_BYTES, decode};
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    f.file("source", b"correct payload");
    let mut corrupted = false;
    let outcome = execute_copy_new(
        f.plan("source", "destination"),
        &Cancellation::default(),
        |p| {
            if p.phase == Phase::Verifying && !corrupted {
                let stage = fs::read_dir(&f.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .as_encoded_bytes()
                            .starts_with(b".browser-copy-")
                    })
                    .unwrap();
                fs::write(stage.join("payload"), b"wrong!! payload").unwrap();
                corrupted = true;
            }
        },
    );
    assert_eq!(failed_clean(outcome).kind, ErrorKind::VerificationFailed);
    f.no_stage();
    committed(execute_copy_new(
        f.plan("source", "destination"),
        &Cancellation::default(),
        |p| {
            if p.phase == Phase::BeforeCommit {
                let stage = fs::read_dir(&f.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .as_encoded_bytes()
                            .starts_with(b".browser-copy-")
                    })
                    .unwrap();
                let bytes = fs::read(stage.join("intent")).unwrap();
                assert!(bytes.len() <= MAX_LOG_BYTES);
                let log = decode(&bytes).unwrap();
                assert!(!log.torn_tail);
                assert_eq!(log.records.len(), 3);
                let record = log.records.last().unwrap();
                assert_eq!(record.phase, JournalPhase::CommitIntent);
                assert_eq!(record.destination_leaf, "destination");
                let metadata = fs::symlink_metadata(stage.join("payload")).unwrap();
                assert_eq!(
                    record.payload_identity,
                    Some(NativeIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino()
                    })
                );
            }
        },
    ));
    assert_eq!(fs::read(f.path("source")).unwrap(), b"correct payload");
    f.no_stage();
}

#[test]
fn process_exit_preserves_a_checksummed_uncertain_commit_record_or_a_complete_destination() {
    for mode in ["before", "after"] {
        let f = Fixture::new();
        f.file("source", b"survive process exit");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_exit_owned_fixture_helper",
                "--nocapture",
            ])
            .env("BROWSER_OPERATIONS_OWNED_FIXTURE", &f.0)
            .env("BROWSER_OPERATIONS_EXIT_PHASE", mode)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(f.path("source")).unwrap(), b"survive process exit");
        if mode == "before" {
            assert!(!f.path("destination").exists());
            let stage = fs::read_dir(&f.0)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .as_encoded_bytes()
                        .starts_with(b".browser-copy-")
                })
                .unwrap();
            let log = browser_operations::journal::decode(&fs::read(stage.join("intent")).unwrap())
                .unwrap();
            assert_eq!(
                log.records.last().unwrap().phase,
                browser_operations::journal::JournalPhase::CommitIntent
            );
            assert_eq!(
                fs::read(stage.join("payload")).unwrap(),
                b"survive process exit"
            );
        } else {
            assert_eq!(
                fs::read(f.path("destination")).unwrap(),
                b"survive process exit"
            );
            f.no_stage();
        }
    }
}

#[test]
fn process_exit_owned_fixture_helper() {
    let Some(root) = std::env::var_os("BROWSER_OPERATIONS_OWNED_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated"))
    );
    let before = std::env::var("BROWSER_OPERATIONS_EXIT_PHASE").unwrap() == "before";
    let plan = plan_copy_new(
        root.join("source"),
        root.join("destination"),
        CopyLimits::default(),
        &Cancellation::default(),
    )
    .unwrap();
    let outcome = execute_copy_new(plan, &Cancellation::default(), |p| {
        if (before && p.phase == Phase::BeforeCommit) || (!before && p.phase == Phase::Committed) {
            std::process::exit(73); // Deliberately bypass RAII; parent owns and removes this fixture.
        }
    });
    panic!("exit boundary was not reached: {outcome:?}");
}

#[test]
fn native_write_failure_keeps_source_and_removes_partial_staging() {
    let f = Fixture::new();
    let payload = vec![17; 131_111];
    f.file("source", &payload);
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "trap '' XFSZ; ulimit -f 1; exec \"$1\" --exact write_failure_owned_fixture_helper --nocapture", "copy-write-limit"])
        .arg(std::env::current_exe().unwrap()).env("BROWSER_OPERATIONS_OWNED_FIXTURE", &f.0)
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(f.path("source")).unwrap(), payload);
    assert!(!f.path("destination").exists());
    f.no_stage();
}

#[test]
fn write_failure_owned_fixture_helper() {
    let Some(root) = std::env::var_os("BROWSER_OPERATIONS_OWNED_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated"))
    );
    let plan = plan_copy_new(
        root.join("source"),
        root.join("destination"),
        CopyLimits::default(),
        &Cancellation::default(),
    )
    .unwrap();
    let error = failed_clean(execute_copy_new(plan, &Cancellation::default(), |_| {}));
    assert_eq!(error.phase, "write copy");
    assert_eq!(error.raw_os_error, Some(27));
}

#[test]
fn wide_copy_completes_with_low_descriptor_limit() {
    let f = Fixture::new();
    for i in 0..128 {
        f.file(format!("source/folder/item-{i}"), b"bounded handles");
    }
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -n 32; exec \"$1\" --exact low_descriptor_owned_fixture_helper --nocapture",
            "copy-fd-limit",
        ])
        .arg(std::env::current_exe().unwrap())
        .env("BROWSER_OPERATIONS_OWNED_FIXTURE", &f.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_dir(f.path("destination/folder")).unwrap().count(),
        128
    );
    f.no_stage();
}

#[test]
fn low_descriptor_owned_fixture_helper() {
    let Some(root) = std::env::var_os("BROWSER_OPERATIONS_OWNED_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated"))
    );
    let plan = plan_copy_new(
        root.join("source"),
        root.join("destination"),
        CopyLimits::default(),
        &Cancellation::default(),
    )
    .unwrap();
    let receipt = committed(execute_copy_new(plan, &Cancellation::default(), |_| {}));
    assert_eq!(receipt.entries, 130);
    assert_eq!(receipt.bytes, 128 * 15);
}

#[test]
fn post_commit_record_failure_is_committed_uncertain_and_retains_recovery_breadcrumb() {
    let f = Fixture::new();
    f.file("source", b"");
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "trap '' XFSZ; ulimit -f 2; exec \"$1\" --exact post_commit_failure_owned_fixture_helper --nocapture", "copy-journal-limit"])
        .arg(std::env::current_exe().unwrap()).env("BROWSER_OPERATIONS_OWNED_FIXTURE", &f.0)
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(f.path("x".repeat(255)).is_file());
    assert!(f.path("source").is_file());
}

#[test]
fn post_commit_failure_owned_fixture_helper() {
    let Some(root) = std::env::var_os("BROWSER_OPERATIONS_OWNED_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures-generated"))
    );
    let plan = plan_copy_new(
        root.join("source"),
        root.join("x".repeat(255)),
        CopyLimits::default(),
        &Cancellation::default(),
    )
    .unwrap();
    match execute_copy_new(plan, &Cancellation::default(), |_| {}) {
        CopyOutcome::Committed(receipt) => {
            assert_eq!(receipt.durability, Durability::Uncertain);
            assert!(receipt.warnings.iter().any(|e| e.raw_os_error == Some(27)));
            let Cleanup::RecoveryRequired(residue) = receipt.cleanup else {
                panic!("commit breadcrumb was removed");
            };
            let log = browser_operations::journal::decode(
                &fs::read(residue.path.join("intent")).unwrap(),
            )
            .unwrap();
            assert!(log.torn_tail);
            assert_eq!(
                log.records.last().unwrap().phase,
                browser_operations::journal::JournalPhase::CommitIntent
            );
            assert!(receipt.destination.is_file());
        }
        other => panic!("publication was falsely reported as failure: {other:?}"),
    }
}
