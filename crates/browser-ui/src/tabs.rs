//! Application shell. Tab controls and scoped input routing land separately.
use super::*;
use browser_core::workspace::{Activation, Limits, Workspace};

pub struct BrowserView {
    pub(super) workspace: Workspace<TabView>,
    pub(super) jobs: AppJobs,
}

impl BrowserView {
    fn with_tab(tab: TabView) -> Self {
        let mut workspace =
            Workspace::new(Limits::default()).expect("valid default workspace limits");
        workspace
            .create(tab, Activation::Foreground)
            .unwrap_or_else(|_| unreachable!("empty workspace admits first tab"));
        Self {
            workspace,
            jobs: AppJobs::default(),
        }
    }
    pub fn new(path: PathBuf) -> Self {
        Self::with_tab(TabView::new(path))
    }
    pub fn fixture(count: usize) -> Self {
        Self::with_tab(TabView::fixture(count))
    }
    fn active_tab(&self) -> Option<&TabView> {
        self.workspace
            .active()
            .and_then(|id| self.workspace.get(id).ok())
    }
    #[cfg(test)]
    pub(super) fn tab(&self) -> &TabView {
        self.active_tab().expect("test has a live tab")
    }
    #[cfg(test)]
    pub(super) fn tab_mut(&mut self) -> &mut TabView {
        let id = self.workspace.active().expect("test has a live tab");
        self.workspace.get_mut(id).expect("active tab is live")
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn paste_files(&mut self) {
        if let Some(id) = self.workspace.active() {
            self.workspace
                .get_mut(id)
                .expect("active tab is live")
                .paste_files(&mut self.jobs);
        }
    }
    #[cfg(target_os = "linux")]
    pub fn enable_native_file_clipboard(&mut self) {
        if self.active_tab().is_some_and(|tab| tab.browser.is_some())
            && self.jobs.file_clipboard.is_none()
        {
            self.jobs.file_clipboard = Some(clipboard::ClipboardUi::new());
        }
    }
    pub fn mutations_busy(&self) -> bool {
        self.jobs.mutations_busy()
    }
    pub fn evidence(&self) -> String {
        if let Some(panel) = self
            .jobs
            .extraction
            .as_ref()
            .filter(|panel| panel.visible())
        {
            return panel.evidence();
        }
        #[cfg(target_os = "linux")]
        if let Some(panel) = self
            .jobs
            .copy_transfers
            .as_ref()
            .filter(|panel| panel.visible())
        {
            return panel.evidence();
        }
        self.active_tab().map_or_else(
            || "{\"mode\":\"empty-workspace\"}".into(),
            |tab| tab.evidence(&self.jobs),
        )
    }
    fn poll_jobs(&mut self) -> bool {
        let (mut changed, refresh) = self.jobs.poll();
        // Workspace count is bounded. Destinations are native paths captured by
        // accepted jobs; refresh all matching tabs, independent of active index.
        let ids: Vec<_> = self.workspace.tabs().map(|(id, _)| id).collect();
        for id in ids {
            let tab = self.workspace.get_mut(id).expect("enumerated live tab");
            if let Some(browser) = &mut tab.browser
                && refresh
                    .iter()
                    .any(|destination| browser.path() == destination)
            {
                browser.navigate(browser.path().to_path_buf());
                tab.reset_listing();
                changed = true;
            }
        }
        changed
    }
}

impl View for BrowserView {
    fn build(&mut self, ui: &mut Ui, input: &Input) -> El {
        self.jobs.raw_extraction_owner = self.jobs.presented_extraction_owner;
        self.jobs.built_extraction_owner = None;
        self.poll_jobs();
        let current_owner = self
            .jobs
            .extraction
            .as_ref()
            .and_then(ExtractionUi::input_owner);
        if current_owner != self.jobs.presented_extraction_owner {
            ui.discard_input_for_view_change();
        }
        let empty_input = Input::default();
        let gated_input =
            if current_owner.is_some() && current_owner == self.jobs.raw_extraction_owner {
                input
            } else {
                &empty_input
            };
        if let Some(tree) = self.jobs.build_extraction(ui, gated_input) {
            return tree;
        }
        if let Some(tree) = self.jobs.build_copy_transfers(ui) {
            return tree;
        }
        if let Some(id) = self.workspace.active() {
            return self
                .workspace
                .get_mut(id)
                .expect("active tab is live")
                .build(ui, &mut self.jobs);
        }
        let tree = col([label("No folders open")]).grow(1).min_w(0).min_h(0);
        self.jobs.with_mutation_status(ui, tree)
    }
    fn changed(&mut self) -> bool {
        let mut changed = self.poll_jobs();
        let ids: Vec<_> = self.workspace.tabs().map(|(id, _)| id).collect();
        for id in ids {
            changed |= self
                .workspace
                .get_mut(id)
                .expect("enumerated live tab")
                .changed();
        }
        changed
    }
    fn request_resize(&mut self, _: u32, _: u32) -> bool {
        false
    }
    fn after_frame(&mut self, ui: &mut Ui) {
        // Ui::frame has just delivered the host batch to the newly built tree.
        // Do not retarget old-owner input into a new extraction controller (or
        // back into the tab). Persistent jobs and semantic host actions survive.
        if self.jobs.raw_extraction_owner != self.jobs.built_extraction_owner {
            ui.discard_input_for_view_change();
        }
        self.jobs.presented_extraction_owner = self.jobs.built_extraction_owner;
        self.jobs.dispatch_clipboard_controls(ui);
        self.jobs.admit_paste_offer();
        if let Some(panel) = self
            .jobs
            .extraction
            .as_mut()
            .filter(|panel| panel.visible())
        {
            panel.after_frame(ui);
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(panel) = self
            .jobs
            .copy_transfers
            .as_mut()
            .filter(|panel| panel.visible())
        {
            panel.after_frame(ui);
            return;
        }
        if let Some(id) = self.workspace.active() {
            self.workspace
                .get_mut(id)
                .expect("active tab is live")
                .after_frame(ui);
        }
    }
    fn claims_key(&self, key: &Key, mods: Mods) -> bool {
        matches!(
            key,
            Key::Up
                | Key::Down
                | Key::Home
                | Key::End
                | Key::PageUp
                | Key::PageDown
                | Key::Enter
                | Key::Space
                | Key::Escape
        ) || ((mods.ctrl || mods.cmd)
            && matches!(
                key,
                Key::Char('l' | 'f' | 'F' | 'a' | 'A' | 'c' | 'C' | 'v' | 'V')
            ))
            || (mods.alt && matches!(key, Key::Left | Key::Right))
    }
}
