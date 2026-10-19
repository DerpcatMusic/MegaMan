//! Explicit-scope filename finding. Paged controls use explicit scope and complete result counts.
use super::{
    Glyph, OVERSCAN, ROW, action, chrome, clicked, color, detail, entry_glyph, glyph, ground,
    hairline, label, muted, selected, selected_ink,
};
use browser_search::{
    Coverage, EntryKind, IndexOptions, MAX_SESSION_PAGE_SIZE, Query, SearchSession,
    SessionDelivery, SessionPage,
};
use mui::prelude::*;
use std::{ops::Range, path::PathBuf};

pub(super) enum FindAction {
    Close,
    Open { path: PathBuf, kind: EntryKind },
}

pub(super) struct FindPanel {
    session: Option<SearchSession>,
    scope_native: PathBuf,
    scope_text: String,
    query: String,
    request_id: u64,
    offset: usize,
    page: Option<Box<SessionPage>>,
    pending: bool,
    cancelled: bool,
    error: Option<String>,
    selected: Option<usize>,
    selection_armed: bool,
    list: ListState,
    row_epoch: u64,
    rows_built: usize,
    visible: Range<usize>,
    viewport: Size,
    window: Size,
    geometry_changed: bool,
    initial_focus: bool,
}

impl FindPanel {
    pub(super) fn new(root: PathBuf) -> Self {
        let (session, error) = match SearchSession::new() {
            Ok(session) => (Some(session), None),
            Err(error) => (None, Some(format!("Could not start search: {error}"))),
        };
        let mut panel = Self {
            session,
            scope_text: root.to_string_lossy().into_owned(),
            scope_native: root,
            query: String::new(),
            request_id: 0,
            offset: 0,
            page: None,
            pending: false,
            cancelled: false,
            error,
            selected: None,
            selection_armed: false,
            list: ListState::uniform_count(0, ROW),
            row_epoch: 0,
            rows_built: 0,
            visible: 0..0,
            viewport: Size::new(320., 100.),
            window: Size::new(320., 240.),
            geometry_changed: false,
            initial_focus: true,
        };
        if panel.session.is_some() {
            panel.restart();
        }
        panel
    }

    fn next_query(&mut self) -> Option<Query> {
        let Some(id) = self.request_id.checked_add(1) else {
            self.error = Some("Close and reopen Find Anywhere to start a new search.".into());
            return None;
        };
        self.request_id = id;
        Some(Query {
            request_id: id,
            text: self.query.clone(),
            offset: self.offset,
            limit: MAX_SESSION_PAGE_SIZE,
            ..Query::default()
        })
    }

    fn reset_rows(&mut self) {
        self.selected = None;
        self.selection_armed = false;
        self.row_epoch = self.row_epoch.wrapping_add(1);
        self.list =
            ListState::uniform_count(self.page.as_ref().map_or(0, |p| p.page.entries.len()), ROW);
        self.rows_built = 0;
        self.visible = 0..0;
    }

    fn restart(&mut self) {
        // Unchanged display text preserves undecodable native scope bytes.
        let root = if self.scope_text == self.scope_native.to_string_lossy() {
            self.scope_native.clone()
        } else {
            PathBuf::from(&self.scope_text)
        };
        if !root.is_absolute() {
            self.error = Some("Enter an absolute folder path, then choose Scan.".into());
            return;
        }
        if self.session.is_none() {
            match SearchSession::new() {
                Ok(session) => self.session = Some(session),
                Err(error) => {
                    self.error = Some(format!("Could not start search: {error}"));
                    return;
                }
            }
        }
        self.offset = 0;
        let Some(query) = self.next_query() else {
            return;
        };
        match self
            .session
            .as_ref()
            .expect("session exists")
            .start_scope(IndexOptions::new(vec![root.clone()]), query)
        {
            Ok(()) => {
                self.scope_native = root;
                self.scope_text = self.scope_native.to_string_lossy().into_owned();
                self.page = None;
                self.pending = true;
                self.cancelled = false;
                self.error = None;
                self.reset_rows();
            }
            Err(error) => self.error = Some(format!("Could not start this scope: {error}")),
        }
    }

