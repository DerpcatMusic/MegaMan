//! App-owned copy job UI. Hiding the screen never drops or cancels an accepted job.
use super::{
    Glyph, action, bytes, chrome, clicked, color, detail, glyph, ground, hairline, label, muted,
};
use browser_operations::session::{
    BatchOutcome, BatchPhase, BatchStop, CopyBatch, CopyProgress, CopySession, JobId, RootOutcome,
    RootReport, SubmitErrorKind,
};
use browser_operations::{
    Cleanup, CopyOptions, Durability, ErrorKind, MetadataCompletion, MetadataPolicy,
    OperationError, RootPublication,
};
use mui::prelude::*;
use std::path::{Path, PathBuf};

const RECEIPT_ROW: f64 = 76.;

pub(super) struct TransfersUi {
    session: Option<CopySession>,
    queued: Option<CopyBatch>,
    active: Option<JobId>,
    progress: Option<CopyProgress>,
    terminal_seen: bool,
    terminal_message: Option<String>,
    terminal_facts: ReceiptFacts,
    local_terminal: Option<String>,
    destination: PathBuf,
    visible: bool,
    epoch: u64,
    initial_focus: bool,
    list: ListState,
    viewport: Size,
    window: Size,
    rows_built: usize,
    cancel_requested: bool,
    refresh_destination: Option<PathBuf>,
    geometry_changed: bool,
}

