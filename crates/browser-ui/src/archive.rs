//! Archive metadata UI and explicit extraction selection. Logical prefixes never become host paths.
use super::{
    Glyph, OVERSCAN, ROW, action, bytes, chrome, clicked, color, detail, entry_glyph, glyph,
    ground, hairline, label, muted, selected, selected_ink,
};
use browser_archive::{
    ArchiveBrowser, ArchivePrefix, BrowserCatalogState, BrowserErrorKind, BrowserGeneration,
    BrowserIssueKind, BrowserNode, BrowserOptions, BrowserPage, EntryKind, ErrorKind,
    ExtractionItem, ExtractionSelection, ListRequest, MAX_BROWSER_PAGE_SIZE,
};
use mui::prelude::*;
use std::{ops::Range, path::PathBuf};

pub(super) enum ArchiveAction {
    Close,
    GoTo,
    Extract(ExtractionSelection),
}

pub(super) struct ArchivePanel {
    source: PathBuf,
    browser: Option<ArchiveBrowser>,
    generation: Option<BrowserGeneration>,
    request_id: u64,
    prefix: ArchivePrefix,
    offset: usize,
    page: Option<Box<BrowserPage>>,
    previous_offsets: Vec<usize>,
    pending: bool,
    cancel_requested: bool,
    reopen_when_stopped: bool,
    worker_stopped: bool,
    error: Option<String>,
    list: ListState,
    row_epoch: u64,
    selected: Option<usize>,
    selection_armed: bool,
    rows_built: usize,
    visible: Range<usize>,
    viewport: Size,
    window: Size,
    geometry_changed: bool,
    initial_focus: bool,
    mutations_busy: bool,
}

impl ArchivePanel {
    pub(super) fn new(source: PathBuf) -> Self {
        let mut panel = Self {
            source,
            browser: None,
            generation: None,
            request_id: 0,
            prefix: ArchivePrefix::root(),
            offset: 0,
            page: None,
            previous_offsets: Vec::new(),
            pending: false,
            cancel_requested: false,
            reopen_when_stopped: false,
            worker_stopped: false,
            error: None,
            list: ListState::uniform_count(0, ROW),
            row_epoch: 0,
            selected: None,
            selection_armed: false,
            rows_built: 0,
            visible: 0..0,
            viewport: Size::new(320., 100.),
            window: Size::new(320., 240.),
            geometry_changed: false,
            initial_focus: true,
            mutations_busy: false,
        };
        panel.open_service();
        panel
    }

    fn open_service(&mut self) {
        match ArchiveBrowser::open(BrowserOptions::new(self.source.clone())) {
            Ok(browser) => {
                self.generation = Some(browser.generation());
                self.browser = Some(browser);
                self.request_id = 0;
                self.prefix = ArchivePrefix::root();
                self.offset = 0;
                self.previous_offsets.clear();
                self.page = None;
                self.pending = true;
                self.cancel_requested = false;
                self.reopen_when_stopped = false;
                self.worker_stopped = false;
                self.error = None;
                self.reset_rows();
            }
            Err(error) => {
                self.pending = false;
                self.reopen_when_stopped = false;
                self.error = Some(format!("Could not open archive: {error}"));
            }
        }
    }

    fn reset_rows(&mut self) {
        self.row_epoch = self.row_epoch.wrapping_add(1);
        self.selected = None;
        self.selection_armed = false;
        self.list =
            ListState::uniform_count(self.page.as_ref().map_or(0, |page| page.nodes.len()), ROW);
        self.rows_built = 0;
        self.visible = 0..0;
    }

    fn request(&mut self, prefix: ArchivePrefix, offset: usize) -> bool {
        if self.cancel_requested || self.reopen_when_stopped {
            return false;
        }
        let Some(id) = self.request_id.checked_add(1) else {
            self.error = Some("Close and reopen the archive to continue browsing.".into());
            return false;
        };
        let request = ListRequest {
            request_id: id,
            prefix: prefix.clone(),
            offset,
            limit: MAX_BROWSER_PAGE_SIZE,
        };
        match self
            .browser
            .as_ref()
            .map(|browser| browser.request_list(request))
        {
            Some(Ok(())) => {
                self.request_id = id;
                self.prefix = prefix;
                self.offset = offset;
                self.page = None;
                self.pending = true;
                self.error = None;
                self.reset_rows();
                true
            }
            Some(Err(error)) => {
                self.error = Some(format!("Could not list archive folder: {error}"));
                false
            }
            None => {
                self.error =
                    Some("Archive browsing is unavailable. Choose Reopen to try again.".into());
                false
            }
        }
    }

    fn navigate(&mut self, prefix: ArchivePrefix) {
        if self.request(prefix, 0) {
            self.previous_offsets.clear();
        }
    }

    fn activate(&mut self, index: usize) {
        let prefix = self
            .page
            .as_ref()
            .and_then(|page| page.nodes.get(index))
            .filter(|node| node.kind == EntryKind::Directory)
            .and_then(|node| node.navigation.clone());
        if let Some(prefix) = prefix {
            self.navigate(prefix);
        }
        // File rows are metadata only: never materialize, execute or invent host member paths.
    }

    fn cancel(&mut self) {
        if let Some(browser) = &self.browser {
            browser.cancel();
        }
        self.cancel_requested = true;
        self.pending = false;
        self.reset_rows();
    }

    fn reopen(&mut self) {
        if self
            .browser
            .as_ref()
            .is_some_and(|browser| !browser.is_worker_stopped())
        {
            self.reopen_when_stopped = true;
            self.cancel();
        } else {
            self.open_service();
        }
    }

    fn apply(&mut self, page: Box<BrowserPage>) -> bool {
        if Some(page.generation) != self.generation
            || page.request_id != self.request_id
            || page.source.as_ref() != &self.source
            || page.prefix != self.prefix
            || page.offset != self.offset
        {
            return false;
        }
        self.pending = false;
        self.error = None;
        self.page = Some(page);
        self.reset_rows();
        true
    }

