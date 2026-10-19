use browser_search::{
    Coverage, EntryKind, Generation, IndexOptions, Query, SearchIndex, SortOrder,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        // Owned fixtures stay on the dev drive; never use the user's directories or /tmp.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures-generated")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, b"fixture").unwrap();
        path
    }
    fn options(&self) -> IndexOptions {
        IndexOptions::new(vec![self.0.clone()])
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn complete(
    index: &SearchIndex,
    generation: Generation,
) -> std::sync::Arc<browser_search::IndexSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = index.snapshot();
        assert_eq!(snapshot.generation, generation);
        if snapshot.coverage != Coverage::Indexing {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "index timed out: {snapshot:?}");
        thread::sleep(Duration::from_millis(1));
    }
}
fn query(text: &str) -> Query {
    Query {
        request_id: 123,
        text: text.into(),
        limit: 100,
        ..Query::default()
    }
}

#[test]
fn late_matches_and_pagination_have_no_catalog_or_result_cap() {
    let fixture = Fixture::new();
    for i in 0..6_123 {
        fixture.file(&format!("item-{i:05}"));
    }
    fixture.file("zz-late-needle");
    let index = SearchIndex::new().unwrap();
    let generation = index.start(fixture.options());
    let snapshot = complete(&index, generation);
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.indexed_entries, 6_124);
    let late = snapshot.query(&query("LATE-NEEDLE"));
    assert_eq!(late.total_matches, 1);
    assert_eq!(late.entries[0].display_name, "zz-late-needle");
    let page = snapshot.query(&Query {
        offset: 6_120,
        limit: 10,
        ..query("")
    });
    assert_eq!(page.total_matches, 6_124);
    assert_eq!(page.entries.len(), 4);
    assert_eq!(page.entries[3].display_name, "zz-late-needle");
    assert_eq!(page.generation, generation);
    assert_eq!(page.request_id, 123);
    let beyond = snapshot.query(&Query {
        offset: usize::MAX,
        limit: usize::MAX,
        ..query("")
    });
    assert!(beyond.entries.is_empty());
    assert_eq!(beyond.total_matches, 6_124);
    let count = snapshot.query(&Query {
        limit: 0,
        ..query("")
    });
    assert!(count.entries.is_empty());
    assert_eq!(count.total_matches, 6_124);
}

#[test]
fn unicode_full_fold_sort_and_path_ties_preserve_identity() {
    let fixture = Fixture::new();
    for name in ["Straße", "STRASSE", "α/Σ", "β/ς", "β/Σ", "z-last"] {
        fixture.file(name);
    }
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(fixture.options()));
    let page = snapshot.query(&query("strasse"));
    assert_eq!(page.total_matches, 2);
    assert_ne!(page.entries[0].path, page.entries[1].path);
    let sigma = snapshot.query(&query("σ"));
    assert_eq!(sigma.total_matches, 3);
    assert!(
        sigma
            .entries
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
    let asc = snapshot.query(&query(""));
    let desc = snapshot.query(&Query {
        sort: SortOrder::NameDescending,
        ..query("")
    });
    assert_eq!(
        asc.entries
            .iter()
            .rev()
            .map(|e| &e.path)
            .collect::<Vec<_>>(),
        desc.entries.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
}

#[test]
fn invalid_empty_and_file_roots_report_partial_even_with_no_results() {
    let fixture = Fixture::new();
    let file = fixture.file("file");
    let index = SearchIndex::new().unwrap();
    for roots in [vec![], vec![fixture.0.join("missing")], vec![file]] {
        let snapshot = complete(&index, index.start(IndexOptions::new(roots)));
        assert_eq!(snapshot.coverage, Coverage::Partial);
        assert_eq!(snapshot.error_count, 1);
        assert_eq!(snapshot.diagnostics.len(), 1);
        assert_eq!(snapshot.query(&query("")).total_matches, 0);
    }
}

#[test]
fn overlapping_roots_are_deduplicated_and_excludes_are_explicit() {
    let fixture = Fixture::new();
    fixture.file("child/visible");
    fixture.file("omit/secret");
    let mut options = IndexOptions::new(vec![
        fixture.0.join("child"),
        fixture.0.clone(),
        fixture.0.clone(),
    ]);
    options.excluded_roots.push(fixture.0.join("omit"));
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(options));
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.effective_roots.len(), 1);
    assert_eq!(snapshot.excluded_subtrees, 1);
    assert_eq!(snapshot.query(&query("visible")).total_matches, 1);
    assert_eq!(snapshot.query(&query("secret")).total_matches, 0);
    // The excluded directory itself is a visible entry; its contents are outside scope.
    assert_eq!(snapshot.indexed_entries, 3);
}

