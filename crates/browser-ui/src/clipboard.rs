//! Native file clipboard UI. Selection paths are materialized on the listing worker.
use browser_core::{
    listing::{ListingSnapshot, Materialization, MaterializeLimits, TicketError, TicketLimits},
    selection::Selection,
};
use browser_native::x11::{ClipboardService, Config, Outcome, Ownership, Request, Startup};
use browser_transfer::{CopyIntent, FileBatch};
use std::path::PathBuf;

pub(super) struct PasteOffer {
    pub destination: PathBuf,
    pub paths: Vec<PathBuf>,
}

enum Pending {
    Materializing {
        work: Materialization,
        count: usize,
    },
    Publishing {
        request: Request,
        count: usize,
    },
    Reading {
        request: Request,
        destination: PathBuf,
        cancelled: bool,
    },
}

pub(super) struct ClipboardUi {
    service: Option<ClipboardService>,
    startup: Startup,
    ownership: Ownership,
    pending: Option<Pending>,
    message: Option<String>,
    failed: bool,
    copied_count: usize,
    ui_epoch: u64,
    paste_offer: Option<PasteOffer>,
}

impl ClipboardUi {
    pub(super) fn new() -> Self {
        let (service, startup) = match ClipboardService::open(Config::default()) {
            Ok(service) => (Some(service), Startup::Connecting),
            Err(error) => (
                None,
                Startup::Failed(browser_native::x11::Error::Native(
                    error.to_string().chars().take(192).collect(),
                )),
            ),
        };
        Self {
            service,
            startup,
            ownership: Ownership::default(),
            pending: None,
            message: None,
            failed: false,
            copied_count: 0,
            ui_epoch: 0,
            paste_offer: None,
        }
    }

    #[cfg(test)]
    pub(super) fn fixture_offer(paths: Vec<PathBuf>, destination: PathBuf) -> Self {
        Self {
            service: None,
            startup: Startup::Ready { window: 1 },
            ownership: Ownership::default(),
            pending: None,
            message: None,
            failed: false,
            copied_count: 0,
            ui_epoch: 1,
            paste_offer: Some(PasteOffer { paths, destination }),
        }
    }