impl TransfersUi {
    pub(super) fn new() -> Self {
        Self {
            session: None,
            queued: None,
            active: None,
            progress: None,
            terminal_seen: false,
            terminal_message: None,
            terminal_facts: ReceiptFacts::default(),
            local_terminal: None,
            destination: PathBuf::new(),
            visible: false,
            epoch: 0,
            initial_focus: false,
            list: ListState::uniform_count(0, RECEIPT_ROW),
            viewport: Size::new(320., 100.),
            window: Size::new(320., 240.),
            rows_built: 0,
            cancel_requested: false,
            refresh_destination: None,
            geometry_changed: false,
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.queued.is_some() || self.active.is_some() || self.local_terminal.is_some()
    }
    pub(super) fn visible(&self) -> bool {
        self.visible
    }
    pub(super) fn show(&mut self) {
        self.visible = true;
        self.initial_focus = true;
        self.geometry_changed = true;
    }
    pub(super) fn hide(&mut self) {
        self.visible = false;
        self.geometry_changed = true;
    }
    pub(super) fn action_id(&self, action: &str) -> String {
        format!("copy-transfer/{}/{action}", self.epoch)
    }
    pub(super) fn start(&mut self, sources: Vec<PathBuf>, destination: PathBuf) -> bool {
        if self.busy() {
            self.show();
            return false;
        }
        let Some(epoch) = self.epoch.checked_add(1) else {
            return false;
        };
        self.epoch = epoch;
        self.destination = destination.clone();
        self.progress = None;
        self.cancel_requested = false;
        self.terminal_seen = false;
        self.terminal_message = None;
        self.terminal_facts = ReceiptFacts::default();
        self.list = ListState::uniform_count(0, RECEIPT_ROW);
        self.show();
        if self.session.is_none() {
            match CopySession::new() {
                Ok(session) => self.session = Some(session),
                Err(_) => {
                    self.local_terminal =
                        Some("The copy worker could not start. No files were copied.".into());
                    return false;
                }
            }
        }
        self.queued = Some(
            CopyBatch::new(sources, destination)
                .keep_both()
                .with_options(CopyOptions {
                    metadata: MetadataPolicy::Basic,
                    root_publication: RootPublication::FinalizeBasicAfterPublish,
                    ..CopyOptions::default()
                }),
        );
        self.poll();
        true
    }
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.geometry_changed);
        let Some(session) = &mut self.session else {
            return changed;
        };
        if let Some(batch) = self.queued.take() {
            match session.submit(batch) {
                Ok(id) => {
                    self.active = Some(id);
                    changed = true;
                }
                Err(error) if error.kind == SubmitErrorKind::Busy => {
                    self.queued = Some(error.batch)
                }
                Err(_) => {
                    self.local_terminal =
                        Some("The copy request was rejected. No files were copied.".into());
                    changed = true;
                }
            }
        }
        if let Some(progress) = session.poll()
            && Some(progress.id) == self.active
        {
            self.progress = Some(progress);
            changed = true;
        }
        if !self.terminal_seen
            && let Some(terminal) = session.terminal().filter(|t| Some(t.id) == self.active)
        {
            self.terminal_seen = true;
            self.terminal_facts = ReceiptFacts::from_roots(&terminal.roots);
            self.terminal_message = Some(terminal_summary(
                terminal.stop,
                terminal.committed_roots,
                self.terminal_facts,
            ));
            if terminal.committed_roots > 0 {
                self.refresh_destination = Some(self.destination.clone());
            }
            self.list = ListState::uniform_count(terminal.roots.len(), RECEIPT_ROW);
            changed = true;
        }
        changed
    }
    pub(super) fn take_refresh_destination(&mut self) -> Option<PathBuf> {
        self.refresh_destination.take()
    }
    fn cancel(&mut self) {
        if self.queued.take().is_some() {
            self.local_terminal =
                Some("Copy cancelled before submission. No files were copied.".into());
        } else if !self.terminal_seen
            && let (Some(session), Some(id)) = (&self.session, self.active)
            && session.cancel(id).is_ok()
        {
            self.cancel_requested = true;
        }
    }
    fn acknowledge(&mut self) -> bool {
        if let Some(id) = self.active {
            if !self.terminal_seen
                || self
                    .session
                    .as_mut()
                    .is_none_or(|s| s.acknowledge(id).is_err())
            {
                return false;
            }
        } else if self.local_terminal.is_none() {
            return false;
        }
        self.active = None;
        self.local_terminal = None;
        self.progress = None;
        self.terminal_seen = false;
        self.terminal_message = None;
        self.terminal_facts = ReceiptFacts::default();
        self.list = ListState::uniform_count(0, RECEIPT_ROW);
        self.hide();
        true
    }
    fn terminal(&self) -> Option<&BatchOutcome> {
        self.session
            .as_ref()
            .and_then(CopySession::terminal)
            .filter(|t| Some(t.id) == self.active)
    }
    pub(super) fn summary(&self) -> String {
        if let Some(message) = &self.local_terminal {
            return message.clone();
        }
        if let Some(message) = &self.terminal_message {
            return message.clone();
        }
        if self.cancel_requested {
            return "Cancellation requested; published items remain while final metadata checks finish.".into();
        }
        if let Some(progress) = self.progress {
            let phase = match progress.phase {
                BatchPhase::Preflight => "Checking paths",
                BatchPhase::Planning => "Planning",
                BatchPhase::Copying => "Copying",
                BatchPhase::Verifying => "Verifying copied bytes",
                BatchPhase::BeforeCommit => "Checking publication",
                BatchPhase::MetadataApplied => "Preserving permissions and timestamps",
                BatchPhase::ContentPublished => "Published; finalizing permissions and timestamps",
                BatchPhase::RootCommitted => "Item published",
            };
            return format!(
                "{phase} · item {} of {} · {} published",
                (progress.root_index + 1).min(progress.roots),
                progress.roots,
                progress.committed_roots
            );
        }
        if self.queued.is_some() {
            "Waiting for the copy worker…".into()
        } else if self.active.is_some() {
            "Checking copy request…".into()
        } else {
            "No copy job".into()
        }
    }
    /// Returns true only for a terminal acknowledgement, so the host can refresh its folder.
    pub(super) fn build(&mut self, ui: &mut Ui) -> (El, bool) {
        self.poll();
        let back_id = self.action_id("back");
        let cancel_id = self.action_id("cancel");
        let ack_id = self.action_id("acknowledge");
        if self.initial_focus {
            ui.focus(&back_id);
            self.initial_focus = false;
        }
        if clicked(ui, &back_id) || ui.shortcuts().iter().any(|k| k.key == Key::Escape) {
            self.hide();
        }
        let terminal = self.terminal_seen || self.local_terminal.is_some();
        if !terminal && clicked(ui, &cancel_id) {
            self.cancel();
        }
        let acknowledged = terminal && clicked(ui, &ack_id) && self.acknowledge();
        let header = row([
            action(
                ui,
                &back_id,
                "Back to folder · Escape (copy continues)",
                glyph(Glyph::Back, muted()),
                true,
                false,
            ),
            label("Copy").grow(1).basis(0).min_w(0).clip(),
            if terminal {
                action(
                    ui,
                    &ack_id,
                    "Acknowledge this copy result",
                    label("Done"),
                    true,
                    false,
                )
            } else {
                action(
                    ui,
                    &cancel_id,
                    "Request cancellation; published items remain",
                    label("Cancel"),
                    !self.cancel_requested,
                    false,
                )
            },
        ])
        .gap(7)
        .pad((6, 4))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0);
        let destination = path_label(&self.destination);
        let destination = row([detail(format!("To: {destination}"))
            .grow(1)
            .basis(0)
            .lines(1)
            .min_w(0)
            .clip()
            .tip(path_label(&self.destination))])
        .pad((8, 4))
        .fill(chrome())
        .shrink(0);
        let summary = self.summary();
        let status = row([detail(summary.clone())
            .grow(1)
            .basis(0)
            .lines(2)
            .min_w(0)
            .clip()
            .tip(summary)])
        .pad((8, 5))
        .fill(chrome())
        .shrink(0)
        .id("copy-transfer-status");
        let preflight = self
            .terminal()
            .and_then(|t| t.preflight_error.as_ref())
            .map(error_message);
        let results = self.results(ui);
        let mut sections = vec![header, hairline(), destination, hairline(), status];
        if let Some(error) = preflight {
            sections.push(
                detail(error.clone())
                    .lines(2)
                    .clip()
                    .tip(error)
                    .pad((8, 4))
                    .fill(color(0xffb4ab))
                    .shrink(0),
            );
        }
        sections.extend([
            hairline(),
            results,
            hairline(),
            row([detail("Never overwrites (keeps both) · each item publishes separately · Basic metadata")
                .grow(1).basis(0).min_w(0)
                .lines(1)
                .clip().tip("Never overwrites: a name clash keeps both as \"name (2)\". Each item publishes separately. Basic preserves ordinary rwx bits and planned atime/mtime; ownership, ACLs and extended attributes are not preserved.")])
                .pad((8, 4))
                .fill(chrome())
                .shrink(0),
        ]);
        (
            col(sections)
                .w(pct(100))
                .h(pct(100))
                .min_w(0)
                .min_h(0)
                .fill(ground())
                .id("copy-transfer-root")
                .named("Copy progress and results"),
            acknowledged,
        )
    }
    fn results(&mut self, ui: &mut Ui) -> El {
        let id = self.action_id("receipts");
        let rows = self
            .session
            .as_ref()
            .and_then(CopySession::terminal)
            .filter(|t| Some(t.id) == self.active)
            .map(|t| t.roots.as_slice())
            .unwrap_or(&[]);
        let list = uniform_list(
            ui,
            &id,
            &mut self.list,
            ListOptions {
                overscan: RECEIPT_ROW,
                ..ListOptions::new(self.viewport.width.max(1.), self.viewport.height.max(1.))
            },
            |_, item| {
                let report = &rows[item.index];
                let (status, warning) = root_status(report);
                let source = path_label(&report.source);
                let destination = path_label(&report.destination);
                let name = path_label(Path::new(
                    report
                        .source
                        .file_name()
                        .unwrap_or(report.source.as_os_str()),
                ));
                col([
                    label(name).w(pct(100)).min_w(0).lines(1).clip().tip(source),
                    detail(status.clone())
                        .w(pct(100))
                        .min_w(0)
                        .lines(1)
                        .clip()
                        .tip(status)
                        .when(warning, |e| e.fill(color(0xffb4ab))),
                    detail(format!("To: {destination}"))
                        .w(pct(100))
                        .min_w(0)
                        .lines(1)
                        .clip()
                        .tip(destination),
                ])
                .align(Align::Start)
                .gap(2)
                .pad((10, 4))
                .h(RECEIPT_ROW)
                .w(pct(100))
                .min_w(0)
                .fill(ground())
            },
        );
        self.rows_built = list.changed.rows_built;
        list.el
            .w(pct(100))
            .grow(1)
            .basis(0)
            .min_w(0)
            .min_h(0)
            .id("copy-transfer-receipts")
    }
    pub(super) fn after_frame(&mut self, ui: &Ui) {
        if let Some(scene) = ui.scene() {
            if let Some(frame) = scene.surface("copy-transfer-root") {
                self.geometry_changed |= self.window != frame.frame.size;
                self.window = frame.frame.size;
            }
            if let Some(frame) = scene.surface("copy-transfer-receipts") {
                self.geometry_changed |= self.viewport != frame.frame.size;
                self.viewport = frame.frame.size;
            }
        }
    }
    pub(super) fn evidence(&self) -> String {
        let (state, committed, roots) = if let Some(t) = self.terminal() {
            (
                match t.stop {
                    BatchStop::Complete => "complete",
                    BatchStop::Failed => "failed",
                    BatchStop::Cancelled => "cancelled",
                    BatchStop::CommittedUncertain => "committed_uncertain",
                    BatchStop::RecoveryRequired => "recovery_required",
                },
                t.committed_roots,
                t.roots.len(),
            )
        } else {
            (
                if self.queued.is_some() {
                    "queued"
                } else if self.active.is_some() {
                    "running"
                } else if self.local_terminal.is_some() {
                    "rejected"
                } else {
                    "idle"
                },
                self.progress.map_or(0, |p| p.committed_roots),
                self.progress.map_or(0, |p| p.roots),
            )
        };
        format!(
            "{{\"schema_version\":1,\"mode\":\"copy-transfer\",\"state\":\"{state}\",\"visible\":{},\"committed_roots\":{committed},\"roots\":{roots},\"rows_built\":{},\"cancel_requested\":{},\"basic_metadata_verified_roots\":{},\"metadata_incomplete_roots\":{},\"durability_uncertain_roots\":{}}}",
            self.visible,
            self.rows_built,
            self.cancel_requested,
            self.terminal_facts.basic_verified,
            self.terminal_facts.incomplete,
            self.terminal_facts.uncertain
        )
    }
}