#[test]
fn generation_replacement_and_cancel_never_publish_stale_results() {
    let old = Fixture::new();
    let new = Fixture::new();
    old.file("obsolete");
    new.file("current");
    let index = SearchIndex::new().unwrap();
    let first = index.start(old.options());
    // start() may finish before the caller next runs. Retain a known completed
    // generation here; the unit handshake test covers true in-flight replacement.
    let retained = complete(&index, first);
    assert_eq!(retained.coverage, Coverage::Complete);
    assert_eq!(retained.query(&query("obsolete")).total_matches, 1);
    let second = index.start(new.options());
    assert!(second.0 > first.0);
    assert!(!index.cancel(first));
    let snapshot = complete(&index, second);
    assert_eq!(snapshot.query(&query("obsolete")).total_matches, 0);
    assert_eq!(snapshot.query(&query("current")).total_matches, 1);
    assert_eq!(retained.generation, first);
    assert_eq!(retained.coverage, Coverage::Complete);
    assert_eq!(retained.query(&query("obsolete")).total_matches, 1);
    assert_eq!(retained.query(&query("current")).total_matches, 0);
    // Cancellation freezes the published snapshot immediately and remains terminal.
    let mut options = old.options();
    options.roots = (0..100).map(|_| old.0.clone()).collect();
    let third = index.start(options);
    let cancelled = index.cancel(third);
    let terminal = if cancelled {
        Coverage::Cancelled
    } else {
        Coverage::Complete
    };
    assert_eq!(index.snapshot().coverage, terminal);
    let retained_third = index.snapshot();
    assert!(!index.cancel(third));
    let fourth = index.start(new.options());
    let snapshot = complete(&index, fourth);
    // One persistent worker: completing fourth synchronizes all earlier scan work.
    assert_eq!(snapshot.generation, fourth);
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.query(&query("obsolete")).total_matches, 0);
    assert_eq!(snapshot.query(&query("current")).total_matches, 1);
    assert_eq!(retained_third.generation, third);
    assert_eq!(retained_third.coverage, terminal);
}

#[test]
fn query_can_read_an_immutable_partial_snapshot_while_worker_continues() {
    let fixture = Fixture::new();
    for i in 0..2_000 {
        fixture.file(&format!("folder/{i}"));
    }
    let index = SearchIndex::new().unwrap();
    let mut options = fixture.options();
    options.publish_interval = Duration::ZERO;
    let generation = index.start(options);
    let deadline = Instant::now() + Duration::from_secs(30);
    let partial = loop {
        let snapshot = index.snapshot();
        if snapshot.indexed_entries > 0 {
            break snapshot;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    };
    // A scheduling delay may allow completion before this thread runs; either immutable state is valid.
    let count = partial.indexed_entries;
    let page = partial.query(&query(""));
    assert_eq!(page.total_matches, count);
    let final_snapshot = complete(&index, generation);
    assert_eq!(final_snapshot.indexed_entries, 2_001);
    assert_eq!(partial.indexed_entries, count);
    assert_eq!(partial.query(&query("")).total_matches, count);
    let cancel = AtomicBool::new(true);
    assert!(
        final_snapshot
            .query_cancellable(&query(""), &cancel)
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn native_filename_bytes_symlink_cycles_and_hardlinks_round_trip() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let fixture = Fixture::new();
    let invalid = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
    fs::write(&invalid, b"native").unwrap();
    let second_invalid = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"native-\xfe".to_vec()));
    fs::write(&second_invalid, b"other").unwrap();
    let file = fixture.file("child/file");
    let hardlink = fixture.0.join("hardlink");
    fs::hard_link(&file, &hardlink).unwrap();
    symlink(&fixture.0, fixture.0.join("child/cycle")).unwrap();
    symlink(fixture.0.join("missing"), fixture.0.join("broken")).unwrap();
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(fixture.options()));
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.directories_scanned, 2);
    assert_eq!(snapshot.symlinks_not_followed, 2);
    let results = snapshot.query(&query("native-"));
    assert_eq!(results.total_matches, 2);
    assert!(results.entries.iter().any(|entry| entry.path == invalid));
    assert!(
        results
            .entries
            .iter()
            .any(|entry| entry.path == second_invalid)
    );
    for entry in results.entries {
        assert!(fs::read(&entry.path).is_ok());
    }
    assert_eq!(
        snapshot.query(&query("cycle")).entries[0].kind,
        EntryKind::Symlink
    );
    assert_eq!(snapshot.query(&query("hardlink")).entries[0].path, hardlink);
    // Explicit symlink roots are rejected, rather than descending into an aliased directory.
    let root_alias = fixture.0.join("alias");
    symlink(&fixture.0, &root_alias).unwrap();
    let mut trailing = root_alias.as_os_str().to_os_string();
    trailing.push("/");
    for root in [root_alias.clone(), trailing.into(), root_alias.join(".")] {
        let alias_snapshot = complete(&index, index.start(IndexOptions::new(vec![root.clone()])));
        assert_eq!(alias_snapshot.coverage, Coverage::Partial, "root {root:?}");
        assert_eq!(alias_snapshot.indexed_entries, 0);
        assert_eq!(alias_snapshot.diagnostics[0].operation, "root");
    }
}