    pub(super) fn set_mutations_busy(&mut self, busy: bool) {
        self.mutations_busy = busy;
    }
    pub(super) fn restore_focus(&mut self) {
        self.initial_focus = true;
    }
    pub(super) fn prepare_extraction(
        &self,
        extraction: &mut super::extraction::ExtractionUi,
        selection: ExtractionSelection,
        destination: PathBuf,
    ) -> bool {
        self.browser
            .as_ref()
            .is_some_and(|browser| extraction.prepare(browser, selection, destination))
    }
    fn can_extract(&self) -> bool {
        cfg!(target_os = "linux")
            && !self.mutations_busy
            && !self.pending
            && !self.cancel_requested
            && !self.source_changed()
            && self.page.as_ref().is_some_and(|page| {
                page.catalog.state == BrowserCatalogState::Complete && page.catalog.source_valid
            })
    }
    fn selected_extraction(&self) -> Option<ExtractionSelection> {
        if !self.can_extract() || !self.selection_armed {
            return None;
        }
        let page = self.page.as_ref()?;
        let node = page.nodes.get(self.selected?)?;
        if node.issue.is_some() {
            return None;
        }
        let item = if node.kind == EntryKind::Directory {
            ExtractionItem::Folder(node.navigation.clone()?)
        } else {
            ExtractionItem::Record(node.record_id?)
        };
        Some(ExtractionSelection::Items {
            generation: page.generation,
            items: vec![item],
        })
    }
    fn folder_extraction(&self) -> Option<ExtractionSelection> {
        self.can_extract().then(|| ExtractionSelection::Items {
            generation: self.generation.expect("complete browser generation"),
            items: vec![ExtractionItem::Folder(self.prefix.clone())],
        })
    }
    fn extraction_id(&self, role: &str) -> String {
        format!(
            "archive-extract-{}-{}-{}-{}-{role}",
            self.generation.map_or(0, BrowserGeneration::value),
            self.request_id,
            self.row_epoch,
            self.selected.unwrap_or(usize::MAX)
        )
    }
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.geometry_changed);
        if let Some(page) = self.browser.as_ref().and_then(ArchiveBrowser::poll) {
            changed |= self.apply(page);
        }
        let stopped = self
            .browser
            .as_ref()
            .is_some_and(ArchiveBrowser::is_worker_stopped);
        if self.worker_stopped != stopped {
            self.worker_stopped = stopped;
            changed = true;
        }
        if self.reopen_when_stopped && stopped {
            self.open_service();
            changed = true;
        }
        changed
    }

    fn list_id(&self) -> String {
        format!("archive-page-{}", self.row_epoch)
    }
    fn prefix_label(&self) -> String {
        if self.prefix.components().is_empty() {
            "Archive root".into()
        } else {
            format!("/{}", self.prefix.components().join("/"))
        }
    }
    fn source_changed(&self) -> bool {
        self.page.as_ref().is_some_and(|page| {
            [&page.error, &page.catalog.error]
                .into_iter()
                .flatten()
                .any(|error| error.kind == BrowserErrorKind::Archive(ErrorKind::SourceChanged))
        })
    }
    fn state(&self) -> BrowserCatalogState {
        self.page.as_ref().map_or_else(
            || {
                if self.browser.is_none() && self.error.is_some() {
                    BrowserCatalogState::Failed
                } else {
                    BrowserCatalogState::Opening
                }
            },
            |page| page.catalog.state,
        )
    }
    fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if self.reopen_when_stopped {
            return "Reopening · waiting for the previous worker to stop".into();
        }
        if self.cancel_requested && !self.worker_stopped {
            return "Cancellation requested · waiting for the worker".into();
        }
        if self.source_changed() {
            return "Archive source changed · Reopen to load its current contents".into();
        }
        if self.page.as_ref().is_some_and(|page| page.error.is_some()) {
            return "Could not list this archive folder · see error details".into();
        }
        if self.pending {
            return "Loading archive folder…".into();
        }
        let indexed = self
            .page
            .as_ref()
            .map_or(0, |page| page.catalog.indexed_records);
        match self.state() {
            BrowserCatalogState::Opening => "Reading archive metadata…".into(),
            BrowserCatalogState::Complete => {
                format!("Catalog complete · {indexed} records indexed")
            }
            BrowserCatalogState::LimitReached => {
                format!("Partial catalog · limit reached · {indexed} records indexed")
            }
            BrowserCatalogState::Failed if indexed > 0 => {
                format!("Partial catalog · reading failed · {indexed} records retained")
            }
            BrowserCatalogState::Failed => "Could not read this archive".into(),
            BrowserCatalogState::Cancelled => {
                "Archive browsing cancelled · Reopen to continue".into()
            }
        }
    }
    fn error_detail(&self) -> Option<String> {
        self.error.clone().or_else(|| {
            self.page
                .as_ref()
                .and_then(|page| page.error.as_ref().or(page.catalog.error.as_ref()))
                .map(|error| {
                    format!(
                        "{}{}",
                        error.message,
                        if error.message_truncated {
                            " [truncated]"
                        } else {
                            ""
                        }
                    )
                })
        })
    }

    pub(super) fn build(&mut self, ui: &mut Ui) -> (El, Option<ArchiveAction>) {
        self.poll();
        if self.initial_focus {
            ui.focus(self.list_id());
            self.initial_focus = false;
        }
        let mut outcome = None;
        let mut keys = ui.shortcuts().to_vec();
        if ui.focus_is_text() {
            keys.extend(
                ui.keys(ui.focus_key().unwrap_or("archive-source"))
                    .iter()
                    .filter(|key| {
                        key.key == Key::Escape
                            || ((key.mods.ctrl || key.mods.cmd)
                                && matches!(key.key, Key::Char('l' | 'L')))
                    })
                    .copied(),
            );
        }
        for key in keys {
            if key.key == Key::Escape {
                outcome = Some(ArchiveAction::Close);
            }
            if (key.mods.ctrl || key.mods.cmd) && matches!(key.key, Key::Char('l' | 'L')) {
                outcome = Some(ArchiveAction::GoTo);
            }
            if key.mods.alt
                && key.key == Key::Up
                && let Some(parent) = self.prefix.parent()
            {
                self.navigate(parent);
            }
        }
        if clicked(ui, "archive-close") {
            outcome = Some(ArchiveAction::Close);
        }
        if clicked(ui, "archive-go-to") {
            outcome = Some(ArchiveAction::GoTo);
        }
        if clicked(ui, "archive-cancel") {
            self.cancel();
        }
        if clicked(ui, "archive-reopen") {
            self.reopen();
        }
        let can_navigate = !self.pending
            && !self.cancel_requested
            && !self.reopen_when_stopped
            && !self.source_changed();
        let can_parent = can_navigate && self.prefix.parent().is_some();
        if can_parent && clicked(ui, "archive-parent") {
            self.navigate(self.prefix.parent().expect("has parent"));
        }
        if can_parent && clicked(ui, "archive-root-nav") {
            self.navigate(ArchivePrefix::root());
        }

        let can_previous = can_navigate && !self.previous_offsets.is_empty();
        let next = self
            .page
            .as_ref()
            .and_then(|page| page.next_offset)
            .filter(|_| can_navigate);
        if can_previous && clicked(ui, "archive-previous") {
            let target = *self.previous_offsets.last().expect("previous page");
            if self.request(self.prefix.clone(), target) {
                self.previous_offsets.pop();
            }
        }
        if let Some(next) = next
            && clicked(ui, "archive-next")
        {
            let previous = self.offset;
            if self.request(self.prefix.clone(), next) {
                self.previous_offsets.push(previous);
            }
        }
        let results = self.results(ui);
        let selected_id = self.extraction_id("selected");
        let folder_id = self.extraction_id("folder");
        let all_id = self.extraction_id("all");
        let selected_extraction = self.selected_extraction();
        let folder_extraction = self.folder_extraction();
        if outcome.is_none() {
            if clicked(ui, &selected_id)
                && let Some(selection) = selected_extraction.clone()
            {
                outcome = Some(ArchiveAction::Extract(selection));
            } else if clicked(ui, &folder_id)
                && let Some(selection) = folder_extraction.clone()
            {
                outcome = Some(ArchiveAction::Extract(selection));
            } else if clicked(ui, &all_id) && self.can_extract() {
                outcome = Some(ArchiveAction::Extract(ExtractionSelection::All));
            }
        }
        let extraction_actions = row([
            action(
                ui,
                &selected_id,
                "Extract selected record or logical folder to a new folder",
                label("Extract selected"),
                selected_extraction.is_some(),
                false,
            ),
            action(
                ui,
                &folder_id,
                "Extract the current logical archive folder to a new folder",
                label("Current folder"),
                folder_extraction.is_some(),
                false,
            ),
            action(
                ui,
                &all_id,
                "Extract the whole archive to a new folder",
                label("Whole archive"),
                self.can_extract(),
                false,
            ),
        ])
        .gap(5)
        .pad((6, 3))
        .min_w(0)
        .fill(chrome())
        .shrink(0)
        .id("archive-extraction-actions");
        // Use the pane's measured available height, including room consumed by
        // retained operation status. Secondary source/metadata details remain in
        // tooltips while primary actions, errors, paging and file rows stay visible.
        let compact = self.window.height < 300.;
        let native_source = self.source.to_string_lossy().into_owned();
        let title = self
            .source
            .file_name()
            .unwrap_or(self.source.as_os_str())
            .to_string_lossy()
            .into_owned();
        let working = self.pending || self.state() == BrowserCatalogState::Opening;
        let mut header_items = vec![
            action(
                ui,
                "archive-close",
                "Back to folder · Escape",
                glyph(Glyph::Back, muted()),
                true,
                false,
            ),
            label(title)
                .grow(1)
                .basis(0)
                .min_w(0)
                .clip()
                .tip(native_source.clone()),
            detail("ZIP metadata").shrink(0),
            if working && !self.cancel_requested {
                action(
                    ui,
                    "archive-cancel",
                    "Cancel archive loading",
                    label("Cancel"),
                    true,
                    false,
                )
            } else {
                action(
                    ui,
                    "archive-reopen",
                    "Reopen archive metadata",
                    label("Reopen"),
                    !self.reopen_when_stopped,
                    false,
                )
            },
        ];
        if compact {
            header_items.push(action(
                ui,
                "archive-go-to",
                "Go to a filesystem location · Ctrl+L",
                glyph(Glyph::Search, muted()),
                true,
                false,
            ));
        }
        let header = row(header_items)
            .gap(7)
            .pad((6, 4))
            .align(Align::Center)
            .fill(chrome())
            .shrink(0);
        let source = row([
            detail("Source").shrink(0),
            detail(native_source.clone())
                .grow(1)
                .basis(0)
                .min_w(0)
                .lines(1)
                .clip()
                .tip(native_source),
            action(
                ui,
                "archive-go-to",
                "Go to a filesystem location · Ctrl+L",
                glyph(Glyph::Search, muted()),
                true,
                false,
            ),
        ])
        .gap(7)
        .pad((8, 3))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0)
        .id("archive-source");
        let prefix = self.prefix_label();
        let navigation = row([
            action(
                ui,
                "archive-parent",
                "Parent archive folder · Alt+Up",
                glyph(Glyph::Up, muted()),
                can_parent,
                false,
            ),
            action(
                ui,
                "archive-root-nav",
                "Archive root",
                glyph(Glyph::Home, muted()),
                can_parent,
                false,
            ),
            label(prefix.clone())
                .grow(1)
                .basis(0)
                .min_w(0)
                .lines(1)
                .clip()
                .tip(prefix),
        ])
        .gap(5)
        .pad((6, 3))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0)
        .id("archive-prefix");
        let status = self.status();
        let metadata_note = "Browsing does not inflate member payloads. Sizes and methods are declared metadata. Extraction uses a separate verified job. Previews, nested opening and archive creation remain unavailable.";
        let mut summaries = vec![
            detail(status.clone())
                .lines(1)
                .min_w(0)
                .clip()
                .tip(format!("{status} · {metadata_note}"))
                .when(
                    self.error.is_some()
                        || self.source_changed()
                        || matches!(
                            self.state(),
                            BrowserCatalogState::Failed | BrowserCatalogState::LimitReached
                        ),
                    |e| e.fill(color(0xffb4ab)),
                ),
        ];
        if !compact {
            summaries.push(
                detail("Metadata only · declared sizes · payloads not verified")
                    .lines(1)
                    .min_w(0)
                    .clip()
                    .tip(metadata_note),
            );
        }
        if let Some(error) = self.error_detail() {
            summaries.push(
                detail(error.clone())
                    .fill(color(0xffb4ab))
                    .lines(1)
                    .min_w(0)
                    .clip()
                    .tip(error),
            );
        }
        let summary = col(summaries)
            .gap(2)
            .pad((8, 4))
            .min_w(0)
            .fill(chrome())
            .shrink(0)
            .id("archive-status");
        let count = self.page.as_ref().map_or_else(
            || "No folder page yet".into(),
            |page| {
                let start = if page.nodes.is_empty() {
                    0
                } else {
                    self.offset.saturating_add(1)
                };
                let end = if page.nodes.is_empty() {
                    0
                } else {
                    self.offset.saturating_add(page.nodes.len())
                };
                format!(
                    "Rows {start}–{end} of {} known{}",
                    page.total_children,
                    if page.catalog.state == BrowserCatalogState::Complete {
                        ""
                    } else {
                        " · partial"
                    }
                )
            },
        );
        let footer = row([
            action(
                ui,
                "archive-previous",
                "Previous archive page",
                glyph(Glyph::Back, muted()),
                can_previous,
                false,
            ),
            detail(count).grow(1).basis(0).min_w(0).clip(),
            action(
                ui,
                "archive-next",
                "Next archive page",
                glyph(Glyph::Forward, muted()),
                next.is_some(),
                false,
            ),
        ])
        .gap(6)
        .pad((8, 4))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0);
        let mut regions = vec![header];
        if !compact {
            regions.push(source);
        }
        regions.extend([
            navigation,
            extraction_actions,
            summary,
            hairline(),
            results,
            hairline(),
            footer,
        ]);
        (
            col(regions)
                .w(pct(100))
                .h(pct(100))
                .min_w(0)
                .min_h(0)
                .fill(ground())
                .clip()
                .id("archive-panel")
                .named("Read-only archive folder browser"),
            outcome,
        )
    }

    fn results(&mut self, ui: &mut Ui) -> El {
        let id = self.list_id();
        let has_nodes = self
            .page
            .as_ref()
            .is_some_and(|page| !page.nodes.is_empty());
        let focused = ui
            .focus_key()
            .is_some_and(|focus| focus.starts_with("archive-page-"))
            || (ui.focus_key().is_none() && has_nodes);
        if focused
            && !ui
                .focus_key()
                .is_some_and(|focus| focus_matches(focus, &id))
        {
            ui.focus(&id);
        }
        let keys = ui.keys(ui.focus_key().unwrap_or(&id));
        if focused
            && keys.iter().any(|key| {
                !key.mods.ctrl
                    && !key.mods.cmd
                    && !key.mods.alt
                    && matches!(
                        key.key,
                        Key::Up | Key::Down | Key::Home | Key::End | Key::PageUp | Key::PageDown
                    )
            })
        {
            self.selection_armed = true;
        }
        let enter = focused
            && keys.iter().any(|key| {
                key.key == Key::Enter && !key.mods.ctrl && !key.mods.cmd && !key.mods.alt
            });
        let nodes = self
            .page
            .as_ref()
            .map(|page| page.nodes.as_slice())
            .unwrap_or(&[]);
        let armed = self.selection_armed;
        let selection = self.selected;
        let wide = self.viewport.width >= 520.;
        let mut picked = None;
        let mut activated = None;
        let list = uniform_list(
            ui,
            &id,
            &mut self.list,
            ListOptions {
                overscan: OVERSCAN,
                ..ListOptions::new(self.viewport.width.max(1.), self.viewport.height.max(1.))
            },
            |ui, item| {
                let node = &nodes[item.index];
                let interaction = ui.get(&item.id);
                if interaction.pressed || interaction.clicked {
                    picked = Some(item.index);
                }
                if interaction.double_clicked {
                    activated = Some(item.index);
                }
                let active = armed && (item.active || selection == Some(item.index));
                let name = if node.name_truncated {
                    format!("{}…", node.display_name)
                } else {
                    node.display_name.clone()
                };
                let note = node_note(node);
                let mut cells = vec![
                    row([
                        entry_glyph(node.kind == EntryKind::Directory, active, muted()),
                        label(name.clone())
                            .when(active, |e| e.fill(selected_ink()))
                            .grow(1)
                            .basis(0)
                            .min_w(0)
                            .lines(1)
                            .clip(),
                    ])
                    .gap(8)
                    .align(Align::Center)
                    .grow(1)
                    .basis(0)
                    .min_w(0),
                ];
                if wide {
                    cells.push(
                        detail(note.clone())
                            .when(active, |e| e.fill(selected_ink()))
                            .reserve("Directory has payload · encrypted")
                            .shrink(0)
                            .min_w(0)
                            .lines(1)
                            .clip(),
                    );
                }
                let last = if !wide && (node.issue.is_some() || node.encrypted) {
                    node_short_note(node)
                } else {
                    node.uncompressed_bytes
                        .map(bytes)
                        .unwrap_or_else(|| "—".into())
                };
                cells.push(
                    detail(last)
                        .when(active, |e| e.fill(selected_ink()))
                        .reserve("999.9 GB")
                        .min_w(76),
                );
                row(cells)
                    .gap(10)
                    .pad((12, 0))
                    .h(ROW)
                    .w(pct(100))
                    .align(Align::Center)
                    .fill(if active {
                        selected()
                    } else if interaction.hovered {
                        color(0x293235)
                    } else {
                        ground()
                    })
                    .when(ui.focus_visible(&item.id), |e| {
                        e.stroke(super::cursor_stroke(active)).stroke_width(1)
                    })
                    .named(format!("{name} · {note}"))
                    .tip(node_details(node))
                    .clip()
            },
        );
        self.rows_built = list.changed.rows_built;
        self.visible = list.changed.visible;
        if let Some(index) = picked {
            self.selected = Some(index);
            self.selection_armed = true;
        } else if self.selection_armed {
            self.selected = list.changed.active.map(|i| i as usize).or(self.selected);
        }
        let activate = activated.or_else(|| {
            (self.selection_armed && enter)
                .then_some(list.changed.activated)
                .flatten()
                .map(|i| i as usize)
        });
        let empty = nodes.is_empty();
        let list_el = list
            .el
            .w(pct(100))
            .h(ROW)
            .grow(1)
            .basis(0)
            .shrink(1)
            .min_w(0)
            .min_h(0);
        if let Some(index) = activate {
            self.activate(index);
        }
        let content = if empty {
            let title = if self.source_changed() {
                "Archive source changed"
            } else if self.page.as_ref().is_some_and(|page| page.error.is_some()) {
                "Could not list this archive folder"
            } else if self.error.is_some() {
                "Archive unavailable"
            } else if self.pending || self.state() == BrowserCatalogState::Opening {
                "Loading archive metadata…"
            } else {
                match self.state() {
                    BrowserCatalogState::Cancelled => "Archive browsing cancelled",
                    BrowserCatalogState::Failed => "No available entries in this archive folder",
                    BrowserCatalogState::LimitReached => {
                        "No known entries in this partial archive folder"
                    }
                    BrowserCatalogState::Complete => "This archive folder is empty",
                    BrowserCatalogState::Opening => "Loading archive metadata…",
                }
            };
            col([
                label(title).wrap(),
                detail(if self.source_changed() {
                    "Reopen the archive to read its current metadata."
                } else if self.error.is_some()
                    || matches!(
                        self.state(),
                        BrowserCatalogState::Failed | BrowserCatalogState::LimitReached
                    )
                {
                    "Review the error above, reopen, or return to the folder."
                } else {
                    "Use Parent or Archive root to browse another logical folder."
                })
                .wrap(),
            ])
            .gap(7)
            .pad(14)
            .grow(1)
            .basis(0)
            .min_w(0)
            .min_h(0)
            .clip()
            // Keep the collection focus target mounted while an async page is pending/empty.
            // Header controls retain their own focus; loaded pages transfer only collection focus.
            .id(&id)
            .focusable()
            .a11y(A11y::Scroll)
        } else {
            let mut columns = vec![detail("Name").grow(1).basis(0).min_w(0)];
            if wide {
                columns.push(
                    detail("Details")
                        .reserve("Directory has payload · encrypted")
                        .shrink(0)
                        .min_w(0),
                );
            }
            columns.push(
                detail(if wide {
                    "Declared size"
                } else {
                    "Size / status"
                })
                .reserve("999.9 GB")
                .min_w(76),
            );
            col([
                row(columns).gap(10).pad((12, 5)).fill(chrome()).shrink(0),
                list_el,
            ])
            .grow(1)
            .basis(0)
            .min_w(0)
            .min_h(0)
        };
        col([content])
            .grow(1)
            .basis(0)
            .min_w(0)
            .min_h(0)
            .w(pct(100))
            .clip()
            .fill(ground())
            .id("archive-results")
    }

    pub(super) fn after_frame(&mut self, ui: &Ui) {
        if let Some(scene) = ui.scene() {
            if let Some(surface) = scene.surface("archive-panel") {
                self.geometry_changed |= self.window != surface.frame.size;
                self.window = surface.frame.size;
            }
            if let Some(surface) = scene.surface(&self.list_id()) {
                let size = surface.frame.size;
                if size.width >= 0. && size.height >= 0. {
                    self.geometry_changed |= self.viewport != size;
                    self.viewport = size;
                }
            }
        }
    }

    pub(super) fn evidence(&self) -> String {
        format!(
            "{{\"schema_version\":1,\"mode\":\"archive\",\"generation\":{},\"request_id\":{},\"catalog_state\":\"{:?}\",\"source_changed\":{},\"source_valid\":{},\"prefix_depth\":{},\"page_offset\":{},\"page_rows\":{},\"total_children\":{},\"indexed_records\":{},\"records_seen\":{},\"logical_directories\":{},\"page_issue_rows\":{},\"page_encrypted_rows\":{},\"rows_built\":{},\"visible_start\":{},\"visible_end\":{},\"viewport_width\":{},\"viewport_height\":{},\"window_width\":{},\"window_height\":{},\"page_limit\":{},\"read_only\":true,\"limitations\":[\"metadata browsing only; declared sizes and payload integrity unverified\",\"archive controls are experimental\"]}}",
            self.generation.map_or(0, BrowserGeneration::value),
            self.request_id,
            self.state(),
            self.source_changed(),
            self.page
                .as_ref()
                .is_some_and(|page| page.catalog.source_valid),
            self.prefix.components().len(),
            self.offset,
            self.page.as_ref().map_or(0, |p| p.nodes.len()),
            self.page.as_ref().map_or(0, |p| p.total_children),
            self.page.as_ref().map_or(0, |p| p.catalog.indexed_records),
            self.page.as_ref().map_or(0, |p| p.catalog.records_seen),
            self.page
                .as_ref()
                .map_or(0, |p| p.catalog.logical_directories),
            self.page.as_ref().map_or(0, |p| p
                .nodes
                .iter()
                .filter(|node| node.issue.is_some())
                .count()),
            self.page
                .as_ref()
                .map_or(0, |p| p.nodes.iter().filter(|node| node.encrypted).count()),
            self.rows_built,
            self.visible.start,
            self.visible.end,
            self.viewport.width,
            self.viewport.height,
            self.window.width,
            self.window.height,
            MAX_BROWSER_PAGE_SIZE
        )
    }
}

