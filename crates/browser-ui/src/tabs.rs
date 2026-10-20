//! Application shell. Tab controls and scoped input routing land separately.
use super::*;
use browser_core::workspace::{Activation, Limits, TabId, TabSnapshot, Workspace};

pub struct BrowserView {
    pub(super) workspace: Workspace<TabView>,
    palette: Option<palette::Palette>,
    pub(super) jobs: AppJobs,
}

impl BrowserView {
    fn with_tab(mut tab: TabView) -> Self {
        let mut workspace =
            Workspace::new(Limits::default()).expect("valid default workspace limits");
        let settings = settings::Settings::load();
        tab.apply_settings(&settings);
        workspace
            .create(tab, Activation::Foreground)
            .unwrap_or_else(|_| unreachable!("empty workspace admits first tab"));
        Self {
            workspace,
            palette: None,
            jobs: AppJobs {
                places: places::Places::load(),
                settings,
                ..AppJobs::default()
            },
        }
    }
    pub fn new(path: PathBuf) -> Self {
        Self::with_tab(TabView::new(path))
    }
    pub fn fixture(count: usize) -> Self {
        let mut view = Self::with_tab(TabView::fixture(count));
        // Synthetic fixtures must not depend on this machine's places.
        view.jobs.places = places::Places::default();
        view
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
    pub(super) fn poll_jobs(&mut self) -> bool {
        let (mut changed, refresh) = self.jobs.poll();
        if refresh.is_empty() {
            return changed;
        }
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

/// A structural workspace change requested this frame (at most one is applied).
enum TabIntent {
    Activate(TabId),
    Close(TabId),
    New,
    Reopen,
    Cycle(isize),
}

impl BrowserView {
    fn tab_intent(&self, ui: &Ui) -> Option<TabIntent> {
        for (id, _) in self.workspace.tabs() {
            if clicked(ui, &format!("tab-close/{id}")) {
                return Some(TabIntent::Close(id));
            }
            if clicked(ui, &format!("tab/{id}")) {
                return Some(TabIntent::Activate(id));
            }
        }
        if clicked(ui, "tab-new") {
            return Some(TabIntent::New);
        }
        for press in ui.shortcuts() {
            if !(press.mods.ctrl || press.mods.cmd) {
                continue;
            }
            match press.key {
                Key::Char('t' | 'T') if press.mods.shift => {
                    return Some(TabIntent::Reopen);
                }
                Key::Char('t' | 'T') => return Some(TabIntent::New),
                Key::Char('w' | 'W') => {
                    return self.workspace.active().map(TabIntent::Close);
                }
                Key::Tab => {
                    return Some(TabIntent::Cycle(if press.mods.shift { -1 } else { 1 }));
                }
                _ => {}
            }
        }
        None
    }
    fn run_command(&mut self, ui: &mut Ui, cmd: palette::Cmd) {
        use palette::Cmd;
        let intent = match cmd {
            Cmd::NewTab => Some(TabIntent::New),
            Cmd::CloseTab => self.workspace.active().map(TabIntent::Close),
            Cmd::ReopenTab => Some(TabIntent::Reopen),
            Cmd::NextTab => Some(TabIntent::Cycle(1)),
            Cmd::PrevTab => Some(TabIntent::Cycle(-1)),
            _ => None,
        };
        if let Some(intent) = intent {
            self.apply_tab_intent(ui, intent);
        } else if let Some(id) = self.workspace.active() {
            ui.focus("file-list");
            self.workspace
                .get_mut(id)
                .expect("active tab is live")
                .run_command(ui, &mut self.jobs, cmd);
        }
    }
    fn apply_tab_intent(&mut self, ui: &mut Ui, intent: TabIntent) {
        let changed = match intent {
            TabIntent::Activate(id) => self.workspace.activate(id).unwrap_or(false),
            TabIntent::Cycle(step) => {
                let ids: Vec<_> = self.workspace.tabs().map(|(id, _)| id).collect();
                let at = self
                    .workspace
                    .active()
                    .and_then(|a| ids.iter().position(|i| *i == a));
                match at {
                    Some(at) if ids.len() > 1 => {
                        let next = (at as isize + step).rem_euclid(ids.len() as isize) as usize;
                        self.workspace.activate(ids[next]).unwrap_or(false)
                    }
                    _ => false,
                }
            }
            TabIntent::New => {
                let location = self
                    .workspace
                    .active()
                    .and_then(|id| self.workspace.get(id).ok())
                    .and_then(TabView::location)
                    .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                    .unwrap_or_else(|| PathBuf::from("/"));
                let mut tab = TabView::new(location);
                tab.apply_settings(&self.jobs.settings);
                self.workspace.create(tab, Activation::Foreground).is_ok()
            }
            TabIntent::Reopen => {
                let Some(closed) = self.workspace.recently_closed().next_back() else {
                    return;
                };
                let (id, location) = (closed.id(), closed.snapshot().location().to_path_buf());
                self.workspace
                    .reopen(id, TabView::new(location), Activation::Foreground)
                    .is_ok()
            }
            TabIntent::Close(id) => {
                let snapshot = self.workspace.get(id).ok().and_then(|tab| {
                    tab.location()
                        .map(|l| TabSnapshot::new(l, Vec::new(), Vec::new(), tab.filter.clone()))
                });
                match snapshot {
                    Some(snapshot) => self.workspace.close(id, snapshot).is_ok(),
                    None => self.workspace.close_without_history(id).is_ok(),
                }
            }
        };
        if changed {
            ui.discard_input_for_view_change();
            ui.focus("file-list");
            self.jobs.geometry_changed = true;
        }
    }
    fn tab_strip(&self, ui: &Ui) -> El {
        let active = self.workspace.active();
        let mut items: Vec<El> = self
            .workspace
            .tabs()
            .map(|(id, tab)| {
                let selected = Some(id) == active;
                row([
                    col([row([
                        glyph(Glyph::Folder, color(0xe2bd55)),
                        label(tab.title()).min_w(0).clip(),
                    ])
                    .gap(7)])
                    .id(format!("tab/{id}"))
                    .a11y(A11y::Button)
                    .named("Switch to tab")
                    .focusable()
                    .min_w(0)
                    .shrink(1),
                    action(
                        ui,
                        &format!("tab-close/{id}"),
                        "Close tab · Ctrl+W",
                        glyph(Glyph::Close, muted()),
                        true,
                        false,
                    ),
                ])
                .gap(4)
                .pad((8, 4))
                .align(Align::Center)
                .fill(if selected { ground() } else { chrome() })
                .radius(3)
                .min_w(0)
                .shrink(1)
            })
            .collect();
        items.push(action(
            ui,
            "tab-new",
            "New tab · Ctrl+T",
            label("+"),
            true,
            false,
        ));
        items.push(spacer());
        row(items)
            .gap(2)
            .align(Align::Center)
            .fill(chrome())
            .w(pct(100))
    }
}

impl View for BrowserView {
    fn build(&mut self, ui: &mut Ui, input: &Input) -> El {
        if ui
            .shortcuts()
            .iter()
            .any(|p| (p.mods.ctrl || p.mods.cmd) && matches!(p.key, Key::Char('p' | 'P')))
        {
            self.palette = Some(palette::Palette::open());
        }
        if let Some(intent) = self.tab_intent(ui) {
            self.apply_tab_intent(ui, intent);
        }
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
        let strip = self.tab_strip(ui);
        let mut palette_el = None;
        if let Some(palette) = &mut self.palette {
            let (el, outcome) = palette.build(ui);
            palette_el = Some(el);
            match outcome {
                palette::Outcome::Open => {}
                palette::Outcome::Close => self.palette = None,
                palette::Outcome::Run(cmd) => {
                    self.palette = None;
                    self.run_command(ui, cmd);
                }
            }
        }
        if let Some(id) = self.workspace.active() {
            let tab = self.workspace.get_mut(id).expect("active tab is live");
            let tree = tab.build(ui, &mut self.jobs, Some(strip));
            if std::mem::take(&mut tab.context_requested) {
                self.palette = Some(palette::Palette::context());
            }
            return match palette_el {
                Some(el) => col([el, tree.grow(1).basis(0).min_h(0)])
                    .w(pct(100))
                    .h(pct(100))
                    .min_w(0)
                    .min_h(0),
                None => tree,
            };
        }
        let tree = col([strip, label("No folders open")])
            .grow(1)
            .min_w(0)
            .min_h(0);
        self.jobs.with_mutation_status(ui, tree)
    }
    fn changed(&mut self) -> bool {
        let mut changed = self.poll_jobs();
        if let Some(now) = self
            .workspace
            .active()
            .and_then(|id| self.workspace.get(id).ok())
            .filter(|t| t.fixture_count.is_none())
            .and_then(TabView::current_settings)
            && now != self.jobs.settings
        {
            self.jobs.settings = now;
            now.save();
        }
        if let Some(tab) = self
            .workspace
            .active()
            .and_then(|id| self.workspace.get(id).ok())
            .filter(|t| {
                t.browser
                    .as_ref()
                    .is_some_and(|b| matches!(b.state(), LoadState::Complete))
            })
            && let Some(path) = tab.location()
        {
            self.jobs.places.visit(&path);
        }
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
                | Key::Delete
                | Key::Function(2)
                | Key::Tab
        ) || ((mods.ctrl || mods.cmd)
            && matches!(
                key,
                Key::Char(
                    'l' | 'f'
                        | 'F'
                        | 'a'
                        | 'A'
                        | 'c'
                        | 'C'
                        | 'v'
                        | 'V'
                        | 'x'
                        | 'X'
                        | 'z'
                        | 'Z'
                        | 'n'
                        | 'N'
                        | 't'
                        | 'T'
                        | 'w'
                        | 'W'
                        | 'd'
                        | 'D'
                        | 'h'
                        | 'H'
                        | 'p'
                        | 'P'
                )
            ))
            || (mods.alt && matches!(key, Key::Left | Key::Right | Key::Up))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use browser_core::selection::{Action, Modifiers};
    use std::{
        ffi::OsString,
        fs,
        os::unix::ffi::OsStringExt,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/tabs-generated")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn tab_intents_open_cycle_close_and_reopen_by_location() {
        let fixture = Fixture::new();
        let sub = fixture.0.join("sub");
        fs::create_dir(&sub).unwrap();
        let mut view = BrowserView::new(fixture.0.clone());
        let mut ui = make_ui().unwrap();
        view.apply_tab_intent(&mut ui, TabIntent::New);
        assert_eq!(view.workspace.len(), 2);
        let second = view.workspace.active().unwrap();
        assert_eq!(view.tab().location().unwrap(), fixture.0);
        view.apply_tab_intent(&mut ui, TabIntent::Cycle(1));
        assert_ne!(view.workspace.active(), Some(second));
        view.apply_tab_intent(&mut ui, TabIntent::Cycle(-1));
        assert_eq!(view.workspace.active(), Some(second));
        view.apply_tab_intent(&mut ui, TabIntent::Close(second));
        assert_eq!(view.workspace.len(), 1);
        view.apply_tab_intent(&mut ui, TabIntent::Reopen);
        assert_eq!(view.workspace.len(), 2);
        assert_ne!(
            view.workspace.active(),
            Some(second),
            "reopened tabs get new ids"
        );
        assert_eq!(view.tab().location().unwrap(), fixture.0);
        drop(sub);
    }
    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "application worker timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn copy_complete(view: &mut BrowserView) -> bool {
        view.changed();
        view.jobs.copy_transfers.as_ref().is_some_and(|copy| {
            let evidence = copy.evidence();
            assert!(!evidence.contains("\"state\":\"failed\""), "{evidence}");
            evidence.contains("\"state\":\"complete\"")
        })
    }
    fn settle_listings(view: &mut BrowserView) {
        until(|| {
            view.changed();
            view.workspace.tabs().all(|(_, tab)| {
                tab.browser
                    .as_ref()
                    .is_none_or(|browser| match browser.state() {
                        LoadState::Complete => true,
                        LoadState::Loading => false,
                        LoadState::Failed(error) => panic!("listing failed: {error}"),
                    })
            })
        });
    }

    #[test]
    fn captured_native_paste_survives_origin_close_and_empty_workspace() {
        let fixture = Fixture::new();
        let source = fixture.0.join("payload");
        let destination = fixture.0.join("destination");
        fs::write(&source, b"app-owned copied payload").unwrap();
        fs::create_dir(&destination).unwrap();
        let mut view = BrowserView::fixture(5);
        view.jobs.file_clipboard = Some(clipboard::ClipboardUi::fixture_offer(
            vec![source],
            destination.clone(),
        ));
        let origin = view.workspace.active().unwrap();
        let removed = view.workspace.close_without_history(origin).unwrap();
        thread::spawn(move || drop(removed)).join().unwrap();
        assert!(view.workspace.is_empty());
        assert!(view.mutations_busy(), "captured offer retains admission");
        view.jobs.admit_paste_offer();
        until(|| copy_complete(&mut view));
        assert_eq!(
            fs::read(destination.join("payload")).unwrap(),
            b"app-owned copied payload"
        );
        assert!(
            view.mutations_busy(),
            "terminal remains until acknowledgement"
        );
        let mut ui = make_ui().unwrap();
        for _ in 0..3 {
            let tree = view.build(&mut ui, &Input::default());
            ui.frame(tree, Some(Size::new(640., 480.)), Input::default(), 0.016)
                .unwrap();
            view.after_frame(&mut ui);
        }
        assert!(ui.scene().unwrap().surface("copy-transfer-root").is_some());
        assert!(view.evidence().contains("\"state\":\"complete\""));
        assert!(view.jobs.copy_transfers.as_ref().unwrap().busy());
    }

    #[test]
    fn committed_copy_refreshes_all_matching_native_tabs_and_preserves_other_tab() {
        let fixture = Fixture::new();
        let destination = fixture
            .0
            .join(OsString::from_vec(b"destination-\xff".to_vec()));
        let unrelated = fixture.0.join("unrelated");
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&unrelated).unwrap();
        fs::write(destination.join("needle-old"), b"existing").unwrap();
        fs::write(unrelated.join("unrelated-file"), b"untouched").unwrap();
        let source = fixture.0.join("needle-new");
        fs::write(&source, b"new content").unwrap();
        let mut view = BrowserView::new(destination.clone());
        let first = view.workspace.active().unwrap();
        let second = view
            .workspace
            .create(TabView::new(destination.clone()), Activation::Background)
            .unwrap_or_else(|_| panic!("second tab admission failed"));
        let other = view
            .workspace
            .create(TabView::new(unrelated.clone()), Activation::Background)
            .unwrap_or_else(|_| panic!("third tab admission failed"));
        for id in [first, second] {
            let tab = view.workspace.get_mut(id).unwrap();
            tab.filter = "needle".into();
            tab.browser.as_mut().unwrap().set_filter("needle");
        }
        settle_listings(&mut view);
        for id in [first, second, other] {
            view.workspace.get_mut(id).unwrap().apply_selection(
                Action::Click {
                    index: 0,
                    modifiers: Modifiers::default(),
                },
                false,
            );
        }
        let old_other_revision = view
            .workspace
            .get(other)
            .unwrap()
            .selection
            .revision()
            .clone();
        let old_first_revision = view
            .workspace
            .get(first)
            .unwrap()
            .selection
            .revision()
            .clone();
        view.jobs.file_clipboard = Some(clipboard::ClipboardUi::fixture_offer(
            vec![source],
            destination.clone(),
        ));
        view.jobs.admit_paste_offer();
        until(|| copy_complete(&mut view));
        settle_listings(&mut view);
        assert_eq!(view.workspace.active(), Some(first));
        for id in [first, second] {
            let tab = view.workspace.get(id).unwrap();
            assert_eq!(tab.browser.as_ref().unwrap().path(), destination);
            assert_eq!(tab.filter, "needle");
            assert_eq!(tab.count(), 2);
            assert_eq!(tab.selection.count(), 0);
            assert!(
                tab.browser
                    .as_ref()
                    .unwrap()
                    .entries()
                    .iter()
                    .any(|entry| entry.path == destination.join("needle-new"))
            );
        }
        assert_ne!(
            view.workspace.get(first).unwrap().selection.revision(),
            &old_first_revision
        );
        let untouched = view.workspace.get(other).unwrap();
        assert_eq!(untouched.browser.as_ref().unwrap().path(), unrelated);
        assert_eq!(untouched.selection.revision(), &old_other_revision);
        assert_eq!(untouched.selection.count(), 1);
    }
}