    pub(super) fn action_id(&self, action: &str) -> String {
        format!("file-clipboard/{}/{action}", self.ui_epoch)
    }
    pub(super) fn can_copy(&self) -> bool {
        matches!(self.startup, Startup::Ready { .. })
            && self.pending.is_none()
            && self.paste_offer.is_none()
    }
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some() || self.paste_offer.is_some()
    }
    pub(super) fn paste_pending(&self) -> bool {
        matches!(self.pending, Some(Pending::Reading { .. })) || self.paste_offer.is_some()
    }
    pub(super) fn take_paste_offer(&mut self) -> Option<PasteOffer> {
        self.paste_offer.take()
    }
    pub(super) fn read_for_paste(&mut self, destination: PathBuf) {
        if self.pending.is_some() || self.paste_offer.is_some() {
            return;
        }
        let Some(epoch) = self.ui_epoch.checked_add(1) else {
            self.error("Restart the app before making another clipboard request.");
            return;
        };
        self.ui_epoch = epoch;
        if !self.can_copy() {
            self.error(self.availability());
            return;
        }
        match self
            .service
            .as_mut()
            .ok_or(browser_native::x11::Error::Closed)
            .and_then(ClipboardService::read_files)
        {
            Ok(request) => {
                self.pending = Some(Pending::Reading {
                    request,
                    destination,
                    cancelled: false,
                });
                self.failed = false;
                self.message = Some("Reading files from the clipboard…".into());
            }
            Err(error) => self.error(native_error(error)),
        }
    }
    pub(super) fn paste_submitted(&mut self, accepted: bool) {
        self.failed = !accepted;
        // Accepted jobs own their progress screen and retained Transfers status.
        // A duplicate clipboard receipt consumes scarce archive/Find chrome.
        self.message = (!accepted).then(|| {
            "Paste was not submitted. Review the pending transfer or error before trying again."
                .into()
        });
    }
    pub(super) fn failed(&self) -> bool {
        self.failed
    }
    pub(super) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
    pub(super) fn availability(&self) -> &'static str {
        match self.startup {
            Startup::Connecting => "Connecting to the X11 file clipboard",
            Startup::Ready { .. } if self.pending.is_some() => {
                "A file clipboard request is in progress"
            }
            Startup::Ready { .. } => "Copy selected files to the native clipboard · Ctrl+C",
            Startup::Failed(_) | Startup::Closed => {
                "Native X11 file clipboard is unavailable on this display"
            }
        }
    }

    fn error(&mut self, message: impl Into<String>) {
        self.message = Some(message.into().chars().take(512).collect());
        self.failed = true;
    }

    pub(super) fn copy(&mut self, snapshot: Option<ListingSnapshot>, selection: &Selection) {
        if self.pending.is_some() {
            return;
        }
        let Some(epoch) = self.ui_epoch.checked_add(1) else {
            self.error("Restart the app before copying more files.");
            return;
        };
        self.ui_epoch = epoch;
        if !self.can_copy() {
            self.error(self.availability());
            return;
        }
        if selection.count() == 0 {
            self.error("Select files or folders to copy.");
            return;
        }
        let Some(snapshot) = snapshot else {
            self.error("Wait for the folder to finish loading before copying.");
            return;
        };
        let work = snapshot
            .ticket(selection, TicketLimits::default())
            .and_then(|ticket| {
                ticket.materialize(snapshot.revision(), MaterializeLimits::default())
            });
        match work {
            Ok(work) => {
                self.pending = Some(Pending::Materializing {
                    work,
                    count: selection.count(),
                });
                self.failed = false;
                self.message = Some(format!("Preparing {} selected items…", selection.count()));
            }
            Err(error) => self.error(selection_error(error)),
        }
    }

    pub(super) fn cancel(&mut self) {
        if self.paste_offer.take().is_some() {
            self.failed = false;
            self.message = Some("Paste cancelled before any copy job was submitted.".into());
            return;
        }
        match self.pending.as_mut() {
            Some(Pending::Materializing { .. }) => {
                // No native publication has been submitted. Drop requests worker cancellation.
                self.pending = None;
                self.message = Some("File clipboard request cancelled.".into());
            }
            Some(Pending::Publishing { request, .. }) => {
                request.cancel();
                // Keep the request until its terminal result: publication may already have happened.
                self.message = Some("Cancellation requested…".into());
            }
            Some(Pending::Reading {
                request, cancelled, ..
            }) => {
                *cancelled = true;
                request.cancel();
                self.message =
                    Some("Cancelling clipboard read; no copy job has been submitted…".into());
            }
            None => {}
        }
    }

    pub(super) fn dismiss(&mut self) {
        if !self.pending() {
            self.message = None;
        }
    }

    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(service) = &mut self.service {
            let startup = service.startup();
            changed |= self.startup != startup;
            self.startup = startup;
            for completion in service.poll() {
                let request_id = match self.pending.as_ref() {
                    Some(
                        Pending::Publishing { request, .. } | Pending::Reading { request, .. },
                    ) => Some(request.id),
                    _ => None,
                };
                if Some(completion.request) != request_id {
                    continue;
                }
                let pending = self.pending.take().expect("matching native request");
                changed = true;
                if let Pending::Reading {
                    destination,
                    cancelled,
                    ..
                } = pending
                {
                    if cancelled {
                        self.failed = false;
                        self.message =
                            Some("Paste cancelled before any copy job was submitted.".into());
                    } else {
                        match completion.result {
                            Ok(Outcome::Read(batch)) if batch.intent == CopyIntent::Copy => {
                                self.paste_offer = Some(PasteOffer {
                                    destination,
                                    paths: batch.paths,
                                });
                                self.failed = false;
                                self.message = Some("Clipboard read; preparing a copy job…".into());
                            }
                            Ok(Outcome::Read(_)) => {
                                self.failed = true;
                                self.message = Some("Cut and move are not supported yet. Source files were left unchanged.".into());
                            }
                            Ok(Outcome::Set { .. }) => {
                                self.failed = true;
                                self.message = Some(
                                    "Unexpected clipboard response. No copy job was submitted."
                                        .into(),
                                );
                            }
                            Err(error) => {
                                self.failed =
                                    !matches!(error, browser_native::x11::Error::Cancelled);
                                self.message = Some(native_error(error).into());
                            }
                        }
                    }
                    continue;
                }
                let Pending::Publishing { count, .. } = pending else {
                    unreachable!("matching native request")
                };
                match completion.result {
                    Ok(Outcome::Set {
                        cancelled_after_commit,
                        ..
                    }) => {
                        self.copied_count = count;
                        self.failed = false;
                        self.message = Some(if cancelled_after_commit {
                            format!("{count} items reached the clipboard before cancellation.")
                        } else {
                            format!("Copied {count} items to the clipboard.")
                        });
                    }
                    Ok(Outcome::Read(_)) => {
                        self.failed = true;
                        self.message =
                            Some("Unexpected clipboard response; files were not copied.".into());
                    }
                    Err(error) => {
                        self.failed = !matches!(error, browser_native::x11::Error::Cancelled);
                        self.message = Some(native_error(error).into());
                    }
                }
            }
            let ownership = service.ownership();
            if self.ownership != ownership {
                if self.ownership.is_ours && !ownership.is_ours && self.pending.is_none() {
                    self.message =
                        Some("The file clipboard was replaced by another application.".into());
                    self.failed = false;
                }
                self.ownership = ownership;
                changed = true;
            }
        }
        let result = match &mut self.pending {
            Some(Pending::Materializing { work, count }) => {
                work.poll().map(|result| (*count, result))
            }
            _ => None,
        };
        if let Some((count, result)) = result {
            self.pending = None;
            changed = true;
            match result {
                Ok(paths) => {
                    // These are the immutable ticket's native identities, even after navigation.
                    let result = self
                        .service
                        .as_mut()
                        .ok_or(browser_native::x11::Error::Closed)
                        .and_then(|service| {
                            service.set_files(FileBatch {
                                intent: CopyIntent::Copy,
                                paths: paths.into_paths(),
                            })
                        });
                    match result {
                        Ok(request) => {
                            self.pending = Some(Pending::Publishing { request, count });
                            self.message = Some(format!("Copying {count} items to the clipboard…"));
                        }
                        Err(error) => self.error(native_error(error)),
                    }
                }
                Err(error) => self.error(selection_error(error)),
            }
        }
        changed
    }

    pub(super) fn evidence(&self) -> String {
        let state = match self.pending {
            Some(Pending::Materializing { .. }) => "materializing",
            Some(Pending::Publishing { .. }) => "publishing",
            Some(Pending::Reading { .. }) => "reading",
            None if self.failed => "failed",
            None if self.ownership.is_ours => "owned",
            None => match self.startup {
                Startup::Connecting => "connecting",
                Startup::Ready { .. } => "ready",
                Startup::Failed(_) => "unavailable",
                Startup::Closed => "closed",
            },
        };
        format!(
            "{{\"transport\":\"X11\",\"state\":\"{state}\",\"copied_items\":{},\"owns_clipboard\":{},\"generation\":{}}}",
            self.copied_count, self.ownership.is_ours, self.ownership.generation
        )
    }
}

