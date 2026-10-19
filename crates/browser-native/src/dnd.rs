//! Bounded drop-target lifecycle, not a native transport or window registration.
//!
//! Keep this model on the transfer worker. URI decoding is bounded but linear;
//! do not decode in rendering. The host must validate native peer/window identity,
//! frame transfer boundaries and enforce deadlines before calling this API.
//! Only `complete_copy` can produce a successful finish, by borrowing the bound
//! CopySession's actual terminal. Accepted data and job admission are not success.
//! See docs/NATIVE-DND.md for exclusive native-host integration requirements.

use browser_operations::{
    Cleanup, CommitReceipt, CopyOptions, Durability, MetadataCompletion, MetadataPolicy,
    session::{
        BatchStop, ControlError, CopyBatch, CopySession, JobId, RootOutcome, SubmitErrorKind,
    },
};
use browser_transfer::{Limits, TransferError, decode_uri_list};
use std::{
    io,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn qualified_basic_receipt(receipt: &CommitReceipt) -> bool {
    receipt.metadata == MetadataCompletion::Achieved(MetadataPolicy::Basic)
        && receipt.durability == Durability::Synced
        && matches!(receipt.cleanup, Cleanup::Clean)
}

/// Process-local capability. The full identity includes a nonreused owner.
/// ```compile_fail
/// use browser_native::dnd::DropId;
/// let forged: DropId = 1_u64.into();
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DropId {
    owner: u64,
    sequence: u64,
}
impl DropId {
    /// Presentation only; retain the full DropId for every asynchronous callback.
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Intent {
    Copy,
    Move,
    Link,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    pub accepted: bool,
    pub action: Option<Intent>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Hovering,
    Receiving,
    Ready,
    Running,
    Finished,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    Rejected,
    Committed,
    Failed,
    Cancelled,
    Partial,
    CommittedUncertain,
    RecoveryRequired,
}
/// Scalar protocol result. Actual receipts/residue remain in the operation center.
/// `success` is deliberately derived, never set by the native drop callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Finish {
    id: DropId,
    disposition: Disposition,
    committed_roots: usize,
    total_roots: usize,
    notify_peer: bool,
}
impl Finish {
    pub fn id(self) -> DropId {
        self.id
    }
    pub fn disposition(self) -> Disposition {
        self.disposition
    }
    pub fn committed_roots(self) -> usize {
        self.committed_roots
    }
    pub fn total_roots(self) -> usize {
        self.total_roots
    }
    pub fn notify_peer(self) -> bool {
        self.notify_peer
    }
    pub fn success(self) -> bool {
        self.disposition == Disposition::Committed
    }
    pub fn performed_action(self) -> Option<Intent> {
        self.success().then_some(Intent::Copy)
    }
}
#[derive(Debug)]
pub enum Error {
    Busy,
    Stale,
    Exhausted,
    WrongPhase,
    NotAccepted,
    InvalidDestination,
    Payload(TransferError),
    Submit(SubmitErrorKind),
    JobControl(ControlError),
    NotTerminal,
    WrongJob,
}

enum State {
    Hovering {
        accepted: bool,
    },
    Receiving {
        destination: PathBuf,
    },
    Ready {
        paths: Vec<PathBuf>,
        destination: PathBuf,
    },
    Running(JobId),
    Finished(Finish),
}
struct Active {
    id: DropId,
    uri_list_offered: bool,
    peer_alive: bool,
    state: State,
}
/// One active offer/payload/job/finish; no event queue and no worker spawning.
///
/// This model never owns an engine terminal or acknowledges it. Keep the shared
/// operation center alive independently of the native window and this model.
pub struct DropTarget {
    owner: u64,
    sequence: u64,
    active: Option<Active>,
    limits: Limits,
}
impl DropTarget {
    pub fn new() -> io::Result<Self> {
        Self::with_owner_allocator(&NEXT_OWNER)
    }
    fn with_owner_allocator(next: &AtomicU64) -> io::Result<Self> {
        let owner = next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("drop target identity exhausted"))?;
        Ok(Self {
            owner,
            sequence: 0,
            active: None,
            // Fixed transport/operation admission ceilings, not caller-enlargeable.
            limits: Limits::default(),
        })
    }
    /// Host has validated a new native offer, with at most bounded MIME metadata.
    /// No position or intent is inferred from the application's last pointer.
    pub fn enter(&mut self, uri_list_offered: bool) -> Result<DropId, Error> {
        if self.active.is_some() {
            return Err(Error::Busy);
        }
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Exhausted)?;
        let id = DropId {
            owner: self.owner,
            sequence: self.sequence,
        };
        self.active = Some(Active {
            id,
            uri_list_offered,
            peer_alive: true,
            state: State::Hovering { accepted: false },
        });
        Ok(id)
    }
    fn active(&self, id: DropId) -> Result<&Active, Error> {
        self.active
            .as_ref()
            .filter(|a| a.id == id)
            .ok_or(Error::Stale)
    }
    fn active_mut(&mut self, id: DropId) -> Result<&mut Active, Error> {
        self.active
            .as_mut()
            .filter(|a| a.id == id)
            .ok_or(Error::Stale)
    }
    pub fn phase(&self, id: DropId) -> Result<Phase, Error> {
        Ok(match self.active(id)?.state {
            State::Hovering { .. } => Phase::Hovering,
            State::Receiving { .. } => Phase::Receiving,
            State::Ready { .. } => Phase::Ready,
            State::Running(_) => Phase::Running,
            State::Finished(_) => Phase::Finished,
        })
    }
    /// Recompute on every native action/hit-target change. The caller supplies
    /// current destination validity and global mutation availability, without I/O.
    /// Move/Link remain refused until their own verified engines exist.
    pub fn negotiate(
        &mut self,
        id: DropId,
        intent: Intent,
        destination_available: bool,
    ) -> Result<Status, Error> {
        let active = self.active_mut(id)?;
        let State::Hovering { accepted } = &mut active.state else {
            return Err(Error::WrongPhase);
        };
        *accepted = active.uri_list_offered && intent == Intent::Copy && destination_available;
        Ok(Status {
            accepted: *accepted,
            action: accepted.then_some(Intent::Copy),
        })
    }
    /// Freeze the last negotiated action at the native Drop boundary. The host
    /// must pin the actual hit destination then, before navigation can change it.
    pub fn begin_receive(&mut self, id: DropId, destination: PathBuf) -> Result<(), Error> {
        self.active(id)?;
        if !destination.is_absolute() || destination.as_os_str().len() > self.limits.max_path_bytes
        {
            return Err(Error::InvalidDestination);
        }
        let active = self.active_mut(id)?;
        match active.state {
            State::Hovering { accepted: true } => active.state = State::Receiving { destination },
            State::Hovering { accepted: false } => return Err(Error::NotAccepted),
            _ => return Err(Error::WrongPhase),
        }
        Ok(())
    }
    /// Exactly one complete, bounded transport payload, decoded all-or-nothing.
    /// Direct/INCR framing belongs to the native selection transport, not here.
    pub fn receive_uri_list(&mut self, id: DropId, payload: &[u8]) -> Result<(), Error> {
        if !matches!(self.active(id)?.state, State::Receiving { .. }) {
            return Err(Error::WrongPhase);
        }
        match decode_uri_list(payload, &self.limits) {
            Ok(paths) => {
                let active = self.active_mut(id)?;
                let State::Receiving { destination } =
                    std::mem::replace(&mut active.state, State::Hovering { accepted: false })
                else {
                    unreachable!()
                };
                active.state = State::Ready { paths, destination };
            }
            Err(error) => {
                self.reject(id)?;
                return Err(Error::Payload(error));
            }
        }
        Ok(())
    }
    /// Call only after application-wide mutation admission. Uses the same copy
    /// service as Paste. Submit rejection restores owned native paths for retry;
    /// successful admission does not complete the native drop.
    pub fn submit_copy(&mut self, id: DropId, session: &mut CopySession) -> Result<JobId, Error> {
        if !matches!(self.active(id)?.state, State::Ready { .. }) {
            return Err(Error::WrongPhase);
        }
        let active = self.active_mut(id)?;
        let State::Ready { paths, destination } =
            std::mem::replace(&mut active.state, State::Hovering { accepted: false })
        else {
            unreachable!()
        };
        let batch = CopyBatch::new(paths, destination).with_options(CopyOptions {
            metadata: MetadataPolicy::Basic,
            ..CopyOptions::default()
        });
        match session.submit(batch) {
            Ok(job) => {
                active.state = State::Running(job);
                Ok(job)
            }
            Err(error) => {
                active.state = State::Ready {
                    paths: error.batch.sources,
                    destination: error.batch.destination_folder,
                };
                Err(Error::Submit(error.kind))
            }
        }
    }
    /// Borrow the bound service's actual terminal, never a caller-created success
    /// flag/receipt. Retains only scalar protocol evidence, not its large report.
    pub fn complete_copy(&mut self, id: DropId, session: &CopySession) -> Result<Finish, Error> {
        let active = self.active_mut(id)?;
        let State::Running(job) = active.state else {
            return Err(Error::WrongPhase);
        };
        let terminal = session.terminal().ok_or(Error::NotTerminal)?;
        if terminal.id != job {
            return Err(Error::WrongJob);
        }
        let clean = terminal.stop == BatchStop::Complete
            && terminal.preflight_error.is_none()
            && !terminal.roots.is_empty()
            && terminal.committed_roots == terminal.roots.len()
            && terminal.roots.iter().all(|root| {
                matches!(&root.outcome, RootOutcome::Committed(receipt)
                    if qualified_basic_receipt(receipt))
            });
        let disposition = if clean {
            Disposition::Committed
        } else {
            match terminal.stop {
                BatchStop::CommittedUncertain => Disposition::CommittedUncertain,
                BatchStop::RecoveryRequired => Disposition::RecoveryRequired,
                _ if terminal.committed_roots > 0 => Disposition::Partial,
                BatchStop::Cancelled => Disposition::Cancelled,
                _ => Disposition::Failed,
            }
        };
        let finish = Finish {
            id,
            disposition,
            committed_roots: terminal.committed_roots,
            total_roots: terminal.roots.len(),
            notify_peer: active.peer_alive,
        };
        active.state = State::Finished(finish);
        Ok(finish)
    }
    /// Reject/abort an offer or transfer. An admitted job must instead be
    /// cancelled and observed through its actual terminal via complete_copy.
    pub fn reject(&mut self, id: DropId) -> Result<Finish, Error> {
        let active = self.active_mut(id)?;
        if matches!(active.state, State::Running(_) | State::Finished(_)) {
            return Err(Error::WrongPhase);
        }
        let finish = Finish {
            id,
            disposition: Disposition::Rejected,
            committed_roots: 0,
            total_roots: 0,
            notify_peer: active.peer_alive,
        };
        active.state = State::Finished(finish);
        Ok(finish)
    }
    /// Cancellation is a request. A racing verified commit stays successful.
    pub fn cancel_copy(&self, id: DropId, session: &CopySession) -> Result<(), Error> {
        let State::Running(job) = self.active(id)?.state else {
            return Err(Error::WrongPhase);
        };
        session.cancel(job).map_err(Error::JobControl)
    }
    /// Peer disappearance/transport timeout cannot discard an admitted job or
    /// turn it into an unverified failure. Operation-center ownership continues.
    pub fn peer_lost(&mut self, id: DropId) -> Result<(), Error> {
        let active = self.active_mut(id)?;
        active.peer_alive = false;
        match &mut active.state {
            State::Running(_) => {}
            State::Finished(finish) => finish.notify_peer = false,
            _ => {
                self.reject(id)?;
            }
        }
        Ok(())
    }
    pub fn finish(&self, id: DropId) -> Result<Option<Finish>, Error> {
        Ok(match self.active(id)?.state {
            State::Finished(finish) => Some(finish),
            _ => None,
        })
    }
    /// Release after native reply/abandonment. Engine acknowledgement remains a
    /// separate operation-center action; this cannot admit a second mutation.
    pub fn acknowledge(&mut self, id: DropId) -> Result<(), Error> {
        if !matches!(self.active(id)?.state, State::Finished(_)) {
            return Err(Error::WrongPhase);
        }
        self.active = None;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use browser_transfer::encode_uri_list;
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".test-work")
                .join(format!(
                    "dnd-{}-{}",
                    std::process::id(),
                    NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn source(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::write(&p, b"verified payload").unwrap();
            p
        }
        fn dest(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::create_dir(&p).unwrap();
            p
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn wait_terminal(session: &mut CopySession) {
        let end = Instant::now() + Duration::from_secs(10);
        while session.terminal().is_none() {
            session.poll();
            assert!(Instant::now() < end, "copy terminal timed out");
            thread::yield_now();
        }
    }
    fn ready(target: &mut DropTarget, source: &[PathBuf], destination: PathBuf) -> DropId {
        let id = target.enter(true).unwrap();
        assert!(target.negotiate(id, Intent::Copy, true).unwrap().accepted);
        target.begin_receive(id, destination).unwrap();
        target
            .receive_uri_list(id, &encode_uri_list(source, &Limits::default()).unwrap())
            .unwrap();
        assert!(target.finish(id).unwrap().is_none());
        id
    }

    #[test]
    fn native_identity_destination_and_verified_receipt_survive_delayed_admission() {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let native = std::ffi::OsString::from_vec(b"native-\xff%25\nfile".to_vec());
        let source = f.0.join(&native);
        fs::write(&source, b"native payload").unwrap();
        let destination = f.dest("actual-hit");
        let other = f.dest("later-navigation");
        let mut target = DropTarget::new().unwrap();
        let id = ready(
            &mut target,
            std::slice::from_ref(&source),
            destination.clone(),
        );
        assert!(matches!(
            target.negotiate(id, Intent::Move, true),
            Err(Error::WrongPhase)
        ));
        assert!(matches!(target.enter(true), Err(Error::Busy)));
        let mut session = CopySession::new().unwrap();
        let job = target.submit_copy(id, &mut session).unwrap();
        assert!(target.finish(id).unwrap().is_none());
        wait_terminal(&mut session);
        let finish = target.complete_copy(id, &session).unwrap();
        assert!(finish.success());
        assert_eq!(finish.performed_action(), Some(Intent::Copy));
        assert_eq!((finish.committed_roots(), finish.total_roots()), (1, 1));
        assert_eq!(
            fs::read(destination.join(&native)).unwrap(),
            b"native payload"
        );
        assert_eq!(fs::read(source).unwrap(), b"native payload");
        assert!(fs::read_dir(other).unwrap().next().is_none());
        target.acknowledge(id).unwrap();
        // Native acknowledgement cannot acknowledge/clear the actual operation.
        assert_eq!(session.terminal().unwrap().id, job);
    }

    #[test]
    fn move_link_unknown_mime_and_unavailable_destination_never_admit() {
        let f = Fixture::new();
        for offered in [false, true] {
            let mut target = DropTarget::new().unwrap();
            let id = target.enter(offered).unwrap();
            for intent in [Intent::Move, Intent::Link] {
                assert_eq!(
                    target.negotiate(id, intent, true).unwrap(),
                    Status {
                        accepted: false,
                        action: None
                    }
                );
                assert!(matches!(
                    target.begin_receive(id, f.0.clone()),
                    Err(Error::NotAccepted)
                ));
            }
            assert!(!target.negotiate(id, Intent::Copy, false).unwrap().accepted);
            assert_eq!(
                target.negotiate(id, Intent::Copy, true).unwrap().accepted,
                offered
            );
            assert!(!target.reject(id).unwrap().success());
        }
    }

    #[test]
    fn malformed_partial_and_oversized_payloads_fail_without_subset_or_output() {
        let f = Fixture::new();
        for payload in [
            b"file:///valid\r\nfile://remote/bad\r\n".to_vec(),
            b"file:///bad%00\n".to_vec(),
            vec![b'x'; (8 << 20) + 1],
        ] {
            let mut target = DropTarget::new().unwrap();
            let id = target.enter(true).unwrap();
            target.negotiate(id, Intent::Copy, true).unwrap();
            target.begin_receive(id, f.0.clone()).unwrap();
            assert!(matches!(
                target.receive_uri_list(id, &payload),
                Err(Error::Payload(_))
            ));
            assert_eq!(
                target.finish(id).unwrap().unwrap().disposition(),
                Disposition::Rejected
            );
            let mut session = CopySession::new().unwrap();
            assert!(matches!(
                target.submit_copy(id, &mut session),
                Err(Error::WrongPhase)
            ));
            assert!(session.terminal().is_none());
            assert!(fs::read_dir(&f.0).unwrap().next().is_none());
        }
    }

    #[test]
    fn full_drop_and_job_identity_reject_foreign_recreated_and_late_callbacks() {
        let f = Fixture::new();
        let source = f.source("source");
        let mut first = DropTarget::new().unwrap();
        let a = ready(&mut first, std::slice::from_ref(&source), f.dest("a"));
        let mut second = DropTarget::new().unwrap();
        let b = ready(&mut second, std::slice::from_ref(&source), f.dest("b"));
        assert_eq!(a.sequence(), b.sequence());
        assert_ne!(a, b);
        assert!(matches!(second.reject(a), Err(Error::Stale)));
        assert!(matches!(second.peer_lost(a), Err(Error::Stale)));
        assert!(matches!(second.acknowledge(a), Err(Error::Stale)));
        let mut jobs_a = CopySession::new().unwrap();
        let mut jobs_b = CopySession::new().unwrap();
        let ja = first.submit_copy(a, &mut jobs_a).unwrap();
        let jb = second.submit_copy(b, &mut jobs_b).unwrap();
        assert_eq!(ja.get(), jb.get());
        assert!(matches!(
            second.cancel_copy(b, &jobs_a),
            Err(Error::JobControl(ControlError::WrongJob))
        ));
        wait_terminal(&mut jobs_a);
        wait_terminal(&mut jobs_b);
        assert!(matches!(
            second.complete_copy(b, &jobs_a),
            Err(Error::WrongJob)
        ));
        assert_eq!(second.phase(b).unwrap(), Phase::Running);
        assert!(second.complete_copy(b, &jobs_b).unwrap().success());
        first.complete_copy(a, &jobs_a).unwrap();
        first.acknowledge(a).unwrap();
        let next = first.enter(true).unwrap();
        assert_ne!(next, a);
        assert!(matches!(first.reject(a), Err(Error::Stale)));
        drop(first);
        let mut recreated = DropTarget::new().unwrap();
        let new = recreated.enter(true).unwrap();
        assert_ne!(a, new);
        assert!(matches!(recreated.reject(a), Err(Error::Stale)));
    }

    #[test]
    fn busy_copy_session_restores_payload_and_frozen_destination_for_retry() {
        let f = Fixture::new();
        let source = f.source("source");
        let mut session = CopySession::new().unwrap();
        let old_job = session
            .submit(CopyBatch::new(vec![source.clone()], f.dest("existing-job")))
            .unwrap();
        wait_terminal(&mut session);
        let mut target = DropTarget::new().unwrap();
        let destination = f.dest("drop-hit");
        let id = ready(&mut target, &[source], destination.clone());
        assert!(matches!(
            target.submit_copy(id, &mut session),
            Err(Error::Submit(SubmitErrorKind::Busy))
        ));
        assert_eq!(target.phase(id).unwrap(), Phase::Ready);
        session.acknowledge(old_job).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            match target.submit_copy(id, &mut session) {
                Ok(_) => break,
                Err(Error::Submit(SubmitErrorKind::Busy)) => {
                    assert!(Instant::now() < end, "copy acknowledgement timed out");
                    thread::yield_now();
                }
                other => panic!("unexpected retry result: {other:?}"),
            }
        }
        wait_terminal(&mut session);
        assert!(target.complete_copy(id, &session).unwrap().success());
        assert_eq!(
            fs::read(destination.join("source")).unwrap(),
            b"verified payload"
        );
    }

    #[test]
    fn partial_commit_is_not_native_success_and_no_source_is_removed() {
        let f = Fixture::new();
        let source = f.source("first");
        let missing = f.0.join("missing");
        let destination = f.dest("hit");
        let mut target = DropTarget::new().unwrap();
        let id = ready(&mut target, &[source.clone(), missing], destination.clone());
        let mut session = CopySession::new().unwrap();
        target.submit_copy(id, &mut session).unwrap();
        wait_terminal(&mut session);
        let finish = target.complete_copy(id, &session).unwrap();
        assert_eq!(finish.disposition(), Disposition::Partial);
        assert!(!finish.success());
        assert_eq!(finish.performed_action(), None);
        assert_eq!((finish.committed_roots(), finish.total_roots()), (1, 2));
        assert!(source.exists() && destination.join("first").exists());
        assert!(!destination.join("missing").exists());
    }

    #[test]
    fn peer_loss_and_late_cancel_preserve_real_commit_and_operation_center_ownership() {
        let f = Fixture::new();
        let source = f.source("source");
        let mut target = DropTarget::new().unwrap();
        let id = ready(&mut target, &[source], f.dest("hit"));
        let mut session = CopySession::new().unwrap();
        target.submit_copy(id, &mut session).unwrap();
        target.peer_lost(id).unwrap();
        wait_terminal(&mut session);
        target.cancel_copy(id, &session).unwrap();
        let finish = target.complete_copy(id, &session).unwrap();
        assert!(finish.success());
        assert!(!finish.notify_peer());
        assert!(matches!(target.reject(id), Err(Error::WrongPhase)));
        drop(target);
        assert_eq!(session.terminal().unwrap().stop, BatchStop::Complete);
    }

    #[test]
    fn checked_owners_and_sequences_fail_without_reusing_ids() {
        let exhausted = AtomicU64::new(u64::MAX);
        assert!(DropTarget::with_owner_allocator(&exhausted).is_err());
        assert_eq!(exhausted.load(Ordering::Relaxed), u64::MAX);
        let mut target = DropTarget::new().unwrap();
        target.sequence = u64::MAX;
        assert!(matches!(target.enter(true), Err(Error::Exhausted)));
        assert!(target.active.is_none());
    }

    #[test]
    fn public_copy_bridge_preserves_executable_permissions_and_exact_native_timestamps() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let f = Fixture::new();
        let source = f.source("executable");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_accessed(std::time::UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_789))
                    .set_modified(
                        std::time::UNIX_EPOCH + Duration::new(1_600_000_001, 987_654_321),
                    ),
            )
            .unwrap();
        let destination = f.dest("hit");
        let mut target = DropTarget::new().unwrap();
        let id = ready(&mut target, &[source], destination.clone());
        let mut session = CopySession::new().unwrap();
        target.submit_copy(id, &mut session).unwrap();
        wait_terminal(&mut session);
        assert!(target.complete_copy(id, &session).unwrap().success());
        let result = fs::metadata(destination.join("executable")).unwrap();
        assert_eq!(result.mode() & 0o7777, 0o751);
        assert_eq!(
            (result.atime(), result.atime_nsec()),
            (1_600_000_000, 123_456_789)
        );
        assert_eq!(
            (result.mtime(), result.mtime_nsec()),
            (1_600_000_001, 987_654_321)
        );
        let RootOutcome::Committed(receipt) = &session.terminal().unwrap().roots[0].outcome else {
            panic!("bridge did not commit")
        };
        assert_eq!(
            receipt.metadata,
            MetadataCompletion::Achieved(MetadataPolicy::Basic)
        );
    }

    #[test]
    fn public_copy_bridge_rejects_readonly_basic_roots_before_output_and_never_finishes_success() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let f = Fixture::new();
        let source = f.dest("readonly");
        fs::write(source.join("child"), b"keep").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o555)).unwrap();
        let destination = f.dest("hit");
        let mut target = DropTarget::new().unwrap();
        let id = ready(
            &mut target,
            std::slice::from_ref(&source),
            destination.clone(),
        );
        let mut session = CopySession::new().unwrap();
        target.submit_copy(id, &mut session).unwrap();
        wait_terminal(&mut session);
        let finish = target.complete_copy(id, &session).unwrap();
        let source_mode = fs::metadata(&source).unwrap().mode() & 0o777;
        // Restore our fixture before assertions so a failing regression cleans up.
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!finish.success());
        assert_eq!(finish.disposition(), Disposition::Failed);
        assert_eq!(finish.committed_roots(), 0);
        assert!(
            fs::read_dir(&destination).unwrap().next().is_none(),
            "prewrite rejection left output/staging"
        );
        assert_eq!(source_mode, 0o555);
        let RootOutcome::Failed {
            error,
            cleanup: Cleanup::Clean,
        } = &session.terminal().unwrap().roots[0].outcome
        else {
            panic!("expected clean Basic metadata rejection")
        };
        assert_eq!(
            error.kind,
            browser_operations::ErrorKind::UnsupportedMetadata
        );
        assert_eq!(error.phase, "metadata admission");
    }

    #[test]
    fn private_policy_engine_terminal_cannot_produce_success_even_when_clean_and_synced() {
        let f = Fixture::new();
        let source = f.source("source");
        let mut session = CopySession::new().unwrap();
        let job = session
            .submit(CopyBatch::new(vec![source], f.dest("private")))
            .unwrap();
        wait_terminal(&mut session);
        let RootOutcome::Committed(receipt) = &session.terminal().unwrap().roots[0].outcome else {
            panic!("private compatibility engine did not commit")
        };
        assert_eq!(
            receipt.metadata,
            MetadataCompletion::Achieved(MetadataPolicy::Private)
        );
        assert_eq!(receipt.durability, Durability::Synced);
        assert!(matches!(receipt.cleanup, Cleanup::Clean));
        let mut target = DropTarget::new().unwrap();
        let id = target.enter(true).unwrap();
        // Inject a genuine legacy/private terminal binding solely to exercise
        // the achieved-policy guard independently of the Basic submit boundary.
        target.active_mut(id).unwrap().state = State::Running(job);
        let finish = target.complete_copy(id, &session).unwrap();
        assert!(!finish.success());
        assert_eq!(finish.performed_action(), None);
        assert_eq!(finish.committed_roots(), 1);
    }
    #[test]
    fn metadata_incomplete_or_unsynced_receipts_never_qualify_for_successful_finish() {
        let f = Fixture::new();
        let source = f.source("source");
        let destination = f.dest("hit").join("copied");
        let plan = browser_operations::plan_copy_new(
            source,
            destination,
            browser_operations::CopyLimits::default(),
            &browser_operations::Cancellation::default(),
        )
        .unwrap();
        let browser_operations::CopyOutcome::Committed(mut receipt) =
            browser_operations::execute_copy_new(
                plan,
                &browser_operations::Cancellation::default(),
                |_| {},
            )
        else {
            panic!("copy did not commit")
        };
        assert!(!qualified_basic_receipt(&receipt));
        receipt.metadata = MetadataCompletion::Incomplete {
            requested: MetadataPolicy::Basic,
        };
        assert_eq!(receipt.durability, Durability::Synced);
        assert!(matches!(receipt.cleanup, Cleanup::Clean));
        assert!(
            !qualified_basic_receipt(&receipt),
            "a requested Basic policy is not achievement"
        );
        receipt.metadata = MetadataCompletion::Achieved(MetadataPolicy::Basic);
        receipt.durability = Durability::Uncertain;
        assert!(
            !qualified_basic_receipt(&receipt),
            "metadata achievement alone is not durable success"
        );
    }
}
