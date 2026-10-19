use crate::{
    Coverage, Diagnostic, Generation, IndexOptions, IndexSnapshot, Query, QueryPage, SearchIndex,
};
use std::{
    error::Error,
    fmt, io,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// Row bound at the UI mailbox boundary. Exact matching counts are never capped.
pub const MAX_SESSION_PAGE_SIZE: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionSubmitError {
    NoScope,
    NonMonotonicRequest { latest: u64, received: u64 },
    PageTooLarge { requested: usize, maximum: usize },
    RelativeRoot(PathBuf),
    RelativeExclusion(PathBuf),
}
impl fmt::Display for SessionSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for SessionSubmitError {}

/// Bounded result rows and scope/error metadata, never an owning catalog snapshot.
#[derive(Clone, Debug)]
pub struct SessionPage {
    pub page: QueryPage,
    pub indexed_entries: usize,
    pub directories_scanned: usize,
    pub symlinks_not_followed: usize,
    pub excluded_subtrees: usize,
    pub error_count: usize,
    /// At most the first 64 diagnostic details; error_count remains uncapped.
    pub diagnostics: Arc<[Diagnostic]>,
    pub requested_roots: Arc<[PathBuf]>,
    pub effective_roots: Arc<[PathBuf]>,
    pub excluded_roots: Arc<[PathBuf]>,
}

#[derive(Clone, Debug)]
pub enum SessionDelivery {
    Page(Box<SessionPage>),
    Cancelled { request_id: u64 },
}
impl SessionDelivery {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Page(page) => page.page.request_id,
            Self::Cancelled { request_id } => *request_id,
        }
    }
}

struct PendingQuery {
    query: Query,
    cancel: Arc<AtomicBool>,
}
struct State {
    latest_request: Option<u64>,
    has_scope: bool,
    pending_scope: Option<IndexOptions>,
    pending_query: Option<PendingQuery>,
    cancel_scope: bool,
    query_cancel: Arc<AtomicBool>,
    delivery: Option<SessionDelivery>,
    shutdown: bool,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// UI-facing service: one query worker owns the index service and all catalog snapshots.
pub struct SearchSession {
    shared: Arc<Shared>,
}
impl SearchSession {
    pub fn new() -> io::Result<Self> {
        let index = SearchIndex::new()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                latest_request: None,
                has_scope: false,
                pending_scope: None,
                pending_query: None,
                cancel_scope: false,
                query_cancel: Arc::new(AtomicBool::new(false)),
                delivery: None,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let worker = shared.clone();
        thread::Builder::new()
            .name("filename-query".into())
            .spawn(move || session_worker(worker, index))?;
        Ok(Self { shared })
    }