#[cfg(unix)]
#[test]
fn denied_directory_is_reported_and_never_claimed_complete() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.file("denied/hidden");
    let denied = fixture.0.join("denied");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(fixture.options()));
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(snapshot.coverage, Coverage::Partial);
    assert_eq!(snapshot.error_count, 1);
    assert_eq!(
        snapshot.diagnostics[0].kind,
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(snapshot.query(&query("hidden")).total_matches, 0);
}

#[test]
fn diagnostic_detail_bound_does_not_hide_total_errors() {
    let fixture = Fixture::new();
    let roots = (0..80)
        .map(|i| fixture.0.join(format!("missing-{i}")))
        .collect();
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(IndexOptions::new(roots)));
    assert_eq!(snapshot.coverage, Coverage::Partial);
    assert_eq!(snapshot.error_count, 80);
    assert_eq!(snapshot.diagnostics.len(), 64);
}

#[cfg(unix)]
#[test]
fn deep_walk_runs_with_a_low_descriptor_limit() {
    let fixture = Fixture::new();
    let mut path = fixture.0.clone();
    for _ in 0..120 {
        path.push("d");
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep-needle"), b"deep").unwrap();
    let output = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -n 32 && exec \"$1\" --exact bounded_descriptor_helper --nocapture",
            "search-low-fd",
        ])
        .arg(std::env::current_exe().unwrap())
        .env("BROWSER_SEARCH_TEST_OWNED_ROOT", &fixture.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn bounded_descriptor_helper() {
    let Some(root) = std::env::var_os("BROWSER_SEARCH_TEST_OWNED_ROOT") else {
        return;
    };
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(IndexOptions::new(vec![root.into()])));
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.directories_scanned, 121);
    assert_eq!(snapshot.query(&query("deep-needle")).total_matches, 1);
}

#[cfg(unix)]
#[test]
fn root_parent_components_follow_native_symlink_semantics() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.file("a/content/wrong");
    fixture.file("b/content/right");
    fs::create_dir_all(fixture.0.join("b/sub")).unwrap();
    symlink(fixture.0.join("b/sub"), fixture.0.join("a/link")).unwrap();
    let root = fixture.0.join("a/link/../content/.");
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(IndexOptions::new(vec![root])));
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.query(&query("right")).total_matches, 1);
    assert_eq!(snapshot.query(&query("wrong")).total_matches, 0);
}

#[cfg(unix)]
#[test]
fn exclusions_respect_real_symlink_parent_semantics_and_reject_ambiguous_missing_paths() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.file("a/excluded/keep");
    fixture.file("b/excluded/hide");
    fs::create_dir_all(fixture.0.join("b/sub")).unwrap();
    symlink(fixture.0.join("b/sub"), fixture.0.join("a/link")).unwrap();
    let mut options = fixture.options();
    options
        .excluded_roots
        .push(fixture.0.join("a/link/../excluded"));
    let index = SearchIndex::new().unwrap();
    let snapshot = complete(&index, index.start(options));
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert_eq!(snapshot.query(&query("hide")).total_matches, 0);
    assert_eq!(snapshot.query(&query("keep")).total_matches, 1);
    let mut options = fixture.options();
    options
        .excluded_roots
        .push(fixture.0.join("a/link/../not-created"));
    let snapshot = complete(&index, index.start(options));
    assert_eq!(snapshot.coverage, Coverage::Partial);
    assert_eq!(snapshot.error_count, 1);
    assert_eq!(snapshot.diagnostics[0].operation, "exclude");
    assert_eq!(
        snapshot.diagnostics[0].kind,
        std::io::ErrorKind::InvalidInput
    );
    // A missing plain suffix under an existing symlink prefix resolves via the real target.
    let mut options = fixture.options();
    options
        .excluded_roots
        .push(fixture.0.join("a/link/not-created"));
    let snapshot = complete(&index, index.start(options));
    assert_eq!(snapshot.coverage, Coverage::Complete);
    assert!(
        snapshot
            .excluded_roots
            .contains(&fixture.0.join("b/sub/not-created"))
    );
    symlink(fixture.0.join("not-created"), fixture.0.join("dangling")).unwrap();
    let mut options = fixture.options();
    options
        .excluded_roots
        .push(fixture.0.join("dangling/child"));
    let snapshot = complete(&index, index.start(options));
    assert_eq!(snapshot.coverage, Coverage::Partial);
    assert_eq!(
        snapshot.diagnostics[0].kind,
        std::io::ErrorKind::InvalidInput
    );
}