fn path_label(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut chars = text.chars();
    let mut label: String = chars
        .by_ref()
        .take(512)
        .map(|c| if c.is_control() { '�' } else { c })
        .collect();
    if chars.next().is_some() {
        label.push_str("… [display truncated]");
    }
    label
}
#[derive(Clone, Copy, Debug, Default)]
struct ReceiptFacts {
    basic_verified: usize,
    incomplete: usize,
    uncertain: usize,
    private_policy: usize,
}
impl ReceiptFacts {
    fn from_roots(roots: &[RootReport]) -> Self {
        let mut facts = Self::default();
        for root in roots {
            if let RootOutcome::Committed(receipt) | RootOutcome::CommittedUncertain(receipt) =
                &root.outcome
            {
                match receipt.metadata {
                    MetadataCompletion::Achieved(MetadataPolicy::Basic) => {
                        facts.basic_verified += 1
                    }
                    MetadataCompletion::Achieved(MetadataPolicy::Private) => {
                        facts.private_policy += 1
                    }
                    MetadataCompletion::Incomplete { .. } => facts.incomplete += 1,
                }
                if receipt.durability == Durability::Uncertain {
                    facts.uncertain += 1;
                }
            }
        }
        facts
    }
}
fn terminal_summary(stop: BatchStop, count: usize, facts: ReceiptFacts) -> String {
    let noun = if count == 1 { "item" } else { "items" };
    let mut message = match stop {
        BatchStop::Complete
            if facts.incomplete == 0
                && facts.uncertain == 0
                && facts.private_policy == 0
                && facts.basic_verified == count =>
        {
            format!("Copy complete · {count} {noun} published; permissions/timestamps verified.")
        }
        BatchStop::Complete | BatchStop::CommittedUncertain => {
            format!("Copy requires review · {count} {noun} published.")
        }
        BatchStop::Failed => format!(
            "Copy stopped on an error · {count} {noun} published; later items were not started."
        ),
        BatchStop::Cancelled => {
            format!(
                "Copy cancelled · {count} {noun} published; content remains in the destination."
            )
        }
        BatchStop::RecoveryRequired => format!(
            "Copy requires recovery review · {count} {noun} published; staging data may remain."
        ),
    };
    if facts.incomplete > 0 {
        message.push_str(&format!(
            " {} published {}: permissions/timestamps not fully verified.",
            facts.incomplete,
            if facts.incomplete == 1 {
                "item"
            } else {
                "items"
            }
        ));
    }
    if facts.uncertain > 0 {
        // Metadata achievement remains a separate fact from sync/journal uncertainty.
        if facts.incomplete == 0 && facts.private_policy == 0 && facts.basic_verified > 0 {
            message.push_str(" Permissions/timestamps verified; durability is uncertain.");
        } else {
            message.push_str(" Durability is uncertain.");
        }
    }
    if facts.private_policy > 0 {
        message.push_str(" Some published items used the private metadata policy.");
    }
    if matches!(stop, BatchStop::Complete | BatchStop::CommittedUncertain)
        && (facts.incomplete > 0 || facts.uncertain > 0 || facts.private_policy > 0)
    {
        message.push_str(" Published content remains in the destination.");
    }
    message
}
fn cleanup_note(cleanup: &Cleanup) -> Option<String> {
    match cleanup {
        Cleanup::Clean => None,
        Cleanup::RecoveryRequired(residue) => {
            let identity = residue.identity.map_or_else(
                || "Recorded identity unavailable".into(),
                |id| format!("Recorded object: device {}, inode {}", id.device, id.inode),
            );
            Some(format!(
                "Staging data remains at last known path {}. {identity}. Do not delete a replacement by path alone.",
                path_label(&residue.path)
            ))
        }
    }
}
fn root_status(report: &RootReport) -> (String, bool) {
    match &report.outcome {
        RootOutcome::NotStarted => ("Not started".into(), false),
        RootOutcome::Committed(receipt) | RootOutcome::CommittedUncertain(receipt) => {
            let incomplete = matches!(receipt.metadata, MetadataCompletion::Incomplete { .. });
            let private_policy =
                receipt.metadata == MetadataCompletion::Achieved(MetadataPolicy::Private);
            let uncertain = receipt.durability == Durability::Uncertain;
            let requires_review = matches!(report.outcome, RootOutcome::CommittedUncertain(_));
            let metadata = match receipt.metadata {
                MetadataCompletion::Achieved(MetadataPolicy::Basic) => {
                    "permissions/timestamps verified"
                }
                MetadataCompletion::Achieved(MetadataPolicy::Private) => "private metadata policy",
                MetadataCompletion::Incomplete { .. } => {
                    "permissions/timestamps not fully verified"
                }
            };
            let mut message = format!(
                "Published; {}{metadata}{} · {} {} · {}",
                if uncertain && !incomplete {
                    "durability uncertain; "
                } else {
                    ""
                },
                if uncertain && incomplete {
                    "; durability uncertain"
                } else if requires_review && !uncertain {
                    "; publication requires review"
                } else {
                    ""
                },
                receipt.entries,
                if receipt.entries == 1 {
                    "entry"
                } else {
                    "entries"
                },
                bytes(receipt.bytes)
            );
            if incomplete || uncertain || requires_review {
                message.push_str(&format!(
                    " · Published object: device {}, inode {}",
                    receipt.identity.device, receipt.identity.inode
                ));
            }
            if !receipt.warnings.is_empty() {
                message.push_str(" · warnings: ");
                for error in receipt.warnings.iter().take(2) {
                    message.push_str(&error_message(error));
                    message.push_str("; ");
                }
                if receipt.warnings.len() > 2 {
                    message.push_str("additional warnings [display truncated]");
                }
            }
            if let Some(note) = cleanup_note(&receipt.cleanup) {
                message.push_str(" · ");
                message.push_str(&note);
            }
            (
                message,
                incomplete
                    || private_policy
                    || uncertain
                    || requires_review
                    || !receipt.warnings.is_empty()
                    || !matches!(receipt.cleanup, Cleanup::Clean),
            )
        }
        RootOutcome::Cancelled { cleanup } => (
            format!(
                "Cancelled before publication{}",
                cleanup_note(cleanup).map_or(String::new(), |n| format!(" · {n}"))
            ),
            !matches!(cleanup, Cleanup::Clean),
        ),
        RootOutcome::Failed { error, cleanup } => (
            format!(
                "{}{}",
                error_message(error),
                cleanup_note(cleanup).map_or(String::new(), |n| format!(" · {n}"))
            ),
            true,
        ),
    }
}
fn error_message(error: &OperationError) -> String {
    let reason = match error.kind {
        ErrorKind::InvalidPath => "The path is not supported",
        ErrorKind::UnsupportedPlatform => "Copy is unavailable on this platform",
        ErrorKind::UnsupportedType => "This file type is not supported",
        ErrorKind::LimitExceeded => "The copy exceeds this build's resource limits",
        ErrorKind::DestinationConflict => {
            "An item already exists at the destination; nothing was overwritten"
        }
        ErrorKind::Containment => {
            "The copy crosses a protected filesystem boundary or contains its destination"
        }
        ErrorKind::SourceChanged => "The source changed during copying",
        ErrorKind::DestinationChanged => "The destination changed during copying",
        ErrorKind::Cancelled => "Copy cancelled before publication",
        ErrorKind::AtomicCommitUnsupported => "This filesystem cannot publish the copy safely",
        ErrorKind::VerificationFailed => "Copied data did not pass verification",
        ErrorKind::InvalidJournal => "Copy recovery data requires review",
        ErrorKind::UnsupportedMetadata => {
            "This item's permissions or timestamps cannot be preserved by this build"
        }
        ErrorKind::Io => "Copy could not finish because of an I/O error",
    };
    format!("{reason} · {}", path_label(&error.path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::make_ui;
    use std::{
        fs,
        os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
        },
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let parent =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/transfers-generated");
            fs::create_dir_all(&parent).unwrap();
            let root = parent.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    fn repair_fixture(path: &Path) {
        if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            for entry in fs::read_dir(path).unwrap() {
                repair_fixture(&entry.unwrap().path());
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            repair_fixture(&self.0);
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn wait(view: &mut TransfersUi) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while view.terminal().is_none() {
            assert!(
                Instant::now() < deadline,
                "copy timed out: {}",
                view.summary()
            );
            view.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn step(view: &mut TransfersUi, ui: &mut Ui, size: Size, input: Input) {
        let (tree, _) = view.build(ui);
        ui.frame(tree, Some(size), input, 0.016).unwrap();
        view.after_frame(ui);
    }
    #[test]
    fn hidden_job_preserves_native_names_payload_exec_bits_and_symlink_without_touching_target() {
        let fixture = Fixture::new();
        let source = fixture.path("source");
        let dest = fixture.path("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        let native = std::ffi::OsString::from_vec(b"run-\xff".to_vec());
        fs::write(source.join(&native), b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(source.join(&native), fs::Permissions::from_mode(0o751)).unwrap();
        let outside = fixture.path("outside");
        fs::write(&outside, b"outside unchanged").unwrap();
        symlink(&outside, source.join("link")).unwrap();
        let mut view = TransfersUi::new();
        assert!(view.start(vec![source.clone()], dest.clone()));
        view.hide();
        wait(&mut view);
        assert!(!view.visible());
        assert!(view.busy());
        assert_eq!(view.terminal().unwrap().stop, BatchStop::Complete);
        assert_eq!(
            fs::read(dest.join("source").join(&native)).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
        assert_eq!(
            fs::metadata(dest.join("source").join(&native))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
        assert_eq!(fs::read_link(dest.join("source/link")).unwrap(), outside);
        assert_eq!(fs::read(&outside).unwrap(), b"outside unchanged");
        assert!(view.acknowledge());
        assert!(!view.busy());
    }
    #[test]
    fn fail_stop_reports_partial_publication_and_holds_admission_until_acknowledged() {
        let fixture = Fixture::new();
        let dest = fixture.path("dest");
        fs::create_dir(&dest).unwrap();
        for name in ["first", "conflict", "later"] {
            fs::write(fixture.path(name), name.as_bytes()).unwrap();
        }
        fs::write(dest.join("conflict"), b"existing destination").unwrap();
        let mut view = TransfersUi::new();
        assert!(
            view.start(
                ["first", "conflict", "later"]
                    .map(|s| fixture.path(s))
                    .to_vec(),
                dest.clone()
            )
        );
        wait(&mut view);
        let outcome = view.terminal().unwrap();
        assert_eq!(outcome.stop, BatchStop::Failed);
        assert_eq!(outcome.committed_roots, 1);
        assert!(matches!(
            outcome.roots[0].outcome,
            RootOutcome::Committed(_)
        ));
        assert!(matches!(
            outcome.roots[1].outcome,
            RootOutcome::Failed { .. }
        ));
        assert!(matches!(outcome.roots[2].outcome, RootOutcome::NotStarted));
        assert!(view.summary().contains("1 item published"));
        assert_eq!(fs::read(dest.join("first")).unwrap(), b"first");
        assert_eq!(
            fs::read(dest.join("conflict")).unwrap(),
            b"existing destination"
        );
        assert!(!dest.join("later").exists());
        assert!(!view.start(vec![fixture.path("later")], dest.clone()));
        let old_ack = view.action_id("acknowledge");
        assert!(view.acknowledge());
        assert!(view.start(vec![fixture.path("later")], dest.clone()));
        assert_ne!(old_ack, view.action_id("acknowledge"));
        wait(&mut view);
        assert_eq!(view.terminal().unwrap().stop, BatchStop::Complete);
    }
    #[test]
    fn intrinsic_terminal_receipts_fit_small_surfaces_and_build_only_visible_rows() {
        let fixture = Fixture::new();
        let dest = fixture.path("dest");
        fs::create_dir(&dest).unwrap();
        let mut sources = Vec::new();
        for i in 0..80 {
            let path = fixture.path(&format!("item-{i}"));
            fs::write(&path, b"owned").unwrap();
            sources.push(path);
        }
        let mut view = TransfersUi::new();
        assert!(view.start(sources, dest));
        wait(&mut view);
        for size in [Size::new(320., 240.), Size::new(640., 480.)] {
            let mut ui = make_ui().unwrap();
            for _ in 0..6 {
                step(&mut view, &mut ui, size, Input::default());
            }
            let scene = ui.scene().unwrap();
            let root = scene.surface("copy-transfer-root").unwrap().frame;
            for id in [
                view.action_id("back"),
                view.action_id("acknowledge"),
                "copy-transfer-status".into(),
                "copy-transfer-receipts".into(),
            ] {
                let frame = scene.surface(&id).unwrap().frame;
                assert!(
                    frame.x >= root.x && frame.right() <= root.right() + 0.01,
                    "{id}: {frame:?}"
                );
                assert!(
                    frame.y >= root.y && frame.bottom() <= root.bottom() + 0.01,
                    "{id}: {frame:?}"
                );
            }
            let receipts = scene.surface("copy-transfer-receipts").unwrap().frame;
            assert!(
                (receipts.size.width - root.size.width).abs() < 0.01,
                "receipt viewport must fill the available width: {receipts:?} vs {root:?}"
            );
            assert!(view.rows_built < 20, "{}", view.rows_built);
        }
    }
    #[test]
    fn cancellation_never_reports_success_and_recovery_paths_are_explicitly_bounded() {
        let mut view = TransfersUi::new();
        view.queued = Some(CopyBatch::new(Vec::new(), PathBuf::from("/unused")));
        view.cancel();
        assert!(view.summary().contains("No files were copied"));
        assert!(view.busy());
        assert!(view.acknowledge());
        let path = PathBuf::from(format!("/{}", "a".repeat(600)));
        assert!(path_label(&path).ends_with("[display truncated]"));
        let error = OperationError {
            kind: ErrorKind::Io,
            phase: "test",
            path: "/owned".into(),
            message: "owned fixture".into(),
            io_kind: None,
            raw_os_error: None,
        };
        let note = cleanup_note(&Cleanup::RecoveryRequired(browser_operations::Residue {
            path: "/owned/staging".into(),
            identity: None,
            error,
        }))
        .unwrap();
        assert!(note.contains("/owned/staging"));
        assert!(note.contains("last known path"));
        assert!(note.contains("Do not delete a replacement"));
    }

    #[test]
    fn viewport_changes_and_navigation_invalidate_once_then_idle() {
        let mut view = TransfersUi::new();
        assert!(!view.poll());
        view.show();
        assert!(view.poll());
        assert!(!view.poll());
        let mut ui = make_ui().unwrap();
        for size in [
            Size::new(1024., 720.),
            Size::new(640., 480.),
            Size::new(320., 240.),
        ] {
            for _ in 0..6 {
                step(&mut view, &mut ui, size, Input::default());
            }
            assert_eq!(view.viewport.width, size.width);
            view.poll();
            assert!(
                !view.poll(),
                "settled geometry must not cause continuous redraw"
            );
        }
        view.hide();
        assert!(view.poll());
        assert!(!view.poll());
    }
    fn presentation_root(metadata: MetadataCompletion, durability: Durability) -> RootReport {
        RootReport {
            source: PathBuf::from("/native/source"),
            destination: PathBuf::from("/native/destination"),
            outcome: RootOutcome::CommittedUncertain(browser_operations::CommitReceipt {
                metadata,
                operation_id: [7; 16],
                destination: PathBuf::from("/native/destination"),
                identity: browser_operations::NativeIdentity {
                    device: 17,
                    inode: 29,
                },
                entries: 3,
                bytes: 42,
                durability,
                warnings: Vec::new(),
                cleanup: Cleanup::Clean,
            }),
        }
    }
    #[test]
    fn receipt_presentation_distinguishes_unknown_metadata_from_verified_metadata_with_uncertain_durability()
     {
        let incomplete = presentation_root(
            MetadataCompletion::Incomplete {
                requested: MetadataPolicy::Basic,
            },
            Durability::Uncertain,
        );
        let (status, warning) = root_status(&incomplete);
        assert!(warning);
        assert!(
            status.starts_with("Published; permissions/timestamps not fully verified"),
            "{status}"
        );
        assert!(status.contains("durability uncertain"));
        assert!(status.contains("device 17, inode 29"));
        assert!(!status.contains("Cancelled") && !status.contains("No files"));
        let facts = ReceiptFacts::from_roots(&[incomplete]);
        let summary = terminal_summary(BatchStop::CommittedUncertain, 1, facts);
        assert!(summary.contains("1 item published."));
        assert!(summary.contains("Published content remains"));
        assert!(summary.contains("1 published item: permissions/timestamps not fully verified"));
        assert!(!summary.contains("Copy complete"));

        let verified = presentation_root(
            MetadataCompletion::Achieved(MetadataPolicy::Basic),
            Durability::Uncertain,
        );
        let (status, warning) = root_status(&verified);
        assert!(warning);
        assert!(
            status.starts_with("Published; durability uncertain; permissions/timestamps verified"),
            "{status}"
        );
        assert!(!status.contains("not fully verified"));
        let facts = ReceiptFacts::from_roots(&[verified]);
        let summary = terminal_summary(BatchStop::CommittedUncertain, 1, facts);
        assert!(summary.contains("Permissions/timestamps verified; durability is uncertain."));
        assert_eq!(facts.basic_verified, 1);
        assert_eq!(facts.incomplete, 0);
        assert_eq!(facts.uncertain, 1);
    }
    #[test]
    fn mixed_partial_presentation_keeps_published_content_and_never_promotes_incomplete_metadata_to_success()
     {
        let mut completed = presentation_root(
            MetadataCompletion::Achieved(MetadataPolicy::Basic),
            Durability::Synced,
        );
        if let RootOutcome::CommittedUncertain(receipt) = completed.outcome {
            completed.outcome = RootOutcome::Committed(receipt);
        }
        let (text, warning) = root_status(&completed);
        assert!(!warning);
        assert!(text.starts_with("Published; permissions/timestamps verified"));
        let roots = [
            completed,
            presentation_root(
                MetadataCompletion::Incomplete {
                    requested: MetadataPolicy::Basic,
                },
                Durability::Synced,
            ),
            RootReport {
                source: "/later".into(),
                destination: "/destination/later".into(),
                outcome: RootOutcome::NotStarted,
            },
        ];
        let facts = ReceiptFacts::from_roots(&roots);
        assert_eq!(facts.basic_verified, 1);
        assert_eq!(facts.incomplete, 1);
        // A synthetic inconsistent envelope must not override the receipt's incomplete fact.
        let summary = terminal_summary(BatchStop::Complete, 2, facts);
        assert!(summary.contains("2 items published"));
        assert!(summary.contains("not fully verified"));
        assert!(!summary.contains("Copy complete"));
        assert_eq!(root_status(&roots[2]), ("Not started".into(), false));
        let cancelled = terminal_summary(BatchStop::Cancelled, 2, facts);
        assert!(cancelled.contains("published; content remains"));
        assert!(!cancelled.contains("No files"));
    }
    #[test]
    fn hidden_readonly_root_paste_verifies_metadata_retains_receipt_and_refreshes_until_acknowledged()
     {
        use std::os::unix::fs::MetadataExt;
        let f = Fixture::new();
        let source = f.path("readonly");
        let empty = f.path("empty0400");
        let dest = f.path("destination");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir(&empty).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("nested/tool"), b"readonly payload").unwrap();
        fs::set_permissions(
            source.join("nested/tool"),
            fs::Permissions::from_mode(0o4751),
        )
        .unwrap();
        fs::set_permissions(source.join("nested"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o4555)).unwrap();
        fs::set_permissions(&empty, fs::Permissions::from_mode(0o400)).unwrap();
        let mut view = TransfersUi::new();
        assert!(view.start(vec![source, empty], dest.clone()));
        view.hide();
        wait(&mut view);
        assert!(!view.visible());
        assert!(view.busy());
        assert_eq!(view.terminal().unwrap().stop, BatchStop::Complete);
        assert_eq!(view.terminal_facts.basic_verified, 2);
        assert_eq!(view.terminal_facts.incomplete, 0);
        assert_eq!(view.terminal_facts.uncertain, 0);
        assert_eq!(view.take_refresh_destination(), Some(dest.clone()));
        assert_eq!(view.take_refresh_destination(), None);
        assert_eq!(
            fs::metadata(dest.join("readonly")).unwrap().mode() & 0o7777,
            0o555
        );
        assert_eq!(
            fs::metadata(dest.join("empty0400")).unwrap().mode() & 0o7777,
            0o400
        );
        assert_eq!(
            fs::metadata(dest.join("readonly/nested/tool"))
                .unwrap()
                .mode()
                & 0o7777,
            0o751
        );
        assert_eq!(
            fs::read(dest.join("readonly/nested/tool")).unwrap(),
            b"readonly payload"
        );
        assert!(view.summary().contains("permissions/timestamps verified"));
        let summary = view.terminal_message.clone();
        view.show();
        assert!(view.poll());
        assert!(!view.poll(), "cached receipt aggregation must settle");
        assert_eq!(view.terminal_message, summary);
        let proof = view.evidence();
        assert!(proof.contains("\"basic_metadata_verified_roots\":2"));
        assert!(proof.contains("\"metadata_incomplete_roots\":0"));
        assert!(view.acknowledge());
        assert!(!view.busy());
        assert!(view.terminal_message.is_none());
        assert_eq!(view.terminal_facts.basic_verified, 0);
    }
}