    /// Replaces scope and query together. Every root/exclusion must already be absolute.
    /// Empty roots are accepted and reported as Partial by the index, never inferred.
    pub fn start_scope(
        &self,
        options: IndexOptions,
        query: Query,
    ) -> Result<(), SessionSubmitError> {
        for root in &options.roots {
            if !root.is_absolute() {
                return Err(SessionSubmitError::RelativeRoot(root.clone()));
            }
        }
        for root in &options.excluded_roots {
            if !root.is_absolute() {
                return Err(SessionSubmitError::RelativeExclusion(root.clone()));
            }
        }
        let mut state = self.shared.state.lock().expect("session state poisoned");
        validate_query(&state, &query)?;
        state.pending_scope = Some(options);
        state.has_scope = true;
        state.cancel_scope = false;
        enqueue_query(&mut state, query);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Latest-only query/page request in the current scope. IDs must increase strictly.
    pub fn submit(&self, query: Query) -> Result<(), SessionSubmitError> {
        let mut state = self.shared.state.lock().expect("session state poisoned");
        if !state.has_scope {
            return Err(SessionSubmitError::NoScope);
        }
        validate_query(&state, &query)?;
        enqueue_query(&mut state, query);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Takes the one-slot delivery mailbox. Always compare request_id with current UI state.
    pub fn poll(&self) -> Option<SessionDelivery> {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .delivery
            .take()
    }

    /// Immediately cancels query work and emits a cancellation event. Restart with start_scope.
    /// The index worker's OS filesystem calls remain cooperatively cancellable.
    pub fn cancel(&self) -> bool {
        let mut state = self.shared.state.lock().expect("session state poisoned");
        if !state.has_scope {
            return false;
        }
        state.has_scope = false;
        state.query_cancel.store(true, Ordering::Relaxed);
        state.pending_query = None;
        state.pending_scope = None;
        state.cancel_scope = true;
        state.delivery = state
            .latest_request
            .map(|request_id| SessionDelivery::Cancelled { request_id });
        self.shared.wake.notify_one();
        true
    }
}
impl Drop for SearchSession {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("session state poisoned");
        state.shutdown = true;
        state.query_cancel.store(true, Ordering::Relaxed);
        state.pending_scope = None;
        state.pending_query = None;
        self.shared.wake.notify_one();
        // No join or snapshot destruction on the caller.
    }
}

fn validate_query(state: &State, query: &Query) -> Result<(), SessionSubmitError> {
    if let Some(latest) = state.latest_request
        && query.request_id <= latest
    {
        return Err(SessionSubmitError::NonMonotonicRequest {
            latest,
            received: query.request_id,
        });
    }
    if query.limit > MAX_SESSION_PAGE_SIZE {
        return Err(SessionSubmitError::PageTooLarge {
            requested: query.limit,
            maximum: MAX_SESSION_PAGE_SIZE,
        });
    }
    Ok(())
}
fn enqueue_query(state: &mut State, query: Query) {
    state.query_cancel.store(true, Ordering::Relaxed);
    let cancel = Arc::new(AtomicBool::new(false));
    state.query_cancel = cancel.clone();
    state.latest_request = Some(query.request_id);
    state.pending_query = Some(PendingQuery { query, cancel });
    state.delivery = None;
}

fn session_worker(shared: Arc<Shared>, index: SearchIndex) {
    let mut generation: Option<Generation> = None;
    let mut active: Option<PendingQuery> = None;
    let mut last_snapshot: Option<Arc<IndexSnapshot>> = None;
    loop {
        let (scope, query, cancel_scope, shutdown) = {
            let mut state = shared.state.lock().expect("session state poisoned");
            if state.pending_scope.is_none()
                && state.pending_query.is_none()
                && !state.cancel_scope
                && !state.shutdown
            {
                if active.is_some()
                    && last_snapshot
                        .as_ref()
                        .is_some_and(|s| s.coverage == Coverage::Indexing)
                {
                    state = shared
                        .wake
                        .wait_timeout(state, Duration::from_millis(100))
                        .expect("session state poisoned")
                        .0;
                } else {
                    // Idle/terminal states do not poll or spin; input/cancel/close wakes this wait.
                    while state.pending_scope.is_none()
                        && state.pending_query.is_none()
                        && !state.cancel_scope
                        && !state.shutdown
                    {
                        state = shared.wake.wait(state).expect("session state poisoned");
                    }
                }
            }
            let cancel_scope = std::mem::take(&mut state.cancel_scope);
            (
                state.pending_scope.take(),
                state.pending_query.take(),
                cancel_scope,
                state.shutdown,
            )
        };
        if shutdown {
            break;
        }
        if cancel_scope {
            if let Some(generation) = generation {
                index.cancel(generation);
            }
            active = None;
            last_snapshot = None;
        }
        if let Some(scope) = scope {
            generation = Some(index.start(scope));
            last_snapshot = None;
        }
        let force_query = query.is_some();
        if let Some(query) = query {
            active = Some(query);
        }
        let Some(request) = active.as_ref() else {
            continue;
        };
        if request.cancel.load(Ordering::Relaxed) {
            active = None;
            last_snapshot = None;
            continue;
        }
        let snapshot = index.snapshot();
        if (force_query
            || last_snapshot
                .as_ref()
                .is_none_or(|old| !Arc::ptr_eq(old, &snapshot)))
            && let Ok(page) = snapshot.query_cancellable(&request.query, &request.cancel)
        {
            let delivery = SessionDelivery::Page(Box::new(SessionPage {
                page,
                indexed_entries: snapshot.indexed_entries,
                directories_scanned: snapshot.directories_scanned,
                symlinks_not_followed: snapshot.symlinks_not_followed,
                excluded_subtrees: snapshot.excluded_subtrees,
                error_count: snapshot.error_count,
                diagnostics: snapshot.diagnostics.clone(),
                requested_roots: snapshot.requested_roots.clone(),
                effective_roots: snapshot.effective_roots.clone(),
                excluded_roots: snapshot.excluded_roots.clone(),
            }));
            let mut state = shared.state.lock().expect("session state poisoned");
            if !state.shutdown
                && state.has_scope
                && state.latest_request == Some(request.query.request_id)
                && !request.cancel.load(Ordering::Relaxed)
            {
                state.delivery = Some(delivery);
            }
        }
        last_snapshot = Some(snapshot);
    }
    // Retained snapshots and the index service are destroyed on this query worker.
    drop(last_snapshot);
    drop(index);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn closing_idle_session_wakes_worker_and_releases_ownership() {
        let session = SearchSession::new().unwrap();
        let weak = Arc::downgrade(&session.shared);
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(5);
        while weak.upgrade().is_some() {
            assert!(Instant::now() < deadline, "idle query worker did not exit");
            thread::yield_now();
        }
    }
}