    fn submit(&mut self, offset: usize) {
        self.offset = offset;
        self.page = None;
        self.reset_rows();
        if self.cancelled {
            return;
        }
        let Some(query) = self.next_query() else {
            return;
        };
        let result = self.session.as_ref().map(|session| session.submit(query));
        match result {
            Some(Ok(())) => {
                self.pending = true;
                self.error = None;
            }
            Some(Err(error)) => {
                self.pending = false;
                self.error = Some(format!("Could not search: {error}"));
            }
            None => {
                self.pending = false;
                self.error = Some("Search is unavailable. Choose Restart to try again.".into());
            }
        }
    }

    fn stop(&mut self) {
        if let Some(session) = &self.session {
            session.cancel();
        }
        self.pending = false;
        self.cancelled = true;
        self.reset_rows();
    }

    fn apply(&mut self, delivery: SessionDelivery) -> bool {
        if delivery.request_id() != self.request_id {
            return false;
        }
        match delivery {
            SessionDelivery::Page(page) if !self.cancelled => {
                self.offset = page.page.offset;
                self.page = Some(page);
                self.pending = false;
                self.error = None;
                // Every publication may reorder rows; old row indices/focus must not activate new paths.
                self.reset_rows();
            }
            SessionDelivery::Cancelled { .. } => {
                self.cancelled = true;
                self.pending = false;
                self.reset_rows();
            }
            _ => return false,
        }
        true
    }

    pub(super) fn poll(&mut self) -> bool {
        let delivery = self.session.as_ref().and_then(SearchSession::poll);
        let changed = delivery.is_some_and(|delivery| self.apply(delivery));
        changed || std::mem::take(&mut self.geometry_changed)
    }

    fn coverage(&self) -> Coverage {
        if self.cancelled {
            Coverage::Cancelled
        } else if self.pending {
            Coverage::Indexing
        } else {
            self.page
                .as_ref()
                .map_or(Coverage::Idle, |p| p.page.coverage)
        }
    }

    fn list_id(&self) -> String {
        format!("find-page-{}", self.row_epoch)
    }

    fn open(&self, index: usize) -> Option<FindAction> {
        let entry = self.page.as_ref()?.page.entries.get(index)?;
        Some(FindAction::Open {
            path: entry.path.clone(),
            kind: entry.kind,
        })
    }

    fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let count = self.page.as_ref().map_or(0, |p| p.indexed_entries);
        match self.coverage() {
            Coverage::Idle => "Choose Scan to search this folder and its subfolders.".into(),
            Coverage::Indexing if self.pending => "Waiting for this search…".into(),
            Coverage::Indexing => format!("Scanning · {count} items indexed · results so far"),
            Coverage::Complete => format!("Scope complete · {count} items indexed"),
            Coverage::Partial => format!("Incomplete scope · {count} items indexed · see errors"),
            Coverage::Cancelled => "Search cancelled · Restart to scan this scope again".into(),
        }
    }

    fn policy(&self) -> String {
        if self.scope_text != self.scope_native.to_string_lossy() {
            return "Scope edited · choose Scan to apply it".into();
        }
        if let Some(page) = &self.page {
            format!(
                "{} excluded scopes · {} links not followed · {} errors",
                page.excluded_roots.len(),
                page.symlinks_not_followed,
                page.error_count
            )
        } else if cfg!(target_os = "linux") {
            "Excludes /proc, /sys, /dev · links are not followed".into()
        } else {
            "Folder and subfolders · links are not followed".into()
        }
    }

    fn policy_details(&self) -> String {
        let exclusions = self.page.as_ref().map_or_else(
            || IndexOptions::new(Vec::new()).excluded_roots,
            |page| page.excluded_roots.to_vec(),
        );
        let excluded = if exclusions.is_empty() {
            "None".into()
        } else {
            exclusions
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "Excluded folders: {excluded}. Hidden filenames are included. Symlink entries are shown; linked folders are not scanned. Matches a literal filename substring, ignoring case with Unicode 16 default case folding; text is not normalized."
        )
    }

    pub(super) fn build(&mut self, ui: &mut Ui) -> (El, Option<FindAction>) {
        self.poll();
        if self.initial_focus {
            ui.focus("find-query");
            self.initial_focus = false;
        }
        let focus = ui.focus_key().map(str::to_owned);
        let mut keys = ui.shortcuts().to_vec();
        if ui.focus_is_text() {
            keys.extend(
                ui.keys(focus.as_deref().unwrap_or("find-query"))
                    .iter()
                    .filter(|p| {
                        p.key == Key::Escape
                            || ((p.mods.ctrl || p.mods.cmd)
                                && matches!(p.key, Key::Char('l' | 'L' | 'f' | 'F')))
                    })
                    .copied(),
            );
        }
        let mut outcome = None;
        for key in keys {
            if key.key == Key::Escape {
                outcome = Some(FindAction::Close);
            }
            if key.mods.ctrl || key.mods.cmd {
                if matches!(key.key, Key::Char('l' | 'L')) {
                    ui.focus("find-scope");
                }
                if matches!(key.key, Key::Char('f' | 'F')) {
                    ui.focus("find-query");
                }
            }
        }
        if clicked(ui, "find-back") {
            outcome = Some(FindAction::Close);
        }
        if clicked(ui, "find-stop") {
            self.stop();
        }
        if clicked(ui, "find-restart") {
            self.restart();
        }

        let scope = text_edit(ui, "find-scope", &mut self.scope_text, TextOpts::default());
        if scope.changed.submitted || clicked(ui, "find-scan") {
            self.restart();
            ui.focus("find-query");
        }
        let scope = scope
            .el
            .grow(1)
            .basis(0)
            .min_w(0)
            .fill(ground())
            .named("Search scope · absolute folder path · Enter to scan · Ctrl+L")
            .tip("Search this folder and its subfolders. Editing applies when you choose Scan.");
        let query = text_edit(ui, "find-query", &mut self.query, TextOpts::default());
        if query.changed.changed {
            self.submit(0);
        }
        if query.changed.submitted {
            ui.focus(self.list_id());
        }
        let query = query
            .el
            .grow(1)
            .basis(0)
            .min_w(0)
            .fill(ground())
            .named("Filename contains · Ctrl+F · Enter to focus results")
            .tip("Match a literal filename substring, ignoring case. An empty name shows every indexed item.");

        let can_previous =
            !self.cancelled && !self.pending && self.offset > 0 && self.page.is_some();
        let can_next = !self.cancelled
            && !self.pending
            && self.page.as_ref().is_some_and(|p| {
                self.offset.saturating_add(p.page.entries.len()) < p.page.total_matches
            });
        if can_previous && clicked(ui, "find-previous") {
            self.submit(self.offset.saturating_sub(MAX_SESSION_PAGE_SIZE));
        }
        if can_next && clicked(ui, "find-next") {
            self.submit(self.offset.saturating_add(MAX_SESSION_PAGE_SIZE));
        }
        let results = self.results(ui, &mut outcome);
        let count_text = self.page.as_ref().map_or_else(
            || "No result page yet".into(),
            |p| {
                let count = p.page.entries.len();
                let start = if count == 0 {
                    0
                } else {
                    self.offset.saturating_add(1)
                };
                let end = self.offset.saturating_add(count);
                let suffix = if self.cancelled {
                    " · frozen"
                } else if p.page.coverage == Coverage::Indexing {
                    " so far"
                } else {
                    ""
                };
                format!("Rows {start}–{end} of {}{suffix}", p.page.total_matches)
            },
        );
        let footer = row([
            action(
                ui,
                "find-previous",
                "Previous result page",
                glyph(Glyph::Back, muted()),
                can_previous,
                false,
            ),
            detail(count_text).grow(1).basis(0).min_w(0).clip(),
            action(
                ui,
                "find-next",
                "Next result page",
                glyph(Glyph::Forward, muted()),
                can_next,
                false,
            ),
        ])
        .gap(6)
        .pad((8, 4))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0);
        let working = matches!(self.coverage(), Coverage::Indexing);
        let header = row([
            action(
                ui,
                "find-back",
                "Back to folder · Escape",
                glyph(Glyph::Back, muted()),
                true,
                false,
            ),
            label("Find anywhere").grow(1).basis(0).min_w(0).clip(),
            if working {
                action(
                    ui,
                    "find-stop",
                    "Cancel search",
                    label("Cancel"),
                    true,
                    false,
                )
            } else {
                action(
                    ui,
                    "find-restart",
                    "Restart search in this scope",
                    label("Restart"),
                    true,
                    false,
                )
            },
        ])
        .gap(6)
        .pad((6, 4))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0);
        let scope_row = row([
            detail("Scope").reserve("Name").shrink(0),
            scope,
            action(
                ui,
                "find-scan",
                "Scan the entered folder and its subfolders",
                label("Scan"),
                true,
                false,
            ),
        ])
        .gap(7)
        .pad((8, 3))
        .align(Align::Center)
        .fill(chrome())
        .shrink(0);
        let query_row = row([detail("Name").reserve("Scope").shrink(0), query])
            .gap(7)
            .pad((8, 3))
            .align(Align::Center)
            .fill(chrome())
            .shrink(0);
        let status = self.status();
        let policy = self.policy();
        let diagnostic = self
            .page
            .as_ref()
            .and_then(|p| p.diagnostics.first())
            .map(|d| format!("{}: {}", d.path.to_string_lossy(), d.message));
        let mut summaries = vec![
            detail(status.clone())
                .when(
                    self.error.is_some() || self.coverage() == Coverage::Partial,
                    |e| e.fill(color(0xffb4ab)),
                )
                .lines(1)
                .min_w(0)
                .clip()
                .tip(diagnostic.clone().unwrap_or(status)),
            detail(policy)
                .lines(1)
                .min_w(0)
                .clip()
                .tip(self.policy_details()),
        ];
        if let Some(diagnostic) = diagnostic {
            summaries.push(
                detail(diagnostic.clone())
                    .fill(color(0xffb4ab))
                    .lines(1)
                    .min_w(0)
                    .clip()
                    .tip(diagnostic),
            );
        }
        let summary = col(summaries)
            .gap(2)
            .pad((8, 4))
            .min_w(0)
            .fill(chrome())
            .shrink(0)
            .id("find-status");
        (
            col([
                header,
                scope_row,
                query_row,
                summary,
                hairline(),
                results,
                hairline(),
                footer,
            ])
            .w(pct(100))
            .h(pct(100))
            .min_w(0)
            .min_h(0)
            .fill(ground())
            .clip()
            .id("find-panel")
            .named("Find filenames within the explicit search scope"),
            outcome,
        )
    }

    fn results(&mut self, ui: &mut Ui, outcome: &mut Option<FindAction>) -> El {
        let list_id = self.list_id();
        let results_focused = ui
            .focus_key()
            .is_some_and(|id| id.starts_with("find-page-"));
        if results_focused {
            // Transfer focus to a fresh collection, but never restore the previous row identity.
            if !ui
                .focus_key()
                .is_some_and(|id| collection_focus(id, &list_id))
            {
                ui.focus(&list_id);
            }
            if ui
                .keys(ui.focus_key().unwrap_or(&list_id))
                .iter()
                .any(|key| {
                    !key.mods.ctrl
                        && !key.mods.cmd
                        && !key.mods.alt
                        && matches!(
                            key.key,
                            Key::Up
                                | Key::Down
                                | Key::Home
                                | Key::End
                                | Key::PageUp
                                | Key::PageDown
                        )
                })
            {
                self.selection_armed = true;
            }
        }
        let enter_pressed = results_focused
            && ui
                .keys(ui.focus_key().unwrap_or(&list_id))
                .iter()
                .any(|key| {
                    key.key == Key::Enter && !key.mods.ctrl && !key.mods.cmd && !key.mods.alt
                });
        let entries = self
            .page
            .as_ref()
            .map(|page| page.page.entries.as_slice())
            .unwrap_or(&[]);
        let selection = self.selected;
        let armed = self.selection_armed;
        let mut picked = None;
        let mut activated = None;
        let list = uniform_list(
            ui,
            &list_id,
            &mut self.list,
            ListOptions {
                overscan: OVERSCAN,
                ..ListOptions::new(self.viewport.width.max(1.), self.viewport.height.max(1.))
            },
            |ui, item| {
                let entry = &entries[item.index];
                let interaction = ui.get(&item.id);
                if interaction.clicked || interaction.pressed {
                    picked = Some(item.index);
                }
                if interaction.double_clicked {
                    activated = Some(item.index);
                }
                let active = armed && (item.active || selection == Some(item.index));
                let path = entry
                    .path
                    .parent()
                    .unwrap_or(&entry.path)
                    .to_string_lossy()
                    .into_owned();
                row([
                    row([
                        entry_glyph(entry.kind == EntryKind::Directory, active, muted()),
                        label(entry.display_name.clone())
                            .when(active, |e| e.fill(selected_ink()))
                            .grow(1)
                            .basis(0)
                            .min_w(0)
                            .lines(1)
                            .clip(),
                    ])
                    .gap(8)
                    .align(Align::Center)
                    .grow(2)
                    .basis(0)
                    .min_w(0),
                    detail(path)
                        .when(active, |e| e.fill(selected_ink()))
                        .grow(3)
                        .basis(0)
                        .min_w(0)
                        .lines(1)
                        .clip(),
                ])
                .gap(12)
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
                .named(format!(
                    "{} · {}",
                    entry.display_name,
                    entry.path.to_string_lossy()
                ))
                .tip(entry.path.to_string_lossy().into_owned())
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
        if let Some(index) = activated.or_else(|| {
            (self.selection_armed && enter_pressed)
                .then_some(list.changed.activated)
                .flatten()
                .map(|i| i as usize)
        }) {
            *outcome = self.open(index);
        }
        let content = if entries.is_empty() {
            let message = if self.error.is_some() {
                "Search could not run"
            } else if self.cancelled {
                "Search stopped"
            } else if self.pending {
                "Waiting for results…"
            } else {
                match self.coverage() {
                    Coverage::Indexing => "No matches yet · scanning continues",
                    Coverage::Partial => "No matches in the available results",
                    Coverage::Complete => "No matching filenames in this scope",
                    _ => "Choose Scan to begin",
                }
            };
            col([
                label(message).wrap(),
                detail(if self.cancelled {
                    "Choose Restart to search this scope again."
                } else if self.coverage() == Coverage::Partial || self.error.is_some() {
                    "Check the scope or error details, then scan again."
                } else {
                    "Change the name filter or choose another scope."
                })
                .wrap(),
            ])
            .gap(7)
            .pad(14)
            .min_w(0)
            .min_h(0)
            .grow(1)
            .basis(0)
            .clip()
        } else {
            let header = row([
                detail("Name").grow(2).basis(0).min_w(0),
                detail("Folder").grow(3).basis(0).min_w(0),
            ])
            .gap(12)
            .pad((12, 5))
            .fill(chrome())
            .shrink(0);
            col([
                header,
                list.el
                    .w(pct(100))
                    .h(ROW)
                    .grow(1)
                    .basis(0)
                    .shrink(1)
                    .min_w(0)
                    .min_h(0),
            ])
            .min_w(0)
            .min_h(0)
            .grow(1)
            .basis(0)
        };
        col([content])
            .grow(1)
            .basis(0)
            .min_w(0)
            .min_h(0)
            .w(pct(100))
            .clip()
            .fill(ground())
            .id("find-results")
    }

    pub(super) fn after_frame(&mut self, ui: &Ui) {
        if let Some(scene) = ui.scene() {
            if let Some(frame) = scene.surface("find-panel") {
                self.geometry_changed |= self.window != frame.frame.size;
                self.window = frame.frame.size;
            }
            if let Some(frame) = scene.surface(&self.list_id()) {
                let size = frame.frame.size;
                if size.width > 0. && size.height > 0. {
                    self.geometry_changed |= self.viewport != size;
                    self.viewport = size;
                }
            }
        }
    }

    pub(super) fn evidence(&self) -> String {
        format!(
            "{{\"mode\":\"find-anywhere\",\"request_id\":{},\"coverage\":\"{:?}\",\"page_offset\":{},\"page_rows\":{},\"total_matches\":{},\"indexed_entries\":{},\"rows_built\":{},\"visible_start\":{},\"visible_end\":{},\"window_width\":{},\"window_height\":{},\"viewport_width\":{},\"viewport_height\":{},\"page_limit\":{},\"limitations\":[\"paged search controls are experimental\",\"best-effort explicit scope; not a whole-machine completeness claim\"]}}",
            self.request_id,
            self.coverage(),
            self.offset,
            self.page.as_ref().map_or(0, |p| p.page.entries.len()),
            self.page.as_ref().map_or(0, |p| p.page.total_matches),
            self.page.as_ref().map_or(0, |p| p.indexed_entries),
            self.rows_built,
            self.visible.start,
            self.visible.end,
            self.window.width,
            self.window.height,
            self.viewport.width,
            self.viewport.height,
            MAX_SESSION_PAGE_SIZE
        )
    }
}

fn collection_focus(focus: &str, collection: &str) -> bool {
    focus == collection
        || focus
            .strip_prefix(collection)
            .is_some_and(|suffix| suffix.starts_with('/'))
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
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/find-generated")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str) {
            fs::write(self.0.join(name), b"find fixture").unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn complete(panel: &mut FindPanel) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            panel.poll();
            if !panel.pending && matches!(panel.coverage(), Coverage::Complete | Coverage::Partial)
            {
                return;
            }
            assert!(Instant::now() < deadline, "{}", panel.evidence());
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn frame(panel: &mut FindPanel, ui: &mut Ui, size: Size, input: Input) -> Option<FindAction> {
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

    #[test]
    fn late_match_and_last_page_beyond_five_thousand_are_reachable() {
        let fixture = Fixture::new();
        for i in 0..6_001 {
            fixture.file(&format!("item-{i:05}"));
        }
        fixture.file("zz-late-needle");
        let mut panel = FindPanel::new(fixture.0.clone());
        complete(&mut panel);
        assert_eq!(panel.page.as_ref().unwrap().page.total_matches, 6_002);
        panel.query = "LATE-NEEDLE".into();
        panel.submit(0);
        complete(&mut panel);
        assert_eq!(panel.page.as_ref().unwrap().page.total_matches, 1);
        assert!(
            matches!(panel.open(0), Some(FindAction::Open { path, kind: EntryKind::File }) if path == fixture.0.join("zz-late-needle"))
        );
        panel.query.clear();
        panel.submit(0);
        complete(&mut panel);
        panel.selected = Some(7);
        panel.selection_armed = true;
        panel.submit(MAX_SESSION_PAGE_SIZE * 11);
        assert_eq!(panel.selected, None);
        assert!(!panel.selection_armed);
        complete(&mut panel);
        let last = panel.page.as_ref().unwrap();
        assert_eq!(last.page.total_matches, 6_002);
        assert_eq!(last.page.entries.len(), 370);
        assert_eq!(
            last.page.entries.last().unwrap().display_name,
            "zz-late-needle"
        );
    }

    #[test]
    fn stale_query_page_is_rejected_and_changed_publications_clear_selection() {
        let fixture = Fixture::new();
        fixture.file("alpha");
        fixture.file("beta");
        let mut panel = FindPanel::new(fixture.0.clone());
        complete(&mut panel);
        let old = panel.page.as_ref().unwrap().clone();
        panel.query = "beta".into();
        panel.submit(0);
        let current = panel.request_id;
        assert!(!panel.apply(SessionDelivery::Page(old)));
        assert_eq!(panel.request_id, current);
        assert!(panel.page.is_none());
        complete(&mut panel);
        panel.selected = Some(0);
        panel.selection_armed = true;
        let publication = panel.page.as_ref().unwrap().clone();
        assert!(panel.apply(SessionDelivery::Page(publication)));
        assert!(panel.selected.is_none());
        assert!(!panel.selection_armed);
        assert!(collection_focus("find-page-1/item/0", "find-page-1"));
        assert!(collection_focus("find-page-1", "find-page-1"));
        assert!(!collection_focus("find-page-10/item/0", "find-page-1"));
    }

    #[cfg(unix)]
    #[test]
    fn native_scope_and_result_bytes_survive_unchanged_lossy_display_and_restart() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let root = fixture
            .0
            .join(std::ffi::OsString::from_vec(b"scope-\xff".to_vec()));
        fs::create_dir(&root).unwrap();
        let native = root.join(std::ffi::OsString::from_vec(b"native-\xfe".to_vec()));
        fs::write(&native, b"native").unwrap();
        let mut panel = FindPanel::new(root.clone());
        complete(&mut panel);
        panel.restart();
        complete(&mut panel);
        assert_eq!(panel.scope_native, root);
        assert_eq!(
            panel.page.as_ref().unwrap().requested_roots.as_ref(),
            std::slice::from_ref(&root)
        );
        assert!(matches!(panel.open(0), Some(FindAction::Open { path, .. }) if path == native));
    }

    #[test]
    fn intrinsic_controls_and_virtual_results_fit_small_and_large_surfaces() {
        let fixture = Fixture::new();
        for i in 0..100 {
            fixture.file(&format!("file-{i:03}"));
        }
        let mut panel = FindPanel::new(fixture.0.clone());
        complete(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        for size in [
            Size::new(640., 480.),
            Size::new(1024., 720.),
            Size::new(320., 240.),
        ] {
            for _ in 0..5 {
                frame(&mut panel, &mut ui, size, Input::default());
            }
            let scene = ui.scene().unwrap();
            for id in [
                "find-panel",
                "find-scope",
                "find-query",
                "find-results",
                "find-previous",
                "find-next",
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

    #[test]
    fn query_enter_then_keyboard_navigation_arms_native_activation_and_escape_closes() {
        let fixture = Fixture::new();
        fixture.file("alpha");
        fixture.file("beta");
        let mut panel = FindPanel::new(fixture.0.clone());
        complete(&mut panel);
        let mut ui = super::super::make_ui().unwrap();
        let size = Size::new(640., 480.);
        for _ in 0..4 {
            frame(&mut panel, &mut ui, size, Input::default());
        }
        ui.focus("find-query");
        frame(&mut panel, &mut ui, size, key(Key::Enter));
        assert!(frame(&mut panel, &mut ui, size, Input::default()).is_none());
        assert!(
            ui.focus_key()
                .is_some_and(|id| collection_focus(id, &panel.list_id()))
        );
        assert!(!panel.selection_armed);
        frame(&mut panel, &mut ui, size, key(Key::Home));
        frame(&mut panel, &mut ui, size, Input::default());
        assert!(panel.selection_armed);
        assert_eq!(panel.selected, Some(0));
        frame(&mut panel, &mut ui, size, key(Key::Enter));
        assert!(
            matches!(frame(&mut panel, &mut ui, size, Input::default()), Some(FindAction::Open { path, .. }) if path == fixture.0.join("alpha"))
        );
        ui.focus("find-query");
        frame(&mut panel, &mut ui, size, key(Key::Escape));
        assert!(matches!(
            frame(&mut panel, &mut ui, size, Input::default()),
            Some(FindAction::Close)
        ));
    }
}
