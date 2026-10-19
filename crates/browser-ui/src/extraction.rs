//! App-owned ZIP extraction screen. Hidden jobs remain independently pollable.
use super::{
    Glyph, action, bytes, chrome, clicked, color, detail, glyph, ground, hairline, label, muted,
};
use browser_archive::{
    ArchiveBrowser, ErrorKind, ExtractionJobId, ExtractionJobOptions, ExtractionJobSnapshot,
    ExtractionJobState, ExtractionJobs, ExtractionOutcome, ExtractionPhase, ExtractionSelection,
    ExtractionTicket,
};
use mui::prelude::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_UI: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ControlScope {
    job: ExtractionJobId,
    epoch: u64,
}
#[derive(Clone, Copy)]
enum Command {
    Start,
    Cancel,
    Acknowledge,
}
pub(super) enum ExtractionAction {
    Back,
}

pub(super) struct ExtractionUi {
    jobs: Option<ExtractionJobs>,
    owner: u64,
    epoch: u64,
    active: Option<ExtractionJobId>,
    snapshot: Option<ExtractionJobSnapshot>,
    ticket: Option<ExtractionTicket>,
    native_destination: PathBuf,
    destination_text: String,
    destination_edited: bool,
    source: PathBuf,
    error: Option<String>,
    visible: bool,
    cancel_requested: bool,
    start_submitted: bool,
    focus_pending: bool,
    changed: bool,
    refresh_parent: Option<PathBuf>,
    window: Size,
}
impl ExtractionUi {
    /// No worker is spawned until an actual extraction action is requested.
    pub(super) fn new() -> Self {
        let owner = NEXT_UI
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .unwrap_or(0);
        Self {
            jobs: None,
            owner,
            epoch: 0,
            active: None,
            snapshot: None,
            ticket: None,
            native_destination: PathBuf::new(),
            destination_text: String::new(),
            destination_edited: false,
            source: PathBuf::new(),
            error: None,
            visible: false,
            cancel_requested: false,
            start_submitted: false,
            focus_pending: false,
            changed: false,
            refresh_parent: None,
            window: Size::new(320., 240.),
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.active.is_some()
    }
    pub(super) fn visible(&self) -> bool {
        self.visible
    }
    pub(super) fn show(&mut self) {
        self.visible = true;
        self.focus_pending = true;
        self.changed = true;
    }
    fn hide(&mut self) {
        self.visible = false;
        self.changed = true;
    }
    fn bump(&mut self) -> bool {
        if self.owner == 0 {
            self.error =
                Some("Extraction control identity is unavailable. Restart the application.".into());
            return false;
        }
        let Some(epoch) = self.epoch.checked_add(1) else {
            self.error = Some("Extraction controls are exhausted. Restart the application.".into());
            return false;
        };
        self.epoch = epoch;
        true
    }
    pub(super) fn prepare(
        &mut self,
        browser: &ArchiveBrowser,
        selection: ExtractionSelection,
        default_destination: PathBuf,
    ) -> bool {
        self.show();
        if self.busy() {
            return false;
        }
        if !self.bump() {
            return false;
        }
        self.source = browser.source().to_owned();
        self.destination_text = default_destination.to_string_lossy().into_owned();
        self.native_destination = default_destination;
        self.destination_edited = false;
        self.error = None;
        self.cancel_requested = false;
        self.start_submitted = false;
        if self.jobs.is_none() {
            match ExtractionJobs::new(ExtractionJobOptions::default()) {
                Ok(jobs) => self.jobs = Some(jobs),
                Err(error) => {
                    self.error = Some(format!("Could not start extraction worker: {error}"));
                    return false;
                }
            }
        }
        match self.jobs.as_ref().unwrap().prepare(browser, selection) {
            Ok(id) => {
                self.active = Some(id);
                self.snapshot = None;
                self.ticket = None;
                true
            }
            Err(error) => {
                self.error = Some(format!(
                    "Could not prepare extraction: {error}. Return to the archive and choose a complete, current selection."
                ));
                false
            }
        }
    }
    fn apply(&mut self, mut snapshot: ExtractionJobSnapshot) -> bool {
        if Some(snapshot.id) != self.active
            || snapshot.source.as_ref() != &self.source
            || self
                .snapshot
                .as_ref()
                .is_some_and(|old| snapshot.revision < old.revision)
        {
            return false;
        }
        let state_changed = self
            .snapshot
            .as_ref()
            .is_none_or(|old| old.state != snapshot.state);
        if let Some(ticket) = snapshot.ticket.take() {
            self.ticket = Some(ticket);
        }
        if snapshot.state == ExtractionJobState::Terminal {
            self.ticket = None;
            let published = match snapshot.outcome.as_ref() {
                Some(ExtractionOutcome::Committed { destination, .. }) => Some(destination),
                Some(ExtractionOutcome::Failed(error)) => error.published.as_ref(),
                None => None,
            };
            self.refresh_parent = published.and_then(|path| path.parent().map(PathBuf::from));
        }
        self.snapshot = Some(snapshot);
        if state_changed {
            self.bump();
            self.focus_pending = true;
        }
        true
    }
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.changed);
        if let Some(snapshot) = self.jobs.as_ref().and_then(ExtractionJobs::poll) {
            changed |= self.apply(snapshot);
        }
        changed
    }
    pub(super) fn take_refresh_parent(&mut self) -> Option<PathBuf> {
        self.refresh_parent.take()
    }
    fn scope(&self) -> Option<ControlScope> {
        self.active.map(|job| ControlScope {
            job,
            epoch: self.epoch,
        })
    }
    fn id(&self, role: &str) -> String {
        format!("extraction-{}-{}-{role}", self.owner, self.epoch)
    }
    fn destination(&self) -> Result<PathBuf, &'static str> {
        let path = if self.destination_edited {
            PathBuf::from(&self.destination_text)
        } else {
            self.native_destination.clone()
        };
        if self.destination_edited && self.destination_text.contains('\0') {
            return Err("The destination cannot contain NUL characters.");
        }
        if !path.is_absolute() {
            return Err("Enter an absolute path for a new destination folder.");
        }
        if path.as_os_str().len() > u16::MAX as usize || path.as_os_str().is_empty() {
            return Err("The destination path exceeds the supported length.");
        }
        Ok(path)
    }
    fn command(&mut self, scope: ControlScope, command: Command) -> bool {
        if self.scope() != Some(scope) {
            return false;
        }
        let Some(jobs) = &self.jobs else {
            return false;
        };
        match command {
            Command::Start => {
                if self.start_submitted
                    || self.cancel_requested
                    || self
                        .snapshot
                        .as_ref()
                        .is_none_or(|s| s.state != ExtractionJobState::Prepared)
                {
                    return false;
                }
                let destination = match self.destination() {
                    Ok(path) => path,
                    Err(message) => {
                        self.error = Some(message.into());
                        return false;
                    }
                };
                let Some(ticket) = self.ticket.take() else {
                    return false;
                };
                match jobs.start(ticket, destination, None) {
                    Ok(()) => {
                        self.start_submitted = true;
                        self.error = None;
                        self.bump();
                    }
                    Err(error) => {
                        self.ticket = Some(error.ticket);
                        self.error = Some(format!("Could not start extraction: {}", error.kind));
                    }
                }
            }
            Command::Cancel => {
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.state == ExtractionJobState::Terminal)
                {
                    return false;
                }
                match jobs.cancel(scope.job) {
                    Ok(()) => {
                        self.cancel_requested = true;
                        self.ticket = None;
                        self.error = None;
                        self.bump();
                    }
                    Err(error) => {
                        self.error = Some(format!("Could not request cancellation: {error}"))
                    }
                }
            }
            Command::Acknowledge => {
                if self
                    .snapshot
                    .as_ref()
                    .is_none_or(|s| s.state != ExtractionJobState::Terminal)
                {
                    return false;
                }
                match jobs.acknowledge(scope.job) {
                    Ok(()) => {
                        self.active = None;
                        self.snapshot = None;
                        self.ticket = None;
                        self.hide();
                        self.error = None;
                        self.bump();
                    }
                    Err(error) => {
                        self.error = Some(format!("Could not acknowledge extraction: {error}"))
                    }
                }
            }
        }
        self.changed = true;
        true
    }
    pub(super) fn status_label(&self) -> String {
        if self.cancel_requested
            && self
                .snapshot
                .as_ref()
                .is_none_or(|s| s.state != ExtractionJobState::Terminal)
        {
            return "Cancellation requested · waiting for worker".into();
        }
        match self.snapshot.as_ref().map(|s| s.state) {
            None if self.active.is_some() => "Preparing ZIP extraction…".into(),
            None => "ZIP extraction".into(),
            Some(ExtractionJobState::Preparing) => "Checking source and selection…".into(),
            Some(ExtractionJobState::Prepared) if self.start_submitted => {
                "Starting extraction…".into()
            }
            Some(ExtractionJobState::Prepared) => "Ready · choose a new destination folder".into(),
            Some(ExtractionJobState::Running(ExtractionPhase::Reading)) => {
                "Extracting and verifying…".into()
            }
            Some(ExtractionJobState::Running(ExtractionPhase::Syncing)) => {
                "Syncing verified output…".into()
            }
            Some(ExtractionJobState::Running(ExtractionPhase::Publishing)) => {
                "Publishing new folder…".into()
            }
            Some(ExtractionJobState::Terminal) => {
                match self.snapshot.as_ref().and_then(|s| s.outcome.as_ref()) {
                    Some(ExtractionOutcome::Committed { .. }) => "Extraction complete".into(),
                    Some(ExtractionOutcome::Failed(error)) if error.published.is_some() => {
                        "Folder published · completion failed".into()
                    }
                    Some(ExtractionOutcome::Failed(error))
                        if error.residue.is_some() || error.kind == ErrorKind::RecoveryRequired =>
                    {
                        "Extraction needs recovery review".into()
                    }
                    Some(ExtractionOutcome::Failed(error))
                        if error.kind == ErrorKind::Cancelled =>
                    {
                        "Extraction cancelled".into()
                    }
                    _ => "Extraction failed".into(),
                }
            }
        }
    }
    fn outcome_lines(&self) -> Vec<String> {
        let Some(outcome) = self.snapshot.as_ref().and_then(|s| s.outcome.as_ref()) else {
            return vec!["Files remain private until every selected record verifies. Existing destinations are never replaced.".into(), "Back hides this screen; the job remains available. Cancel is an explicit request and cannot undo publication.".into()];
        };
        match outcome {
            ExtractionOutcome::Committed {
                entries,
                bytes: count,
                ..
            } => vec![
                format!(
                    "{entries} {} verified · {} published.",
                    if *entries == 1 { "record" } else { "records" },
                    bytes(*count)
                ),
                "The new folder was committed. A later cancellation cannot undo it.".into(),
            ],
            ExtractionOutcome::Failed(error) => {
                let mut lines = Vec::new();
                if error.kind == ErrorKind::CredentialRequired {
                    lines.push("Password-protected entries need credentials. Password entry is unavailable in this view.".into());
                }
                if error.published.is_some() {
                    lines.push("The destination was published before a later failure. Inspect it; cancellation did not roll it back.".into());
                } else if error.kind == ErrorKind::RecoveryRequired {
                    lines.push("Publication or cleanup is uncertain. No path in this report is proof that output is absent. Inspect the destination before retrying.".into());
                } else {
                    lines.push("No destination publication was reported.".into());
                }
                if error.residue.is_some() {
                    lines.push("Private staging residue remains. Review it before removing anything or retrying.".into());
                }
                lines.push(error.message.clone());
                if error.message_truncated {
                    lines.push("The diagnostic was shortened for display.".into());
                }
                lines
            }
        }
    }
    pub(super) fn build_status(&mut self, ui: &Ui) -> Option<El> {
        if !self.busy() || self.visible {
            return None;
        }
        let id = self.id("show");
        if clicked(ui, &id) {
            self.show();
        }
        Some(
            row([
                detail(self.status_label()).grow(1).basis(0).min_w(0).clip(),
                action(
                    ui,
                    &id,
                    "Show ZIP extraction job",
                    label("Show extraction"),
                    true,
                    false,
                ),
            ])
            .gap(7)
            .pad((8, 3))
            .fill(chrome())
            .min_w(0)
            .shrink(0)
            .id("extraction-hidden-status"),
        )
    }
    pub(super) fn build(&mut self, ui: &mut Ui) -> (El, Option<ExtractionAction>) {
        self.poll();
        let back_id = self.id("back");
        let start_id = self.id("start");
        let cancel_id = self.id("cancel");
        let ack_id = self.id("ack");
        let destination_id = self.id("destination");
        // Apply this frame's editor input before consuming a Start action.
        let was_ready = self.ticket.is_some()
            && !self.start_submitted
            && !self.cancel_requested
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == ExtractionJobState::Prepared);
        let field = if was_ready {
            let before = self.destination_text.clone();
            let field = text_edit(
                ui,
                &destination_id,
                &mut self.destination_text,
                TextOpts::default(),
            )
            .el;
            self.destination_edited |= self.destination_text != before;
            field
        } else {
            col([label(self.destination_text.clone()).lines(1).clip()])
                .pad((8, 6))
                .fill(chrome())
                .id(&destination_id)
        };
        let scope = self.scope();
        let mut outcome = None;
        let escape = ui.shortcuts().iter().any(|k| k.key == Key::Escape)
            || (ui.focus_is_text()
                && ui
                    .keys(ui.focus_key().unwrap_or(""))
                    .iter()
                    .any(|k| k.key == Key::Escape));
        if clicked(ui, &back_id) || escape {
            self.hide();
            outcome = Some(ExtractionAction::Back);
        }
        if outcome.is_none()
            && let Some(scope) = scope
        {
            if clicked(ui, &start_id) {
                self.command(scope, Command::Start);
            } else if clicked(ui, &cancel_id) {
                self.command(scope, Command::Cancel);
            } else if clicked(ui, &ack_id) && self.command(scope, Command::Acknowledge) {
                outcome = Some(ExtractionAction::Back);
            }
        }
        let terminal = self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.state == ExtractionJobState::Terminal);
        let ready = self.ticket.is_some()
            && !self.start_submitted
            && !self.cancel_requested
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == ExtractionJobState::Prepared);
        if self.focus_pending {
            ui.focus(if ready {
                destination_id.clone()
            } else {
                back_id.clone()
            });
            self.focus_pending = false;
        }
        let header = row([
            action(
                ui,
                &back_id,
                "Back · keep extraction job",
                glyph(Glyph::Back, muted()),
                true,
                false,
            ),
            label("Extract ZIP").grow(1).basis(0).min_w(0),
            detail("New folder only").shrink(0),
        ])
        .gap(7)
        .pad((6, 4))
        .fill(chrome())
        .align(Align::Center)
        .shrink(0);
        let mut body = vec![
            label(self.status_label())
                .wrap()
                .min_w(0)
                .id("extraction-phase"),
        ];
        if let Some(snapshot) = &self.snapshot {
            body.push(
                detail(format!(
                    "{} {} · {} declared · {} decoded (unverified work)",
                    snapshot.records,
                    if snapshot.records == 1 {
                        "record"
                    } else {
                        "records"
                    },
                    bytes(snapshot.declared_bytes),
                    bytes(snapshot.decoded_bytes)
                ))
                .wrap()
                .min_w(0)
                .id("extraction-counts"),
            );
        }
        // Recovery/publication truth precedes source/destination details so it
        // remains visible in a small viewport; lower details scroll naturally.
        if terminal {
            for (index, line) in self.outcome_lines().into_iter().enumerate() {
                body.push(
                    detail(line)
                        .wrap()
                        .min_w(0)
                        .id(format!("extraction-outcome-{index}")),
                );
            }
            if let Some(outcome) = self.snapshot.as_ref().and_then(|s| s.outcome.as_ref()) {
                let paths = match outcome {
                    ExtractionOutcome::Committed { destination, .. } => {
                        vec![("Published", destination)]
                    }
                    ExtractionOutcome::Failed(error) => error
                        .published
                        .iter()
                        .map(|p| ("Published", p))
                        .chain(error.residue.iter().map(|p| ("Residue", p)))
                        .collect(),
                };
                for (role, path) in paths {
                    let display = path.to_string_lossy().into_owned();
                    body.push(detail(role));
                    body.push(label(display.clone()).lines(1).min_w(0).clip().tip(display));
                }
            }
        }
        if !terminal {
            body.push(detail("New absolute destination folder"));
            body.push(
                col([field
                    .w(pct(100))
                    .min_w(0)
                    .named("New absolute destination folder")
                    .when(was_ready && !ready, |e| e.disabled())])
                .w(pct(100))
                .min_w(0)
                .id("extraction-destination-box"),
            );
            if !self.destination_edited && self.native_destination.to_str().is_none() {
                body.push(
                    detail("Native filename bytes are preserved until you edit this field.")
                        .wrap()
                        .min_w(0),
                );
            }
            if ready && let Err(message) = self.destination() {
                body.push(
                    detail(message)
                        .fill(color(0xffb4ab))
                        .wrap()
                        .min_w(0)
                        .id("extraction-validation"),
                );
            }
            if let Some(error) = &self.error {
                body.push(
                    detail(error.clone())
                        .fill(color(0xffb4ab))
                        .wrap()
                        .min_w(0)
                        .id("extraction-error"),
                );
            }
            for line in self.outcome_lines() {
                body.push(detail(line).wrap().min_w(0));
            }
        }
        let source = self.source.to_string_lossy().into_owned();
        body.push(detail("Source"));
        body.push(
            label(source.clone())
                .lines(1)
                .min_w(0)
                .clip()
                .tip(source)
                .id("extraction-source"),
        );
        if !terminal {
            body.push(detail("Archive folder prefixes are preserved. Password entry is unavailable; encrypted selections cannot complete here.").wrap().min_w(0));
        }
        if terminal && let Some(error) = &self.error {
            body.push(
                detail(error.clone())
                    .fill(color(0xffb4ab))
                    .wrap()
                    .min_w(0)
                    .id("extraction-error"),
            );
        }
        let body = col(body.into_iter().map(|child| child.w(pct(100)).shrink(0)))
            .align(Align::Start)
            .gap(7)
            .pad(10)
            .min_w(0)
            .grow(1)
            .basis(0)
            .min_h(0)
            .scroll()
            .id("extraction-body");
        let footer = row([
            if terminal {
                action(
                    ui,
                    &ack_id,
                    "Acknowledge terminal extraction result",
                    label("Acknowledge"),
                    true,
                    false,
                )
            } else {
                action(
                    ui,
                    &start_id,
                    "Extract to the new folder",
                    label("Extract new folder"),
                    ready && self.destination().is_ok(),
                    false,
                )
            },
            spacer(),
            action(
                ui,
                &cancel_id,
                "Cancel this extraction job",
                label("Cancel job"),
                self.active.is_some() && !terminal && !self.cancel_requested,
                false,
            ),
        ])
        .gap(7)
        .pad((8, 4))
        .fill(chrome())
        .shrink(0)
        .min_w(0)
        .id("extraction-controls");
        (
            col([header, hairline(), body, hairline(), footer])
                .w(pct(100))
                .h(pct(100))
                .min_w(0)
                .min_h(0)
                .fill(ground())
                .clip()
                .id("extraction-screen")
                .named("ZIP extraction job"),
            outcome,
        )
    }
    pub(super) fn after_frame(&mut self, ui: &Ui) {
        if let Some(frame) = ui.scene().and_then(|s| s.surface("extraction-screen"))
            && self.window != frame.frame.size
        {
            self.window = frame.frame.size;
            self.changed = true;
        }
    }
    pub(super) fn evidence(&self) -> String {
        format!(
            "{{\"schema_version\":1,\"mode\":\"extraction\",\"busy\":{},\"visible\":{},\"job_sequence\":{},\"control_epoch\":{},\"state\":\"{:?}\",\"cancel_requested\":{},\"destination_edited\":{},\"window_width\":{},\"window_height\":{},\"password_entry\":false}}",
            self.busy(),
            self.visible,
            self.active.map_or(0, ExtractionJobId::sequence),
            self.epoch,
            self.snapshot.as_ref().map(|s| s.state),
            self.cancel_requested,
            self.destination_edited,
            self.window.width,
            self.window.height
        )
    }
}

