//! Application-owned mutation and native clipboard controllers.
//! No controller or accepted operation belongs to a directory tab.
use super::*;
use extraction::ExtractionInputOwner;

#[derive(Default)]
pub(super) struct AppJobs {
    #[cfg(target_os = "linux")]
    pub(super) file_clipboard: Option<clipboard::ClipboardUi>,
    pub(super) extraction: Option<ExtractionUi>,
    #[cfg(target_os = "linux")]
    pub(super) copy_transfers: Option<transfers::TransfersUi>,
    pub(super) fileops: fileops::FileOps,
    /// Files dropped on the window from other applications, waiting for a free worker.
    pub(super) dropped: Vec<PathBuf>,
    pub(super) places: places::Places,
    pub(super) settings: settings::Settings,
    pub(super) copy_return_pending: bool,
    pub(super) geometry_changed: bool,
    // The built tree and the source of current host input are deliberately
    // separate: host resolve delivers its input after View::build returns.
    pub(super) presented_extraction_owner: Option<ExtractionInputOwner>,
    pub(super) raw_extraction_owner: Option<ExtractionInputOwner>,
    pub(super) built_extraction_owner: Option<ExtractionInputOwner>,
}

impl AppJobs {
    pub(super) fn poll_file_clipboard(&mut self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(clipboard) = &mut self.file_clipboard {
            return clipboard.poll();
        }
        false
    }
    pub(super) fn clipboard_evidence(&self) -> String {
        #[cfg(target_os = "linux")]
        if let Some(clipboard) = &self.file_clipboard {
            return clipboard.evidence();
        }
        "null".into()
    }
    pub(super) fn mutations_busy(&self) -> bool {
        let extraction_busy =
            self.extraction.as_ref().is_some_and(ExtractionUi::busy) || self.fileops.busy();
        #[cfg(target_os = "linux")]
        return extraction_busy
            || self
                .copy_transfers
                .as_ref()
                .is_some_and(transfers::TransfersUi::busy)
            || self
                .file_clipboard
                .as_ref()
                .is_some_and(clipboard::ClipboardUi::paste_pending);
        #[cfg(not(target_os = "linux"))]
        extraction_busy
    }
    // Admission runs only after this frame's controls have been resolved. A completed
    // native Read must not overtake a Cancel click in the frame being processed.
    pub(super) fn admit_paste_offer(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(offer) = self
            .file_clipboard
            .as_mut()
            .and_then(clipboard::ClipboardUi::take_paste_offer)
        {
            let accepted = if self.mutations_busy() {
                false
            } else if self.fileops.take_cut_matching(&offer.paths) {
                self.fileops.start_move(offer.paths, offer.destination)
            } else {
                self.copy_transfers
                    .get_or_insert_with(transfers::TransfersUi::new)
                    .start(offer.paths, offer.destination)
            };
            if let Some(clipboard) = &mut self.file_clipboard {
                clipboard.paste_submitted(accepted);
            }
            self.geometry_changed = true;
        }
    }
    pub(super) fn dispatch_clipboard_controls(&mut self, ui: &Ui) {
        #[cfg(target_os = "linux")]
        if let Some(clipboard) = &mut self.file_clipboard {
            if clicked(ui, &clipboard.action_id("cancel")) {
                clipboard.cancel();
            }
            if clicked(ui, &clipboard.action_id("dismiss")) {
                clipboard.dismiss();
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ui;
    }
    pub(super) fn build_copy_transfers(&mut self, ui: &mut Ui) -> Option<El> {
        #[cfg(target_os = "linux")]
        if let Some(transfers) = self.copy_transfers.as_mut().filter(|t| t.visible()) {
            let (tree, _) = transfers.build(ui);
            if !transfers.visible() {
                self.copy_return_pending = true;
                self.geometry_changed = true;
            }
            return Some(tree);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ui;
        None
    }
    pub(super) fn with_copy_transfer_status(&mut self, ui: &Ui, tree: El) -> El {
        #[cfg(target_os = "linux")]
        if let Some(transfers) = self
            .copy_transfers
            .as_mut()
            .filter(|t| t.busy() && !t.visible())
        {
            let id = transfers.action_id("show");
            if clicked(ui, &id) {
                transfers.show();
                self.geometry_changed = true;
            }
            let summary = transfers.summary();
            return col([
                tree.grow(1).basis(0).min_h(0),
                hairline(),
                row([
                    detail(summary.clone())
                        .grow(1)
                        .basis(0)
                        .min_w(0)
                        .lines(1)
                        .clip()
                        .tip(summary),
                    action(
                        ui,
                        &id,
                        "Show copy progress and results",
                        label("Transfers"),
                        true,
                        false,
                    ),
                ])
                .gap(7)
                .pad((8, 3))
                .align(Align::Center)
                .fill(chrome())
                .shrink(0),
            ])
            .w(pct(100))
            .h(pct(100))
            .min_w(0)
            .min_h(0);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ui;
        tree
    }
    pub(super) fn clipboard_status(&mut self, ui: &Ui) -> Option<El> {
        #[cfg(target_os = "linux")]
        if let Some(clipboard) = &mut self.file_clipboard {
            let cancel_id = clipboard.action_id("cancel");
            let dismiss_id = clipboard.action_id("dismiss");
            let message = clipboard.message()?.to_owned();
            return Some(
                row([
                    detail(message.clone())
                        .grow(1)
                        .basis(0)
                        .min_w(0)
                        .lines(1)
                        .clip()
                        .tip(message)
                        .when(clipboard.failed(), |el| el.fill(color(0xffb4ab))),
                    if clipboard.pending() {
                        action(
                            ui,
                            &cancel_id,
                            "Cancel file clipboard request",
                            label("Cancel"),
                            true,
                            false,
                        )
                    } else {
                        action(
                            ui,
                            &dismiss_id,
                            "Dismiss clipboard status",
                            glyph(Glyph::Close, muted()),
                            true,
                            false,
                        )
                    },
                ])
                .gap(7)
                .pad((8, 3))
                .align(Align::Center)
                .fill(chrome())
                .shrink(0)
                .min_w(0)
                .id("file-clipboard-status"),
            );
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ui;
        None
    }
    pub(super) fn build_extraction(&mut self, ui: &mut Ui, _gated_input: &Input) -> Option<El> {
        let extraction = self.extraction.as_mut().filter(|panel| panel.visible())?;
        let owner = extraction.input_owner();
        if owner != self.presented_extraction_owner {
            ui.discard_input_for_view_change();
        }
        self.built_extraction_owner = owner;
        let (tree, outcome) = extraction.build(ui);
        if matches!(outcome, Some(ExtractionAction::Back)) {
            self.geometry_changed = true;
            self.copy_return_pending = true;
        }
        // Keep this frame on its own tree so Back's Escape/Enter cannot also
        // activate an archive or host action from the previous input batch.
        Some(tree)
    }
    pub(super) fn with_mutation_status(&mut self, ui: &Ui, tree: El) -> El {
        // A native Read keeps its destination while browsing another view. Keep
        // its Cancel control reachable until the worker response is consumed.
        let tree = if let Some(status) = self.clipboard_status(ui) {
            col([tree.grow(1).basis(0).min_h(0), hairline(), status])
                .w(pct(100))
                .h(pct(100))
                .min_w(0)
                .min_h(0)
        } else {
            tree
        };
        self.with_transfer_statuses(ui, tree)
    }
    /// Main-view variant: the browser tree already embeds the clipboard status.
    pub(super) fn with_transfer_statuses(&mut self, ui: &Ui, tree: El) -> El {
        let tree = self.with_copy_transfer_status(ui, tree);
        self.with_extraction_status(ui, tree)
    }
    pub(super) fn with_extraction_status(&mut self, ui: &Ui, tree: El) -> El {
        if let Some(status) = self
            .extraction
            .as_mut()
            .and_then(|panel| panel.build_status(ui))
        {
            col([status, hairline(), tree.grow(1).basis(0).min_h(0).min_w(0)])
                .w(pct(100))
                .h(pct(100))
                .min_w(0)
                .min_h(0)
        } else {
            tree
        }
    }

    pub(super) fn poll(&mut self) -> (bool, Vec<PathBuf>) {
        let mut changed = self.poll_file_clipboard() | self.fileops.poll();
        let mut refresh = self.fileops.take_refresh();
        #[cfg(target_os = "linux")]
        if self.copy_transfers.as_ref().is_none_or(|c| !c.busy())
            && let Some((paths, dest)) = self.fileops.take_outbox()
        {
            self.copy_transfers
                .get_or_insert_with(transfers::TransfersUi::new)
                .start(paths, dest);
            self.geometry_changed = true;
        }
        if let Some(extraction) = &mut self.extraction {
            changed |= extraction.poll();
            if let Some(parent) = extraction.take_refresh_parent() {
                refresh.push(parent);
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(copy) = &mut self.copy_transfers {
            changed |= copy.poll();
            if let Some(destination) = copy.take_refresh_destination() {
                refresh.push(destination);
            }
        }
        changed |= std::mem::take(&mut self.geometry_changed);
        (changed, refresh)
    }
}