fn selection_error(error: TicketError) -> &'static str {
    match error {
        TicketError::TooManyItems => "Copy supports at most 4096 selected items in this build.",
        TicketError::PathBytesExceeded => {
            "The selected paths exceed the clipboard byte limit. Copy fewer items."
        }
        TicketError::TooManyRanges | TicketError::RangeBytesExceeded => {
            "The selection contains too many separate ranges. Copy fewer items."
        }
        TicketError::StaleRevision => {
            "The folder changed. Select the files again and retry copying."
        }
        TicketError::Busy => "The folder worker is busy. Retry copying in a moment.",
        TicketError::Cancelled => "File clipboard request cancelled.",
        TicketError::WorkerClosed => {
            "The folder is no longer available. Open it again to copy files."
        }
        TicketError::InvalidRanges | TicketError::InvalidLimits => {
            "The selection could not be used. Refresh the folder and select the files again."
        }
    }
}
fn native_error(error: browser_native::x11::Error) -> &'static str {
    use browser_native::x11::Error;
    match error {
        Error::Cancelled => "File clipboard request cancelled.",
        Error::Busy => "The native clipboard is busy. Retry in a moment.",
        Error::Timeout => "The native clipboard did not respond. Retry the request.",
        Error::TooLarge => "The selected files exceed the clipboard limits. Copy fewer items.",
        Error::OwnerChanged => {
            "Another application replaced the clipboard during this request. Retry the request."
        }
        Error::PublicationUncertain => {
            "Clipboard ownership could not be confirmed. Check the clipboard before retrying."
        }
        Error::InvalidPayload(_) => {
            "These paths cannot be represented by the supported local file clipboard formats."
        }
        Error::NoOwner => "No application owns the file clipboard.",
        Error::UnsupportedTargets => "The clipboard does not contain a supported file list.",
        Error::Closed
        | Error::Unavailable
        | Error::InvalidConfig
        | Error::Protocol(_)
        | Error::Native(_) => {
            "The native X11 file clipboard is unavailable. Restart the app or check the desktop connection."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BrowserView, make_ui};
    use mui::{host::View, prelude::*};

    fn step(view: &mut BrowserView, ui: &mut Ui, size: Size, input: Input) {
        let tree = view.build(ui, &input);
        ui.frame(tree, Some(size), input, 0.016).unwrap();
        view.after_frame(ui);
    }

    #[test]
    fn cancel_realized_in_the_frame_of_a_ready_offer_prevents_copy_admission() {
        let mut view = BrowserView::fixture(0);
        view.jobs.file_clipboard = Some(ClipboardUi {
            service: None,
            startup: Startup::Ready { window: 1 },
            ownership: Ownership::default(),
            pending: None,
            message: Some("Reading files from the clipboard…".into()),
            failed: false,
            copied_count: 0,
            ui_epoch: 1,
            paste_offer: None,
        });
        let mut ui = make_ui().unwrap();
        let size = Size::new(640., 480.);
        let id = view
            .jobs
            .file_clipboard
            .as_ref()
            .unwrap()
            .action_id("cancel");
        // Paint the prior reading scene's real control without a native worker.
        // The independent X11 review harness covers the actual pending Read.
        let reading_tree = |ui: &Ui| {
            row([crate::action(
                ui,
                &id,
                "Cancel file clipboard request",
                crate::label("Cancel"),
                true,
                false,
            )])
            .w(pct(100))
            .h(pct(100))
        };
        for _ in 0..3 {
            let tree = reading_tree(&ui);
            ui.frame(tree, Some(size), Input::default(), 0.016).unwrap();
        }
        let frame = ui.scene().unwrap().surface(&id).unwrap().frame;
        let point = Point::new(
            frame.x + frame.size.width / 2.,
            frame.y + frame.size.height / 2.,
        );
        for down in [false, true] {
            let tree = reading_tree(&ui);
            ui.frame(
                tree,
                Some(size),
                Input::from(PointerInput {
                    pos: Some(point),
                    buttons: Buttons::default().set(Button::Primary, down),
                    ..PointerInput::default()
                }),
                0.016,
            )
            .unwrap();
            view.after_frame(&mut ui);
        }
        // A Read becomes ready before this release frame is resolved.
        view.jobs.file_clipboard.as_mut().unwrap().paste_offer = Some(PasteOffer {
            paths: vec!["/megaman-cancel-fixture/source".into()],
            destination: "/megaman-cancel-fixture/destination".into(),
        });
        // Admission is forbidden during build, even though the offer is ready.
        let _ = view.build(&mut ui, &Input::default());
        assert!(view.jobs.copy_transfers.is_none());
        let tree = reading_tree(&ui);
        ui.frame(
            tree,
            Some(size),
            Input::from(PointerInput {
                pos: Some(point),
                ..PointerInput::default()
            }),
            0.016,
        )
        .unwrap();
        assert!(ui.get(&id).clicked);
        view.after_frame(&mut ui);
        assert!(
            view.jobs.copy_transfers.is_none(),
            "Cancel must precede copy admission"
        );
        let clipboard = view.jobs.file_clipboard.as_ref().unwrap();
        assert!(!clipboard.paste_pending());
        assert!(clipboard.message().unwrap().contains("before any copy job"));
    }

    #[test]
    fn unavailable_clipboard_has_intrinsic_dismissible_feedback_without_native_io() {
        for size in [
            Size::new(320., 240.),
            Size::new(640., 480.),
            Size::new(1024., 720.),
        ] {
            let mut view = BrowserView::fixture(10);
            // A pure model fixture: no server discovery, worker or clipboard access.
            let mut clipboard = ClipboardUi {
                service: None,
                startup: Startup::Failed(browser_native::x11::Error::Unavailable),
                ownership: Ownership::default(),
                pending: None,
                message: None,
                failed: false,
                copied_count: 0,
                ui_epoch: 0,
                paste_offer: None,
            };
            clipboard.copy(None, &view.tab().selection);
            assert!(clipboard.failed());
            view.jobs.file_clipboard = Some(clipboard);
            let mut ui = make_ui().unwrap();
            for _ in 0..6 {
                step(&mut view, &mut ui, size, Input::default());
            }
            let scene = ui.scene().unwrap();
            let root = scene.surface("browser-root").unwrap().frame;
            let copy_id = view.tab().copy_files_id();
            let dismiss_id = view
                .jobs
                .file_clipboard
                .as_ref()
                .unwrap()
                .action_id("dismiss");
            let cancel_id = view
                .jobs
                .file_clipboard
                .as_ref()
                .unwrap()
                .action_id("cancel");
            for id in [
                copy_id.as_str(),
                "file-clipboard-status",
                dismiss_id.as_str(),
            ] {
                let frame = scene.surface(id).unwrap().frame;
                assert!(
                    frame.x >= root.x && frame.right() <= root.right() + 0.01,
                    "{id}: {frame:?}"
                );
                assert!(
                    frame.y >= root.y && frame.bottom() <= root.bottom() + 0.01,
                    "{id}: {frame:?}"
                );
            }
            assert!(scene.surface(&cancel_id).is_none());
            let button = scene.surface(&dismiss_id).unwrap().frame;
            let point = Point::new(
                button.x + button.size.width / 2.,
                button.y + button.size.height / 2.,
            );
            for down in [false, true, false] {
                step(
                    &mut view,
                    &mut ui,
                    size,
                    Input::from(PointerInput {
                        pos: Some(point),
                        buttons: Buttons::default().set(Button::Primary, down),
                        ..PointerInput::default()
                    }),
                );
            }
            for _ in 0..4 {
                step(&mut view, &mut ui, size, Input::default());
            }
            assert!(
                view.jobs
                    .file_clipboard
                    .as_ref()
                    .unwrap()
                    .message()
                    .is_none()
            );
            assert!(
                ui.scene()
                    .unwrap()
                    .surface("file-clipboard-status")
                    .is_none()
            );
            assert_eq!(
                view.tab().selection.count(),
                1,
                "dismissal must not change file selection"
            );
        }
    }
}