/// Optional CPU evidence uses the real resolved scene; no production I/O.
#[cfg(test)]
pub(super) fn capture_evidence(ui: &Ui, size: Size, name: &str) {
    let Some(directory) = std::env::var_os("EXTRACTION_UI_EVIDENCE") else {
        return;
    };
    use mui::vello::vello_cpu::{Pixmap, RenderContext, Resources};
    let width = size.width as u16;
    let height = size.height as u16;
    let mut context = RenderContext::new(width, height);
    let mut resources = Resources::default();
    mui::vello::paint(
        &mut mui::vello::Cpu {
            ctx: &mut context,
            resources: &mut resources,
            cache: &mut mui::vello::Cache::default(),
        },
        ui.scene().unwrap(),
        mui::vello::kurbo::Affine::IDENTITY,
    )
    .unwrap();
    context.flush();
    let mut pixmap = Pixmap::new(width, height);
    context.render(&mut pixmap, &mut resources);
    let directory = PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{name}-{width}x{height}.rgba")),
        pixmap.take_rgba8(mui::vello::peniko::ImageAlphaType::Alpha),
    )
    .unwrap();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use browser_archive::{
        BrowserCatalogState, BrowserOptions, Cancellation, Control, CreateOptions, CreateSource,
        ExtractionFailure, Limits, create_new,
    };
    use mui::host::View;
    use std::{
        fs,
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/extraction-generated")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn zip(&self) -> PathBuf {
            let source = self.0.join("payload");
            fs::write(&source, b"verified payload").unwrap();
            let path = self.0.join("source.zip");
            let cancel = Cancellation::default();
            create_new(
                &[CreateSource::file(source, "folder/member")],
                &path,
                &CreateOptions {
                    deflate_level: None,
                    aes256: false,
                },
                None,
                Limits::default(),
                &Control::new(&cancel, Duration::from_secs(10), 1 << 20),
            )
            .unwrap();
            path
        }
        fn browser(&self) -> ArchiveBrowser {
            let browser = ArchiveBrowser::open(BrowserOptions::new(self.zip())).unwrap();
            wait(|| {
                browser
                    .poll()
                    .filter(|p| p.catalog.state == BrowserCatalogState::Complete)
            });
            browser
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn wait<T>(mut check: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = check() {
                return result;
            }
            assert!(Instant::now() < until, "UI worker timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn ready(panel: &mut ExtractionUi) {
        wait(|| {
            panel.poll();
            assert!(
                panel
                    .snapshot
                    .as_ref()
                    .is_none_or(|s| s.state != ExtractionJobState::Terminal),
                "unexpected failure {:?}",
                panel.error
            );
            panel.ticket.is_some().then_some(())
        });
    }
    fn terminal(panel: &mut ExtractionUi) {
        wait(|| {
            panel.poll();
            panel
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == ExtractionJobState::Terminal)
                .then_some(())
        });
    }
    fn frame(
        panel: &mut ExtractionUi,
        ui: &mut Ui,
        size: Size,
        input: Input,
    ) -> Option<ExtractionAction> {
        let (tree, action) = panel.build(ui);
        ui.frame(tree, Some(size), input, 0.016).unwrap();
        panel.after_frame(ui);
        action
    }
    fn click(
        panel: &mut ExtractionUi,
        ui: &mut Ui,
        size: Size,
        id: &str,
    ) -> Option<ExtractionAction> {
        let rect = ui.scene().unwrap().surface(id).unwrap().frame;
        let point = Point::new(
            rect.x + rect.size.width / 2.,
            rect.y + rect.size.height / 2.,
        );
        for down in [false, true, false] {
            frame(
                panel,
                ui,
                size,
                Input::from(PointerInput {
                    pos: Some(point),
                    buttons: Buttons::default().set(Button::Primary, down),
                    ..PointerInput::default()
                }),
            );
        }
        frame(panel, ui, size, Input::default())
    }
    fn synthetic_failure(
        panel: &mut ExtractionUi,
        kind: ErrorKind,
        published: Option<PathBuf>,
        residue: Option<PathBuf>,
    ) {
        let id = panel.active.unwrap();
        // Presentation-only fixtures use a genuine accepted opaque ID. The engine
        // separately qualifies native failure transport with transaction faults.
        panel.apply(ExtractionJobSnapshot {
            id,
            revision: u64::MAX,
            state: ExtractionJobState::Terminal,
            source: Arc::new(panel.source.clone()),
            records: 1,
            declared_bytes: 16,
            decoded_bytes: 7,
            verified_records: 0,
            ticket: None,
            outcome: Some(ExtractionOutcome::Failed(ExtractionFailure {
                kind,
                io_kind: None,
                raw_os_error: None,
                message: "Fixture failure after partial work".into(),
                message_truncated: false,
                published,
                residue,
            })),
        });
    }

    #[test]
    fn extraction_and_paste_share_admission_through_terminal_acknowledgement() {
        use crate::{BrowserView, archive::ArchivePanel, clipboard::ClipboardUi};
        let fixture = Fixture::new();
        let browser = fixture.browser();
        let source = fixture.0.join("payload");
        let destination = fixture.0.join("copied");
        fs::create_dir(&destination).unwrap();
        let mut extraction = ExtractionUi::new();
        assert!(extraction.prepare(
            &browser,
            ExtractionSelection::All,
            fixture.0.join("extracted")
        ));
        ready(&mut extraction);
        extraction.hide();
        let mut view = BrowserView::fixture(0);
        view.extraction = Some(extraction);
        view.copy_transfers = Some(crate::transfers::TransfersUi::new());
        view.paste_files();
        assert!(
            !view.copy_transfers.as_ref().unwrap().visible(),
            "an idle prior copy must not hide the blocking extraction"
        );
        view.copy_transfers = None;
        for terminal_state in [false, true] {
            if terminal_state {
                let panel = view.extraction.as_mut().unwrap();
                assert!(panel.command(panel.scope().unwrap(), Command::Cancel));
                terminal(panel);
            }
            view.file_clipboard = Some(ClipboardUi::fixture_offer(
                vec![source.clone()],
                destination.clone(),
            ));
            assert!(view.mutations_busy());
            view.admit_paste_offer();
            assert!(
                view.copy_transfers.is_none(),
                "extraction owns admission even when hidden or terminal"
            );
            assert!(!view.file_clipboard.as_ref().unwrap().paste_pending());
            assert!(!destination.join("payload").exists());
        }
        let panel = view.extraction.as_mut().unwrap();
        assert!(panel.command(panel.scope().unwrap(), Command::Acknowledge));
        assert!(!view.mutations_busy());
        view.file_clipboard = Some(ClipboardUi::fixture_offer(
            vec![source],
            destination.clone(),
        ));
        assert!(
            view.mutations_busy(),
            "a ready native offer itself reserves admission"
        );
        view.copy_transfers = Some(crate::transfers::TransfersUi::new());
        view.paste_files();
        assert!(
            !view.copy_transfers.as_ref().unwrap().visible(),
            "an idle prior copy must not hide a pending native offer"
        );
        view.admit_paste_offer();
        assert!(view.copy_transfers.is_some());
        assert!(
            view.file_clipboard.as_ref().unwrap().message().is_none(),
            "accepted jobs must not duplicate the retained transfer status"
        );
        wait(|| {
            view.poll_copy_transfers();
            view.copy_transfers
                .as_ref()
                .unwrap()
                .summary()
                .contains("1 item published")
                .then_some(())
        });
        assert_eq!(
            fs::read(destination.join("payload")).unwrap(),
            b"verified payload"
        );
        assert!(
            view.mutations_busy(),
            "copy results retain admission until explicitly acknowledged"
        );
        view.copy_transfers.as_mut().unwrap().hide();
        view.archive = Some(ArchivePanel::new(browser.source().to_owned()));
        let mut ui = crate::make_ui().unwrap();
        let size = Size::new(640., 480.);
        let step = |view: &mut BrowserView, ui: &mut Ui, input: Input| {
            let tree = view.build(ui, &input);
            ui.frame(tree, Some(size), input, 0.016).unwrap();
            view.after_frame(ui);
        };
        for _ in 0..6 {
            step(&mut view, &mut ui, Input::default());
        }
        let transfers = view.copy_transfers.as_ref().unwrap();
        let show_id = transfers.action_id("show");
        assert!(
            ui.scene().unwrap().surface(&show_id).is_some(),
            "copy receipts remain reachable while browsing ZIP"
        );
        assert!(!view.extraction.as_ref().unwrap().busy());
        assert!(!fixture.0.join("extracted").exists());
        // A visible result screen must retain its own acknowledgement and not
        // accidentally dispatch that click into a newly exposed archive action.
        view.copy_transfers.as_mut().unwrap().show();
        for _ in 0..4 {
            step(&mut view, &mut ui, Input::default());
        }
        let id = view
            .copy_transfers
            .as_ref()
            .unwrap()
            .action_id("acknowledge");
        let rect = ui.scene().unwrap().surface(&id).unwrap().frame;
        let point = Point::new(
            rect.x + rect.size.width / 2.,
            rect.y + rect.size.height / 2.,
        );
        for down in [false, true, false] {
            step(
                &mut view,
                &mut ui,
                Input::from(PointerInput {
                    pos: Some(point),
                    buttons: Buttons::default().set(Button::Primary, down),
                    ..PointerInput::default()
                }),
            );
        }
        step(&mut view, &mut ui, Input::default());
        assert!(!view.mutations_busy());
        assert!(view.archive.is_some());
        assert!(!view.extraction.as_ref().unwrap().busy());
    }

    #[test]
    fn hidden_publication_refreshes_only_the_matching_current_host_folder_once() {
        use crate::BrowserView;
        let fixture = Fixture::new();
        let browser = fixture.browser();
        let other = fixture.0.join("other");
        fs::create_dir(&other).unwrap();
        let mut view = BrowserView::new(fixture.0.clone());
        wait(|| {
            view.refresh();
            matches!(
                view.browser.as_ref().unwrap().state(),
                crate::LoadState::Complete
            )
            .then_some(())
        });
        assert!(
            !view
                .browser
                .as_ref()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| entry.name == "extracted")
        );
        let mut panel = ExtractionUi::new();
        assert!(panel.prepare(
            &browser,
            ExtractionSelection::All,
            fixture.0.join("extracted")
        ));
        ready(&mut panel);
        assert!(panel.command(panel.scope().unwrap(), Command::Start));
        panel.hide();
        view.extraction = Some(panel);
        wait(|| {
            view.changed();
            view.browser
                .as_ref()
                .unwrap()
                .entries()
                .iter()
                .any(|entry| entry.name == "extracted")
                .then_some(())
        });
        assert_eq!(
            fs::read(fixture.0.join("extracted/folder/member")).unwrap(),
            b"verified payload"
        );
        assert!(
            view.extraction
                .as_mut()
                .unwrap()
                .take_refresh_parent()
                .is_none()
        );
        assert!(!view.extraction.as_ref().unwrap().visible());
        let panel = view.extraction.as_mut().unwrap();
        assert!(panel.command(panel.scope().unwrap(), Command::Acknowledge));
        // Acknowledgement is queued to the retained worker; wait for its
        // admission slot rather than assuming synchronous reclamation.
        wait(|| {
            panel
                .prepare(&browser, ExtractionSelection::All, fixture.0.join("second"))
                .then_some(())
        });
        ready(panel);
        view.navigate(other.clone());
        wait(|| {
            view.refresh();
            matches!(
                view.browser.as_ref().unwrap().state(),
                crate::LoadState::Complete
            )
            .then_some(())
        });
        let serial = view.listing_serial;
        let panel = view.extraction.as_mut().unwrap();
        assert!(panel.command(panel.scope().unwrap(), Command::Start));
        panel.hide();
        wait(|| {
            view.changed();
            view.extraction
                .as_ref()
                .unwrap()
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.state == ExtractionJobState::Terminal)
                .then_some(())
        });
        assert_eq!(view.browser.as_ref().unwrap().path(), other);
        assert_eq!(
            view.listing_serial, serial,
            "publication elsewhere must not reset the current listing or selection"
        );
        assert!(view.browser.as_ref().unwrap().entries().is_empty());
        assert!(
            view.extraction
                .as_mut()
                .unwrap()
                .take_refresh_parent()
                .is_none()
        );
        assert_eq!(
            fs::read(fixture.0.join("second/folder/member")).unwrap(),
            b"verified payload"
        );
    }

    #[test]
    fn synthetic_fixture_and_idle_controller_spawn_no_extraction_worker() {
        let view = super::super::BrowserView::fixture(5);
        assert!(view.extraction.is_none());
        assert!(!view.mutations_busy());
        let panel = ExtractionUi::new();
        assert!(panel.jobs.is_none());
        assert!(!panel.busy());
    }

    #[test]
    fn accepted_preparation_survives_panel_close_and_hidden_execution_remains_pollable() {
        let f = Fixture::new();
        let source = f.zip();
        let mut archive = super::super::archive::ArchivePanel::new(source);
        let mut extraction = ExtractionUi::new();
        wait(|| {
            archive.poll();
            archive
                .prepare_extraction(
                    &mut extraction,
                    ExtractionSelection::All,
                    f.0.join("new-folder"),
                )
                .then_some(())
        });
        drop(archive);
        ready(&mut extraction);
        let scope = extraction.scope().unwrap();
        assert!(extraction.command(scope, Command::Start));
        extraction.visible = false;
        let mut view = super::super::BrowserView::fixture(0);
        view.extraction = Some(extraction);
        wait(|| {
            view.changed();
            view.extraction
                .as_ref()
                .unwrap()
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == ExtractionJobState::Terminal)
                .then_some(())
        });
        assert!(
            view.mutations_busy(),
            "terminal results still require acknowledgement"
        );
        assert_eq!(
            fs::read(f.0.join("new-folder/folder/member")).unwrap(),
            b"verified payload"
        );
        let mut ui = super::super::make_ui().unwrap();
        for _ in 0..3 {
            let tree = view.build(&mut ui, &Input::default());
            ui.frame(tree, Some(Size::new(320., 240.)), Input::default(), 0.016)
                .unwrap();
            view.after_frame(&mut ui);
        }
        assert!(
            ui.scene()
                .unwrap()
                .surface("extraction-hidden-status")
                .is_some()
        );
        let controller = view.extraction.as_mut().unwrap();
        controller.show();
        let scope = controller.scope().unwrap();
        assert!(controller.command(scope, Command::Acknowledge));
        assert!(!view.mutations_busy());
    }

    #[test]
    fn stale_epoch_and_foreign_job_controls_cannot_start_cancel_or_acknowledge() {
        let f = Fixture::new();
        let browser = f.browser();
        let mut first = ExtractionUi::new();
        let mut other = ExtractionUi::new();
        assert!(first.prepare(&browser, ExtractionSelection::All, f.0.join("first")));
        ready(&mut first);
        assert!(other.prepare(&browser, ExtractionSelection::All, f.0.join("other")));
        ready(&mut other);
        let foreign = first.scope().unwrap();
        assert!(!other.command(foreign, Command::Start));
        assert!(!other.command(foreign, Command::Cancel));
        assert!(!other.command(foreign, Command::Acknowledge));
        let old = other.scope().unwrap();
        assert!(other.command(old, Command::Cancel));
        terminal(&mut other);
        assert!(!other.command(old, Command::Acknowledge));
        assert!(other.busy());
        assert!(other.command(other.scope().unwrap(), Command::Acknowledge));
        wait(|| {
            other
                .prepare(&browser, ExtractionSelection::All, f.0.join("other-new"))
                .then_some(())
        });
        ready(&mut other);
        assert!(!other.command(old, Command::Cancel));
        assert!(!other.command(old, Command::Start));
        assert!(other.ticket.is_some());
        assert!(!other.cancel_requested);
        assert!(first.command(first.scope().unwrap(), Command::Start));
        terminal(&mut first);
        assert!(matches!(
            first.snapshot.as_ref().unwrap().outcome,
            Some(ExtractionOutcome::Committed { .. })
        ));
        assert!(!first.command(first.scope().unwrap(), Command::Cancel));
        assert!(!f.0.join("other").exists() && !f.0.join("other-new").exists());
    }

    #[test]
    fn unchanged_non_utf8_destination_uses_native_identity_and_an_edit_is_explicit() {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let browser = f.browser();
        let mut panel = ExtractionUi::new();
        let native = f.0.join(std::ffi::OsString::from_vec(b"new-\xff".to_vec()));
        assert!(panel.prepare(&browser, ExtractionSelection::All, native.clone()));
        ready(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        for _ in 0..3 {
            frame(&mut panel, &mut ui, Size::new(320., 240.), Input::default());
        }
        assert!(!panel.destination_edited);
        assert_eq!(panel.destination().unwrap(), native);
        assert!(panel.command(panel.scope().unwrap(), Command::Start));
        terminal(&mut panel);
        assert_eq!(
            fs::read(native.join("folder/member")).unwrap(),
            b"verified payload"
        );
        panel.destination_text = f.0.join("edited").to_str().unwrap().into();
        panel.destination_edited = true;
        assert_eq!(panel.destination().unwrap(), f.0.join("edited"));
        panel.destination_text = "relative".into();
        assert!(panel.destination().is_err());
    }

    #[test]
    fn back_hides_without_cancelling_and_real_pointer_start_keeps_native_destination() {
        let f = Fixture::new();
        let browser = f.browser();
        let mut panel = ExtractionUi::new();
        assert!(panel.prepare(&browser, ExtractionSelection::All, f.0.join("out")));
        ready(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(320., 240.);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        let back = panel.id("back");
        assert!(matches!(
            click(&mut panel, &mut ui, size, &back),
            Some(ExtractionAction::Back)
        ));
        assert!(!panel.visible());
        assert!(panel.busy());
        assert!(!panel.cancel_requested);
        assert!(panel.ticket.is_some());
        panel.show();
        for _ in 0..2 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        let start = panel.id("start");
        click(&mut panel, &mut ui, size, &start);
        terminal(&mut panel);
        assert_eq!(panel.status_label(), "Extraction complete");
        assert_eq!(
            fs::read(f.0.join("out/folder/member")).unwrap(),
            b"verified payload"
        );
        assert!(
            !panel.evidence().contains("verified payload") && !panel.evidence().contains("out/")
        );
    }

    #[test]
    fn partial_residue_and_uncertain_publication_language_is_distinct_and_intrinsic() {
        let f = Fixture::new();
        let browser = f.browser();
        for (kind, published, residue, expected) in [
            (ErrorKind::Cancelled, None, None, "Extraction cancelled"),
            (
                ErrorKind::Io,
                Some(f.0.join("published")),
                None,
                "Folder published · completion failed",
            ),
            (
                ErrorKind::Io,
                None,
                Some(f.0.join("residue")),
                "Extraction needs recovery review",
            ),
            (
                ErrorKind::RecoveryRequired,
                None,
                None,
                "Extraction needs recovery review",
            ),
            (
                ErrorKind::CredentialRequired,
                None,
                None,
                "Extraction failed",
            ),
        ] {
            let mut panel = ExtractionUi::new();
            assert!(panel.prepare(&browser, ExtractionSelection::All, f.0.join("out")));
            ready(&mut panel);
            synthetic_failure(&mut panel, kind, published.clone(), residue.clone());
            assert_eq!(panel.status_label(), expected);
            let lines = panel.outcome_lines().join(" ");
            if published.is_some() {
                assert!(lines.contains("was published") && lines.contains("did not roll it back"));
            }
            if residue.is_some() {
                assert!(lines.contains("residue remains"));
            }
            if kind == ErrorKind::RecoveryRequired {
                assert!(
                    lines.contains("uncertain")
                        && !lines.contains("No destination publication was reported")
                );
            }
            if kind == ErrorKind::CredentialRequired {
                assert!(lines.contains("Password entry is unavailable"));
            }
            let mut ui = super::super::make_ui().unwrap();
            for size in [Size::new(640., 480.), Size::new(320., 240.)] {
                for _ in 0..3 {
                    frame(&mut panel, &mut ui, size, Input::default());
                }
                let scene = ui.scene().unwrap();
                let counts = scene.surface("extraction-counts").unwrap().frame;
                let warning = scene.surface("extraction-outcome-0").unwrap().frame;
                assert!(
                    counts.bottom() <= warning.y + 0.1,
                    "wrapped text overlapped: {counts:?} / {warning:?}"
                );
                if let Some(diagnostic) = scene.surface("extraction-outcome-1") {
                    assert!(
                        warning.bottom() <= diagnostic.frame.y + 0.1,
                        "recovery warning overlapped its diagnostic"
                    );
                }
                let baselines = |key: &str| {
                    scene
                        .paint
                        .iter()
                        .filter(|paint| paint.key.as_str() == key)
                        .filter_map(|paint| paint.text.as_ref().map(|text| text.origin.y))
                        .collect::<Vec<_>>()
                };
                let count_lines = baselines("extraction-counts");
                let warning_lines = baselines("extraction-outcome-0");
                if let (Some(last), Some(first)) = (count_lines.last(), warning_lines.first()) {
                    assert!(
                        *first - *last >= 12.,
                        "painted text overlaps despite disjoint frames: {count_lines:?} / {warning_lines:?}"
                    );
                }
                let diagnostic = baselines("extraction-outcome-1");
                if let (Some(last), Some(first)) = (warning_lines.last(), diagnostic.first()) {
                    assert!(
                        *first - *last >= 12.,
                        "painted warning overlaps diagnostic: {warning_lines:?} / {diagnostic:?}"
                    );
                }
                if kind == ErrorKind::RecoveryRequired || published.is_some() {
                    capture_evidence(
                        &ui,
                        size,
                        if published.is_some() {
                            "published-failure"
                        } else {
                            "uncertain"
                        },
                    );
                }
                for id in [
                    "extraction-screen",
                    "extraction-body",
                    "extraction-controls",
                ] {
                    let rect = ui.scene().unwrap().surface(id).unwrap().frame;
                    assert!(
                        rect.x >= -0.1
                            && rect.y >= -0.1
                            && rect.right() <= size.width + 0.1
                            && rect.bottom() <= size.height + 0.1,
                        "{id}: {rect:?} in {size:?}"
                    );
                }
                assert!(ui.scene().unwrap().surface(&panel.id("ack")).is_some());
                assert!(
                    ui.scene()
                        .unwrap()
                        .surface("extraction-body")
                        .unwrap()
                        .frame
                        .size
                        .height
                        > 0.
                );
            }
        }
    }
    #[test]
    fn late_cancel_does_not_relabel_a_committed_job_and_preparation_rejection_is_visible() {
        let f = Fixture::new();
        let browser = f.browser();
        let mut panel = ExtractionUi::new();
        assert!(panel.prepare(&browser, ExtractionSelection::All, f.0.join("out")));
        ready(&mut panel);
        assert!(panel.command(panel.scope().unwrap(), Command::Start));
        let committed = wait(|| {
            panel
                .jobs
                .as_ref()
                .and_then(ExtractionJobs::poll)
                .filter(|s| s.state == ExtractionJobState::Terminal)
        });
        assert!(panel.command(panel.scope().unwrap(), Command::Cancel));
        assert!(panel.cancel_requested);
        panel.apply(committed);
        assert_eq!(panel.status_label(), "Extraction complete");
        assert!(panel.outcome_lines().join(" ").contains("cannot undo"));
        browser.cancel();
        let mut rejected = ExtractionUi::new();
        assert!(!rejected.prepare(&browser, ExtractionSelection::All, f.0.join("rejected")));
        assert!(rejected.visible() && !rejected.busy());
        assert!(rejected.error.as_ref().unwrap().contains("Closed"));
        assert!(!f.0.join("rejected").exists());
    }
    #[test]
    fn destination_editor_input_is_applied_before_start_in_the_same_batch() {
        let f = Fixture::new();
        let browser = f.browser();
        let mut panel = ExtractionUi::new();
        let original = f.0.join("original");
        let edited = f.0.join("edited");
        assert!(panel.prepare(&browser, ExtractionSelection::All, original.clone()));
        ready(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(640., 480.);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        let start = panel.id("start");
        let field = panel.id("destination");
        let rect = ui.scene().unwrap().surface(&start).unwrap().frame;
        let point = Point::new(
            rect.x + rect.size.width / 2.,
            rect.y + rect.size.height / 2.,
        );
        for down in [false, true] {
            frame(
                &mut panel,
                &mut ui,
                size,
                Input::from(PointerInput {
                    pos: Some(point),
                    buttons: Buttons::default().set(Button::Primary, down),
                    ..PointerInput::default()
                }),
            );
        }
        // A native final composition can arrive with the release that clicks Start.
        ui.focus(&field);
        let original_bytes = panel.destination_text.len();
        frame(
            &mut panel,
            &mut ui,
            size,
            Input {
                pointer: PointerInput {
                    pos: Some(point),
                    ..PointerInput::default()
                },
                ime: vec![
                    Ime::Selection(0..original_bytes),
                    Ime::Commit(edited.to_str().unwrap().into()),
                ],
                ..Input::default()
            },
        );
        frame(&mut panel, &mut ui, size, Input::default());
        terminal(&mut panel);
        assert!(panel.destination_edited);
        assert_eq!(
            fs::read(edited.join("folder/member")).unwrap(),
            b"verified payload"
        );
        assert!(
            !original.exists(),
            "Start used the path from before the editor input"
        );
    }
    #[test]
    fn prepared_screen_and_fixture_progress_fit_both_viewports() {
        let f = Fixture::new();
        let browser = f.browser();
        let mut panel = ExtractionUi::new();
        assert!(panel.prepare(&browser, ExtractionSelection::All, f.0.join("out")));
        ready(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        for size in [Size::new(640., 480.), Size::new(320., 240.)] {
            for _ in 0..3 {
                frame(&mut panel, &mut ui, size, Input::default());
            }
            for id in [
                "extraction-screen".into(),
                "extraction-body".into(),
                "extraction-controls".into(),
                "extraction-destination-box".into(),
                panel.id("start"),
                panel.id("cancel"),
                panel.id("back"),
            ] {
                let rect = ui.scene().unwrap().surface(&id).unwrap().frame;
                assert!(
                    rect.x >= -0.1
                        && rect.y >= -0.1
                        && rect.right() <= size.width + 0.1
                        && rect.bottom() <= size.height + 0.1,
                    "{id}: {rect:?}"
                );
            }
            capture_evidence(&ui, size, "prepared");
        }
        // A labeled presentation fixture, not throughput or native-host evidence.
        let mut snapshot = panel.snapshot.take().unwrap();
        snapshot.revision = u64::MAX;
        snapshot.state = ExtractionJobState::Running(ExtractionPhase::Reading);
        snapshot.decoded_bytes = 7;
        panel.start_submitted = true;
        panel.apply(snapshot);
        for size in [Size::new(640., 480.), Size::new(320., 240.)] {
            for _ in 0..3 {
                frame(&mut panel, &mut ui, size, Input::default());
            }
            capture_evidence(&ui, size, "fixture-progress");
            assert_eq!(panel.status_label(), "Extracting and verifying…");
        }
    }
    #[test]
    fn idle_back_invalidates_isolated_frame_and_paints_archive_without_cancelling() {
        let f = Fixture::new();
        let mut view = super::super::BrowserView::fixture(0);
        view.archive = Some(super::super::archive::ArchivePanel::new(f.zip()));
        let mut controller = ExtractionUi::new();
        wait(|| {
            let archive = view.archive.as_mut().unwrap();
            archive.poll();
            archive
                .prepare_extraction(&mut controller, ExtractionSelection::All, f.0.join("out"))
                .then_some(())
        });
        ready(&mut controller);
        view.extraction = Some(controller);
        let mut ui = super::super::make_ui().unwrap();
        let step = |view: &mut super::super::BrowserView, ui: &mut Ui, input: Input| {
            let tree = view.build(ui, &input);
            ui.frame(tree, Some(Size::new(640., 480.)), input, 0.016)
                .unwrap();
            view.after_frame(ui);
        };
        // Both states can remain idle indefinitely without a worker update.
        for terminal_state in [false, true] {
            if terminal_state {
                let panel = view.extraction.as_mut().unwrap();
                assert!(panel.command(panel.scope().unwrap(), Command::Cancel));
                terminal(panel);
                panel.show();
            }
            for _ in 0..8 {
                step(&mut view, &mut ui, Input::default());
                view.changed();
            }
            assert!(!view.changed(), "fixture must be idle before Back");
            let panel = view.extraction.as_ref().unwrap();
            let job = panel.active;
            let id = panel.id("back");
            let rect = ui.scene().unwrap().surface(&id).unwrap().frame;
            let point = Point::new(
                rect.x + rect.size.width / 2.,
                rect.y + rect.size.height / 2.,
            );
            for down in [false, true, false] {
                step(
                    &mut view,
                    &mut ui,
                    Input::from(PointerInput {
                        pos: Some(point),
                        buttons: Buttons::default().set(Button::Primary, down),
                        ..PointerInput::default()
                    }),
                );
            }
            assert!(ui.get(&id).clicked);
            // Consume Back in its isolated event frame, as the host does.
            step(&mut view, &mut ui, Input::default());
            assert!(ui.scene().unwrap().surface("extraction-screen").is_some());
            assert!(!view.extraction.as_ref().unwrap().visible());
            assert!(view.changed(), "Back must schedule the replacement frame");
            assert!(
                !view.geometry_changed,
                "Archive early return must consume geometry dirtiness"
            );
            step(&mut view, &mut ui, Input::default());
            assert!(ui.scene().unwrap().surface("extraction-screen").is_none());
            assert!(ui.scene().unwrap().surface("archive-panel").is_some());
            let panel = view.extraction.as_ref().unwrap();
            assert_eq!(panel.active, job);
            assert!(panel.busy());
            assert_eq!(panel.cancel_requested, terminal_state);
            assert!(!f.0.join("out").exists());
        }
        // Navigation invalidation must also survive the Find early return.
        view.find = Some(super::super::find::FindPanel::new(f.0.clone()));
        view.geometry_changed = true;
        assert!(view.changed());
        assert!(!view.geometry_changed);
    }

    #[test]
    fn escape_from_extraction_does_not_also_close_archive_and_hidden_show_reopens_job() {
        let f = Fixture::new();
        let source = f.zip();
        let mut view = super::super::BrowserView::new(f.0.clone());
        view.archive = Some(super::super::archive::ArchivePanel::new(source));
        let mut controller = ExtractionUi::new();
        wait(|| {
            let archive = view.archive.as_mut().unwrap();
            archive.poll();
            archive
                .prepare_extraction(&mut controller, ExtractionSelection::All, f.0.join("out"))
                .then_some(())
        });
        ready(&mut controller);
        view.extraction = Some(controller);
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(320., 240.);
        let step = |view: &mut super::super::BrowserView, ui: &mut Ui, input: Input| {
            let tree = view.build(ui, &input);
            ui.frame(tree, Some(size), input, 0.016).unwrap();
            view.after_frame(ui);
        };
        for _ in 0..3 {
            step(&mut view, &mut ui, Input::default());
        }
        step(
            &mut view,
            &mut ui,
            Input {
                keys: vec![KeyPress {
                    key: Key::Escape,
                    mods: Mods::default(),
                }],
                ..Input::default()
            },
        );
        for _ in 0..3 {
            step(&mut view, &mut ui, Input::default());
        }
        assert!(
            view.archive.is_some(),
            "Escape cascaded into closing the archive"
        );
        assert!(!view.extraction.as_ref().unwrap().visible());
        assert!(view.mutations_busy());
        let id = view.extraction.as_ref().unwrap().id("show");
        let rect = ui.scene().unwrap().surface(&id).unwrap().frame;
        let point = Point::new(
            rect.x + rect.size.width / 2.,
            rect.y + rect.size.height / 2.,
        );
        for down in [false, true, false] {
            step(
                &mut view,
                &mut ui,
                Input::from(PointerInput {
                    pos: Some(point),
                    buttons: Buttons::default().set(Button::Primary, down),
                    ..PointerInput::default()
                }),
            );
        }
        for _ in 0..3 {
            step(&mut view, &mut ui, Input::default());
        }
        assert!(view.extraction.as_ref().unwrap().visible());
        assert!(ui.scene().unwrap().surface("extraction-screen").is_some());
        assert!(view.mutations_busy());
        assert!(!view.extraction.as_ref().unwrap().cancel_requested);
    }
}
