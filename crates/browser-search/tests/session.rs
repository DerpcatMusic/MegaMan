use browser_search::{
    Coverage, IndexOptions, MAX_SESSION_PAGE_SIZE, Query, SearchSession, SessionDelivery,
    SessionPage, SessionSubmitError,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
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
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str) {
        fs::write(self.0.join(name), b"fixture").unwrap();
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
fn query(id: u64, text: &str) -> Query {
    Query {
        request_id: id,
        text: text.into(),
        limit: 100,
        ..Query::default()
    }
}
fn terminal(session: &SearchSession, id: u64) -> Box<SessionPage> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(delivery) = session.poll() {
            assert_eq!(delivery.request_id(), id, "stale session delivery");
            match delivery {
                SessionDelivery::Page(page) if page.page.coverage != Coverage::Indexing => {
                    return page;
                }
                SessionDelivery::Page(_) => (),
                SessionDelivery::Cancelled { .. } => panic!("unexpected cancellation"),
            }
        }
        assert!(Instant::now() < deadline, "session timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn latest_scope_and_query_win_and_rapid_page_requests_stay_bounded() {
    let old = Fixture::new();
    old.file("obsolete");
    let current = Fixture::new();
    for i in 0..6_123 {
        current.file(&format!("item-{i:05}"));
    }
    current.file("zz-late-needle");
    let session = SearchSession::new().unwrap();
    session
        .start_scope(old.options(), query(1, "obsolete"))
        .unwrap();
    session
        .start_scope(current.options(), query(2, ""))
        .unwrap();
    for id in 3..100 {
        session.submit(query(id, "obsolete")).unwrap();
    }
    session.submit(query(100, "LATE-NEEDLE")).unwrap();
    let late = terminal(&session, 100);
    assert_eq!(late.page.coverage, Coverage::Complete);
    assert_eq!(late.page.total_matches, 1);
    assert_eq!(late.page.entries[0].display_name, "zz-late-needle");
    assert_eq!(
        late.requested_roots.as_ref(),
        std::slice::from_ref(&current.0)
    );
    assert_eq!(
        late.effective_roots.as_ref(),
        std::slice::from_ref(&current.0)
    );
    session
        .submit(Query {
            offset: 6_120,
            limit: MAX_SESSION_PAGE_SIZE,
            ..query(101, "")
        })
        .unwrap();
    let page = terminal(&session, 101);
    assert_eq!(page.page.total_matches, 6_124);
    assert_eq!(page.page.entries.len(), 4);
    assert_eq!(page.page.entries[3].display_name, "zz-late-needle");
    assert_eq!(page.page.generation, late.page.generation);
    // Terminal indexing does not continuously requery/redeliver an unchanged publication.
    thread::sleep(Duration::from_millis(220));
    assert!(session.poll().is_none());
}

#[test]
fn invalid_submissions_leave_current_scope_and_request_intact() {
    let fixture = Fixture::new();
    fixture.file("keep");
    let session = SearchSession::new().unwrap();
    assert_eq!(
        session.submit(query(1, "")),
        Err(SessionSubmitError::NoScope)
    );
    assert!(matches!(
        session.start_scope(IndexOptions::new(vec!["relative".into()]), query(1, "")),
        Err(SessionSubmitError::RelativeRoot(_))
    ));
    let mut options = fixture.options();
    options.excluded_roots = vec!["relative".into()];
    assert!(matches!(
        session.start_scope(options, query(1, "")),
        Err(SessionSubmitError::RelativeExclusion(_))
    ));
    session
        .start_scope(fixture.options(), query(1, ""))
        .unwrap();
    assert_eq!(
        session.submit(Query {
            limit: MAX_SESSION_PAGE_SIZE + 1,
            ..query(2, "")
        }),
        Err(SessionSubmitError::PageTooLarge {
            requested: 513,
            maximum: 512
        })
    );
    assert_eq!(
        session.submit(query(1, "")),
        Err(SessionSubmitError::NonMonotonicRequest {
            latest: 1,
            received: 1
        })
    );
    assert_eq!(terminal(&session, 1).page.total_matches, 1);
}

#[test]
fn cancel_from_initial_and_terminal_states_wakes_and_restart_requires_new_id() {
    let fixture = Fixture::new();
    fixture.file("current");
    let session = SearchSession::new().unwrap();
    assert!(!session.cancel());
    session
        .start_scope(fixture.options(), query(1, ""))
        .unwrap();
    terminal(&session, 1);
    assert!(session.cancel());
    assert!(matches!(
        session.poll(),
        Some(SessionDelivery::Cancelled { request_id: 1 })
    ));
    assert!(!session.cancel());
    assert_eq!(
        session.submit(query(2, "")),
        Err(SessionSubmitError::NoScope)
    );
    assert!(matches!(
        session.start_scope(fixture.options(), query(1, "")),
        Err(SessionSubmitError::NonMonotonicRequest { .. })
    ));
    session
        .start_scope(fixture.options(), query(2, "current"))
        .unwrap();
    assert_eq!(terminal(&session, 2).page.total_matches, 1);
    // Cancelling a just-submitted query cannot be overwritten by its stale worker page.
    session.submit(query(3, "")).unwrap();
    assert!(session.cancel());
    thread::sleep(Duration::from_millis(20));
    assert!(matches!(
        session.poll(),
        Some(SessionDelivery::Cancelled { request_id: 3 })
    ));
}

#[test]
fn no_results_still_deliver_partial_coverage_and_diagnostics() {
    let fixture = Fixture::new();
    let session = SearchSession::new().unwrap();
    session
        .start_scope(
            IndexOptions::new(vec![fixture.0.join("missing")]),
            query(1, "no-match"),
        )
        .unwrap();
    let page = terminal(&session, 1);
    assert_eq!(page.page.coverage, Coverage::Partial);
    assert_eq!(page.page.total_matches, 0);
    assert_eq!(page.error_count, 1);
    assert_eq!(page.diagnostics.len(), 1);
}

#[cfg(unix)]
#[test]
fn session_rows_preserve_undecodable_native_identity() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    let path = fixture
        .0
        .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
    fs::write(&path, b"native").unwrap();
    let session = SearchSession::new().unwrap();
    session
        .start_scope(fixture.options(), query(1, "NATIVE-"))
        .unwrap();
    let page = terminal(&session, 1);
    assert_eq!(page.page.total_matches, 1);
    assert_eq!(page.page.entries[0].path, path);
    assert_eq!(fs::read(&page.page.entries[0].path).unwrap(), b"native");
}

#[test]
fn close_releases_query_worker_and_owned_index_service() {
    // Repeated lifecycle exercise uses only a tiny owned scope, with no catalog in the caller.
    let fixture = Fixture::new();
    fixture.file("close");
    for _ in 0..10 {
        let session = SearchSession::new().unwrap();
        session
            .start_scope(fixture.options(), query(1, ""))
            .unwrap();
        terminal(&session, 1);
        drop(session);
    }
}