fn focus_matches(focus: &str, collection: &str) -> bool {
    focus == collection
        || focus
            .strip_prefix(collection)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
fn node_note(node: &BrowserNode) -> String {
    let mut note = if let Some(issue) = &node.issue {
        match issue.kind {
            BrowserIssueKind::UnsafeName => "Unsafe name".into(),
            BrowserIssueKind::AmbiguousName => "Ambiguous name".into(),
            BrowserIssueKind::UnsupportedCompression => "Unsupported method".into(),
            BrowserIssueKind::UnsupportedKind => "Unsupported type".into(),
            BrowserIssueKind::PayloadDirectory => "Directory has payload".into(),
        }
    } else {
        match node.kind {
            EntryKind::Directory if node.synthetic => "Folder · implicit".into(),
            EntryKind::Directory => "Folder".into(),
            EntryKind::Symlink => "Symbolic link".into(),
            EntryKind::Special => "Special entry".into(),
            EntryKind::File => match node.compression {
                Some(0) => "Stored".into(),
                Some(8) => "Deflate".into(),
                Some(method) => format!("Method {method}"),
                None => "File".into(),
            },
        }
    };
    if node.encrypted {
        note.push_str(" · encrypted");
    }
    note
}
fn node_short_note(node: &BrowserNode) -> String {
    match node.issue.as_ref().map(|issue| issue.kind) {
        Some(BrowserIssueKind::UnsafeName) => "Unsafe".into(),
        Some(BrowserIssueKind::AmbiguousName) => "Ambiguous".into(),
        Some(BrowserIssueKind::UnsupportedCompression | BrowserIssueKind::UnsupportedKind) => {
            "Unsupported".into()
        }
        Some(BrowserIssueKind::PayloadDirectory) => "Dir payload".into(),
        None if node.encrypted => "Encrypted".into(),
        None => node_note(node),
    }
}
fn node_details(node: &BrowserNode) -> String {
    let mut text = format!("{} · {}", node.display_name, node_note(node));
    if let Some(size) = node.uncompressed_bytes {
        text.push_str(&format!(" · declared size {size} bytes"));
    }
    if let Some(size) = node.compressed_bytes {
        text.push_str(&format!(" · compressed {size} bytes"));
    }
    if let Some(issue) = &node.issue {
        text.push_str(&format!(
            " · {}{}",
            issue.message,
            if issue.message_truncated {
                " [truncated]"
            } else {
                ""
            }
        ));
    }
    if node.name_truncated {
        text.push_str(" · display name truncated");
    }
    if node.navigation.is_some() {
        text.push_str(" · Enter opens this logical folder");
    } else {
        text.push_str(" · metadata only; file opening is unavailable");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn service_start_failure_offers_reopen_immediately() {
        // Relative sources fail before worker creation, exercising the same UI
        // error branch as a thread-spawn failure without exhausting host resources.
        let mut panel = ArchivePanel::new(PathBuf::from("relative.zip"));
        assert!(panel.browser.is_none());
        assert_eq!(panel.state(), BrowserCatalogState::Failed);
        let mut ui = super::super::make_ui().unwrap();
        for _ in 0..3 {
            let (tree, _) = panel.build(&mut ui);
            ui.frame(tree, Some(Size::new(640., 480.)), Input::default(), 0.016)
                .unwrap();
            panel.after_frame(&ui);
        }
        let scene = ui.scene().unwrap();
        assert!(scene.surface("archive-reopen").is_some());
        assert!(scene.surface("archive-cancel").is_none());
        assert!(panel.evidence().contains("\"catalog_state\":\"Failed\""));
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/archive-generated")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn zip(&self, records: &[Record]) -> PathBuf {
            let path = self.0.join("fixture.zip");
            fs::write(&path, zip_bytes(records)).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    struct Record {
        name: String,
        method: u16,
        flags: u16,
    }
    impl Record {
        fn file(name: impl Into<String>) -> Self {
            Self {
                name: name.into(),
                method: 0,
                flags: 0x800,
            }
        }
    }
    fn u16le(output: &mut Vec<u8>, value: u16) {
        output.extend(value.to_le_bytes());
    }
    fn u32le(output: &mut Vec<u8>, value: u32) {
        output.extend(value.to_le_bytes());
    }
    /// Empty-payload classic ZIP fixtures; independent header construction preserves invalid/duplicate names.
    fn zip_bytes(records: &[Record]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut offsets = Vec::new();
        for record in records {
            offsets.push(output.len() as u32);
            u32le(&mut output, 0x04034b50);
            u16le(&mut output, 20);
            u16le(&mut output, record.flags);
            u16le(&mut output, record.method);
            u16le(&mut output, 0);
            u16le(&mut output, 0x21);
            for _ in 0..3 {
                u32le(&mut output, 0);
            }
            u16le(&mut output, record.name.len() as u16);
            u16le(&mut output, 0);
            output.extend(record.name.as_bytes());
        }
        let central = output.len();
        for (record, offset) in records.iter().zip(offsets) {
            u32le(&mut output, 0x02014b50);
            u16le(&mut output, 0x0314);
            u16le(&mut output, 20);
            u16le(&mut output, record.flags);
            u16le(&mut output, record.method);
            u16le(&mut output, 0);
            u16le(&mut output, 0x21);
            for _ in 0..3 {
                u32le(&mut output, 0);
            }
            u16le(&mut output, record.name.len() as u16);
            for _ in 0..4 {
                u16le(&mut output, 0);
            }
            u32le(&mut output, 0o100644 << 16);
            u32le(&mut output, offset);
            output.extend(record.name.as_bytes());
        }
        let directory_bytes = output.len() - central;
        u32le(&mut output, 0x06054b50);
        u16le(&mut output, 0);
        u16le(&mut output, 0);
        u16le(&mut output, records.len() as u16);
        u16le(&mut output, records.len() as u16);
        u32le(&mut output, directory_bytes as u32);
        u32le(&mut output, central as u32);
        u16le(&mut output, 0);
        output
    }
    fn complete(panel: &mut ArchivePanel) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            panel.poll();
            if !panel.pending && panel.state() != BrowserCatalogState::Opening {
                return;
            }
            assert!(panel.error.is_none(), "{:?}", panel.error);
            assert!(Instant::now() < deadline, "{}", panel.evidence());
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn frame(
        panel: &mut ArchivePanel,
        ui: &mut Ui,
        size: Size,
        input: Input,
    ) -> Option<ArchiveAction> {
        let (tree, action) = panel.build(ui);
        ui.frame(tree, Some(size), input, 0.016).unwrap();
        panel.after_frame(ui);
        action
    }
    fn key(key: Key) -> Input {
        Input {
            keys: vec![KeyPress {
                key,
                mods: Mods::default(),
            }],
            ..Input::default()
        }
    }
    fn click(panel: &mut ArchivePanel, ui: &mut Ui, size: Size, id: &str) {
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
        frame(panel, ui, size, Input::default());
    }

    #[test]
    fn implicit_folders_and_invalid_records_use_typed_navigation_only() {
        let fixture = Fixture::new();
        let source = fixture.zip(&[
            Record::file("dir/nested/file"),
            Record::file("dir/leaf"),
            Record::file("../unsafe"),
            Record::file("same"),
            Record::file("same"),
        ]);
        let mut panel = ArchivePanel::new(source.clone());
        complete(&mut panel);
        assert_eq!(panel.state(), BrowserCatalogState::Complete);
        let page = panel.page.as_ref().unwrap();
        assert_eq!(page.source.as_ref(), &source);
        assert_eq!(page.catalog.indexed_records, 5);
        let directory = page
            .nodes
            .iter()
            .position(|node| node.display_name == "dir")
            .unwrap();
        let unsafe_row = page
            .nodes
            .iter()
            .position(|node| {
                node.issue
                    .as_ref()
                    .is_some_and(|issue| issue.kind == BrowserIssueKind::UnsafeName)
            })
            .unwrap();
        assert!(page.nodes[unsafe_row].navigation.is_none());
        let id = panel.request_id;
        panel.activate(unsafe_row);
        assert_eq!(panel.request_id, id);
        panel.activate(directory);
        complete(&mut panel);
        assert_eq!(panel.prefix.components(), &["dir"]);
        assert!(
            panel
                .page
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .any(|node| node.synthetic && node.display_name == "nested")
        );
        let old = panel.page.as_ref().unwrap().clone();
        panel.navigate(ArchivePrefix::root());
        complete(&mut panel);
        assert!(!panel.apply(old));
        assert!(panel.prefix.components().is_empty());
        assert!(panel.selected.is_none());
    }

    #[test]
    fn byte_limited_pages_follow_real_next_offsets_and_previous_retraces_actual_ranges() {
        let fixture = Fixture::new();
        // Long logical directory prefixes make the byte cap bind before the 512-row cap.
        let records = (0..400)
            .map(|i| Record::file(format!("dir-{i:04}-{}{}", "x".repeat(2_048), "/leaf")))
            .collect::<Vec<_>>();
        let mut panel = ArchivePanel::new(fixture.zip(&records));
        complete(&mut panel);
        let first = panel.page.as_ref().unwrap();
        assert_eq!(first.total_children, 400);
        let next = first.next_offset.expect("byte-limited continuation");
        assert!(next < MAX_BROWSER_PAGE_SIZE && next == first.nodes.len());
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(640., 480.);
        for _ in 0..4 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        click(&mut panel, &mut ui, size, "archive-next");
        complete(&mut panel);
        assert_eq!(panel.offset, next);
        let second = panel.page.as_ref().unwrap();
        assert_eq!(second.offset, next);
        assert_eq!(next + second.nodes.len(), 400);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        click(&mut panel, &mut ui, size, "archive-previous");
        complete(&mut panel);
        assert_eq!(panel.offset, 0);
        assert!(panel.previous_offsets.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn native_source_bytes_and_flagged_metadata_survive_without_host_member_paths() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let source = fixture
            .0
            .join(std::ffi::OsString::from_vec(b"source-\xff.zip".to_vec()));
        let records = [
            Record {
                name: "encrypted".into(),
                method: 0,
                flags: 0x801,
            },
            Record {
                name: "unsupported".into(),
                method: 93,
                flags: 0x800,
            },
        ];
        fs::write(&source, zip_bytes(&records)).unwrap();
        let mut panel = ArchivePanel::new(source.clone());
        complete(&mut panel);
        assert_eq!(panel.source, source);
        let page = panel.page.as_ref().unwrap();
        assert_eq!(page.source.as_ref(), &source);
        let encrypted = page.nodes.iter().find(|node| node.encrypted).unwrap();
        assert!(node_short_note(encrypted).contains("Encrypted"));
        let unsupported = page
            .nodes
            .iter()
            .find(|node| {
                node.issue
                    .as_ref()
                    .is_some_and(|issue| issue.kind == BrowserIssueKind::UnsupportedCompression)
            })
            .unwrap();
        assert_eq!(node_short_note(unsupported), "Unsupported");
        assert!(page.nodes.iter().all(|node| node.navigation.is_none()));
        let id = panel.request_id;
        panel.activate(0);
        assert_eq!(panel.request_id, id);
        assert_eq!(
            fs::read_dir(&fixture.0).unwrap().count(),
            1,
            "browsing must not materialize members"
        );
    }

    #[test]
    fn intrinsic_controls_and_virtual_rows_fit_640_and_320() {
        let fixture = Fixture::new();
        let records = (0..100)
            .map(|i| Record::file(format!("file-{i:03}")))
            .collect::<Vec<_>>();
        let mut panel = ArchivePanel::new(fixture.zip(&records));
        complete(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        for size in [Size::new(640., 480.), Size::new(320., 240.)] {
            for _ in 0..5 {
                frame(&mut panel, &mut ui, size, Input::default());
            }
            let scene = ui.scene().unwrap();
            for id in [
                "archive-panel",
                "archive-go-to",
                "archive-prefix",
                "archive-results",
                "archive-next",
                "archive-previous",
            ] {
                let rect = scene
                    .surface(id)
                    .unwrap_or_else(|| panic!("missing {id}"))
                    .frame;
                assert!(
                    rect.x >= -0.1
                        && rect.y >= -0.1
                        && rect.right() <= size.width + 0.1
                        && rect.bottom() <= size.height + 0.1,
                    "{id}: {rect:?} in {size:?}"
                );
            }
            assert!(panel.viewport.height > 0., "{}", panel.evidence());
            assert!(
                panel.rows_built <= (panel.viewport.height / ROW).ceil() as usize + 6,
                "{}",
                panel.evidence()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_copy_status_leaves_short_archive_controls_and_visible_rows_inside_window() {
        use mui::host::View;
        let fixture = Fixture::new();
        let source = fixture.zip(&[Record::file("member")]);
        let input = fixture.0.join("copied-file");
        let destination = fixture.0.join("destination");
        fs::write(&input, b"copy payload").unwrap();
        fs::create_dir(&destination).unwrap();
        let mut transfers = crate::transfers::TransfersUi::new();
        assert!(transfers.start(vec![input], destination));
        let until = Instant::now() + Duration::from_secs(10);
        while !transfers.summary().contains("1 item published") {
            assert!(Instant::now() < until);
            transfers.poll();
            thread::sleep(Duration::from_millis(1));
        }
        transfers.hide();
        let mut view = crate::BrowserView::fixture(0);
        let mut panel = ArchivePanel::new(source);
        complete(&mut panel);
        view.archive = Some(panel);
        view.copy_transfers = Some(transfers);
        let mut ui = crate::make_ui().unwrap();
        for size in [
            Size::new(1024., 720.),
            Size::new(320., 240.),
            Size::new(640., 480.),
        ] {
            for _ in 0..8 {
                let tree = view.build(&mut ui, &Input::default());
                ui.frame(tree, Some(size), Input::default(), 0.016).unwrap();
                view.after_frame(&mut ui);
            }
            let panel = view.archive.as_ref().unwrap();
            let scene = ui.scene().unwrap();
            for id in [
                "archive-close",
                "archive-go-to",
                "archive-next",
                "archive-previous",
                "archive-status",
                view.copy_transfers
                    .as_ref()
                    .unwrap()
                    .action_id("show")
                    .as_str(),
            ] {
                let rect = scene.surface(id).unwrap().frame;
                assert!(
                    rect.x >= -0.1
                        && rect.y >= -0.1
                        && rect.right() <= size.width + 0.1
                        && rect.bottom() <= size.height + 0.1,
                    "{id}: {rect:?} in {size:?}"
                );
            }
            assert!(
                panel.viewport.height >= ROW,
                "at least one row must be visible, not just built by overscan: {}",
                panel.evidence()
            );
            let list = scene.surface(&panel.list_id()).unwrap().frame;
            assert!(list.bottom() <= size.height + 0.1);
            assert!(
                list.size.width <= size.width + 0.1,
                "narrow width must update even after a zero-height frame"
            );
            assert_eq!(
                scene.surface("archive-source").is_some(),
                size.height >= 300.
            );
        }
    }

    #[test]
    fn keyboard_enters_only_directories_and_escape_or_ctrl_l_returns_to_host() {
        let fixture = Fixture::new();
        let mut panel = ArchivePanel::new(fixture.zip(&[Record::file("dir/leaf")]));
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(640., 480.);
        // Hold delivery, not the worker: deterministically render a real Opening placeholder
        // before applying its asynchronous completed page, regardless of machine speed.
        let browser = panel.browser.take();
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        assert!(
            ui.focus_key()
                .is_some_and(|focus| focus_matches(focus, &panel.list_id()))
        );
        panel.browser = browser;
        complete(&mut panel);
        for _ in 0..4 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        frame(&mut panel, &mut ui, size, key(Key::Home));
        frame(&mut panel, &mut ui, size, Input::default());
        assert_eq!(panel.selected, Some(0));
        frame(&mut panel, &mut ui, size, key(Key::Enter));
        frame(&mut panel, &mut ui, size, Input::default());
        assert!(
            panel.pending,
            "directory Enter must request a new logical folder"
        );
        let browser = panel.browser.take();
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        assert!(
            ui.focus_key()
                .is_some_and(|focus| focus_matches(focus, &panel.list_id()))
        );
        panel.browser = browser;
        complete(&mut panel);
        assert_eq!(panel.prefix.components(), &["dir"]);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        frame(&mut panel, &mut ui, size, key(Key::Home));
        frame(&mut panel, &mut ui, size, Input::default());
        let id = panel.request_id;
        frame(&mut panel, &mut ui, size, key(Key::Enter));
        frame(&mut panel, &mut ui, size, Input::default());
        assert_eq!(
            panel.request_id, id,
            "file Enter must not execute or open a member"
        );
        frame(&mut panel, &mut ui, size, key(Key::Escape));
        assert!(matches!(
            frame(&mut panel, &mut ui, size, Input::default()),
            Some(ArchiveAction::Close)
        ));
        frame(
            &mut panel,
            &mut ui,
            size,
            Input {
                keys: vec![KeyPress {
                    key: Key::Char('l'),
                    mods: Mods {
                        ctrl: true,
                        ..Mods::default()
                    },
                }],
                ..Input::default()
            },
        );
        assert!(matches!(
            frame(&mut panel, &mut ui, size, Input::default()),
            Some(ArchiveAction::GoTo)
        ));
    }

    #[test]
    fn async_publication_preserves_header_button_focus() {
        let fixture = Fixture::new();
        let mut panel = ArchivePanel::new(fixture.zip(&[Record::file("leaf")]));
        let browser = panel.browser.take();
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(320., 240.);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        ui.focus("archive-close");
        frame(&mut panel, &mut ui, size, Input::default());
        panel.browser = browser;
        complete(&mut panel);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        assert_eq!(ui.focus_key(), Some("archive-close"));
    }

    #[test]
    fn source_change_is_distinct_from_an_empty_complete_archive() {
        let fixture = Fixture::new();
        let source = fixture.zip(&[]);
        let mut panel = ArchivePanel::new(source.clone());
        complete(&mut panel);
        assert_eq!(panel.state(), BrowserCatalogState::Complete);
        assert_eq!(panel.page.as_ref().unwrap().total_children, 0);
        fs::write(&source, zip_bytes(&[Record::file("changed-source")])).unwrap();
        assert!(panel.request(ArchivePrefix::root(), 0));
        complete(&mut panel);
        assert!(panel.source_changed());
        assert!(!panel.page.as_ref().unwrap().catalog.source_valid);
        assert!(panel.status().contains("source changed"));
    }

    #[test]
    fn cancellation_waits_for_worker_and_reopen_resets_generation_and_prefix() {
        let fixture = Fixture::new();
        let mut panel = ArchivePanel::new(fixture.zip(&[Record::file("dir/leaf")]));
        complete(&mut panel);
        let original = panel.generation;
        panel.activate(0);
        complete(&mut panel);
        assert_eq!(panel.prefix.components(), &["dir"]);
        panel.cancel();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !panel.worker_stopped || panel.state() != BrowserCatalogState::Cancelled {
            panel.poll();
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(panel.status().contains("cancelled"));
        panel.reopen();
        complete(&mut panel);
        assert_ne!(panel.generation, original);
        assert!(panel.prefix.components().is_empty());
        assert!(!panel.cancel_requested);
        assert_eq!(panel.state(), BrowserCatalogState::Complete);
    }
    #[test]
    fn extraction_actions_keep_typed_folder_record_and_whole_archive_meanings() {
        let fixture = Fixture::new();
        let mut panel =
            ArchivePanel::new(fixture.zip(&[Record::file("implicit/a"), Record::file("root.bin")]));
        complete(&mut panel);
        let page = panel.page.as_ref().unwrap();
        let generation = page.generation;
        let folder_index = page.nodes.iter().position(|n| n.synthetic).unwrap();
        let file_index = page
            .nodes
            .iter()
            .position(|n| n.kind == EntryKind::File)
            .unwrap();
        let file_id = page.nodes[file_index].record_id.unwrap();
        panel.selected = Some(folder_index);
        panel.selection_armed = true;
        match panel.selected_extraction().unwrap() {
            ExtractionSelection::Items {
                generation: actual,
                items,
            } => {
                assert_eq!(actual, generation);
                assert!(
                    matches!(items.as_slice(), [ExtractionItem::Folder(prefix)] if prefix.components() == ["implicit"])
                );
            }
            _ => panic!("selected logical directory became All"),
        }
        panel.selected = Some(file_index);
        assert!(
            matches!(panel.selected_extraction(), Some(ExtractionSelection::Items { items, .. }) if matches!(items.as_slice(), [ExtractionItem::Record(id)] if *id == file_id))
        );
        assert!(
            matches!(panel.folder_extraction(), Some(ExtractionSelection::Items { items, .. }) if matches!(items.as_slice(), [ExtractionItem::Folder(prefix)] if prefix.components().is_empty()))
        );
        let mut ui = super::super::make_ui().unwrap();
        for size in [Size::new(640., 480.), Size::new(320., 240.)] {
            for _ in 0..3 {
                frame(&mut panel, &mut ui, size, Input::default());
            }
            super::super::extraction::capture_evidence(&ui, size, "archive-actions");
            for id in [
                panel.extraction_id("selected"),
                panel.extraction_id("folder"),
                panel.extraction_id("all"),
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
        }
        panel.set_mutations_busy(true);
        assert!(!panel.can_extract());
        assert!(panel.selected_extraction().is_none());
        assert!(panel.folder_extraction().is_none());
    }
    #[test]
    fn old_extract_button_press_cannot_retarget_a_replaced_archive_panel() {
        let first = Fixture::new();
        let second = Fixture::new();
        let mut panel = ArchivePanel::new(first.zip(&[Record::file("one")]));
        complete(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(640., 480.);
        for _ in 0..3 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        let old_id = panel.extraction_id("all");
        let rect = ui.scene().unwrap().surface(&old_id).unwrap().frame;
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
        panel = ArchivePanel::new(second.zip(&[Record::file("two")]));
        complete(&mut panel);
        assert_ne!(panel.extraction_id("all"), old_id);
        for input in [
            Input::from(PointerInput {
                pos: Some(point),
                ..PointerInput::default()
            }),
            Input::default(),
            Input::default(),
        ] {
            assert!(!matches!(
                frame(&mut panel, &mut ui, size, input),
                Some(ArchiveAction::Extract(_))
            ));
        }
    }
}
