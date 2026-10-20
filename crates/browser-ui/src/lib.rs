//! Intrinsic native file browser. Layout is entirely a constrained row/column tree.
#![forbid(unsafe_code)]

use browser_core::selection::{
    Action as SelectAction, ListingRevision, Modifiers as SelectMods, Movement, Selection,
};
use browser_core::{Browser, EntryKind, LoadState, SortKey, SortOrder};
use mui::host::View;
use mui::prelude::*;
use std::io::Read;
use std::path::{Path as FsPath, PathBuf};
use std::sync::OnceLock;

mod dirsize;
mod fileops;
mod jobs;
mod palette;
mod places;
mod preview;
mod settings;
mod thumbs;
use jobs::AppJobs;
mod tabs;
pub use tabs::BrowserView;

mod find;
use find::{FindAction, FindPanel};
mod extraction;
use extraction::{ExtractionAction, ExtractionUi};
mod archive;
#[cfg(target_os = "linux")]
mod transfers;
use archive::{ArchiveAction, ArchivePanel};
#[cfg(target_os = "linux")]
mod clipboard;

const ROW: f64 = 25.0;
const OVERSCAN: f64 = ROW * 2.0;
const TILE_W: f64 = 112.0;
const TILE_H: f64 = 104.0;
const CONTENT_MIN: f64 = 280.0;
const RAIL_MIN: f64 = 154.0;
const INSPECTOR_MIN: f64 = 220.0;
const GAP: f64 = 6.0;

fn color(hex: u32) -> Color {
    Color::srgb(
        ((hex >> 16) & 255) as f32 / 255.0,
        ((hex >> 8) & 255) as f32 / 255.0,
        (hex & 255) as f32 / 255.0,
    )
}
fn ground() -> Color {
    color(0x14181a)
}
fn chrome() -> Color {
    color(0x1b2023)
}
fn line() -> Color {
    color(0x262d30)
}
fn ink() -> Color {
    color(0xe4e8e8)
}
fn muted() -> Color {
    color(0xa4afb2)
}
fn selected() -> Color {
    color(0x087d99)
}
fn selected_ink() -> Color {
    color(0xffffff)
}
fn cursor_stroke(active: bool) -> Color {
    if active {
        selected_ink()
    } else {
        color(0x63c9e0)
    }
}
fn label(s: impl Into<std::sync::Arc<str>>) -> El {
    text(s).text_size(13).fill(ink())
}
fn detail(s: impl Into<std::sync::Arc<str>>) -> El {
    text(s).text_size(12).fill(muted())
}
fn hairline() -> El {
    block(0, 1).w(pct(100)).fill(line()).shrink(0)
}

#[derive(Clone, Copy)]
enum Glyph {
    Folder,
    File,
    Back,
    Forward,
    Up,
    Home,
    Search,
    Inspector,
    Close,
}

/// Authored vector primitives share one 16-unit grid; never font/emoji substitutes.
fn glyph(kind: Glyph, tint: Color) -> El {
    glyph_with_tone(kind, tint, false)
}

fn entry_glyph(folder: bool, active: bool, file_tint: Color) -> El {
    glyph_with_tone(
        if folder { Glyph::Folder } else { Glyph::File },
        if active {
            selected_ink()
        } else if folder {
            color(0xe2bd55)
        } else {
            file_tint
        },
        active,
    )
}

fn glyph_with_tone(kind: Glyph, tint: Color, monochrome: bool) -> El {
    canvas(move |size| {
        let path = |points: &[(f64, f64)], close| {
            Path::polyline(
                points
                    .iter()
                    .map(|(x, y)| Point::new(x * size.width / 16.0, y * size.height / 16.0)),
                close,
            )
        };
        match kind {
            Glyph::Folder => vec![
                Draw::fill(
                    path(
                        &[
                            (1., 3.),
                            (6., 3.),
                            (7.5, 5.),
                            (15., 5.),
                            (15., 13.),
                            (1., 13.),
                        ],
                        true,
                    ),
                    tint,
                ),
                Draw::fill(
                    path(&[(1., 6.), (15., 6.), (15., 13.), (1., 13.)], true),
                    if monochrome { tint } else { color(0xf1cc65) },
                ),
            ],
            Glyph::File => vec![
                Draw::stroke(
                    path(
                        &[(4., 1.5), (10., 1.5), (13., 4.5), (13., 14.5), (4., 14.5)],
                        true,
                    ),
                    tint,
                    1.,
                ),
                Draw::stroke(path(&[(9.5, 2.), (9.5, 5.), (12.5, 5.)], false), tint, 1.),
            ],
            Glyph::Back => vec![
                Draw::stroke(path(&[(9., 4.), (5., 8.), (9., 12.)], false), tint, 1.4),
                Draw::stroke(path(&[(5., 8.), (14., 8.)], false), tint, 1.4),
            ],
            Glyph::Forward => vec![
                Draw::stroke(path(&[(7., 4.), (11., 8.), (7., 12.)], false), tint, 1.4),
                Draw::stroke(path(&[(2., 8.), (11., 8.)], false), tint, 1.4),
            ],
            Glyph::Up => vec![
                Draw::stroke(path(&[(4., 7.), (8., 3.), (12., 7.)], false), tint, 1.4),
                Draw::stroke(path(&[(8., 3.), (8., 14.)], false), tint, 1.4),
            ],
            Glyph::Home => vec![
                Draw::stroke(path(&[(1., 7.), (8., 1.5), (15., 7.)], false), tint, 1.2),
                Draw::stroke(
                    path(&[(3., 6.), (3., 14.), (13., 14.), (13., 6.)], false),
                    tint,
                    1.2,
                ),
            ],
            Glyph::Search => vec![
                Draw::stroke(
                    path(
                        &[
                            (2., 4.),
                            (4., 2.),
                            (8., 2.),
                            (10., 4.),
                            (10., 8.),
                            (8., 10.),
                            (4., 10.),
                            (2., 8.),
                        ],
                        true,
                    ),
                    tint,
                    1.2,
                ),
                Draw::stroke(path(&[(10., 10.), (14., 14.)], false), tint, 1.4),
            ],
            Glyph::Inspector => vec![
                Draw::stroke(
                    path(&[(1.5, 2.), (14.5, 2.), (14.5, 14.), (1.5, 14.)], true),
                    tint,
                    1.2,
                ),
                Draw::stroke(path(&[(10., 2.), (10., 14.)], false), tint, 1.2),
            ],
            Glyph::Close => vec![
                Draw::stroke(path(&[(4., 4.), (12., 12.)], false), tint, 1.2),
                Draw::stroke(path(&[(12., 4.), (4., 12.)], false), tint, 1.2),
            ],
        }
    })
    .square(16)
    .shrink(0)
}

fn action(ui: &Ui, id: &str, name: &str, content: El, enabled: bool, active: bool) -> El {
    col([content])
        .pad((6, 5))
        .center()
        .radius(3)
        .fill(if active { color(0x34474d) } else { chrome() })
        .id(id)
        .a11y(A11y::Button)
        .named(name)
        .tip(name)
        .focusable()
        .cursor(Cursor::Hand)
        .on(State::Hover, |s| s.fill(color(0x354045)))
        .on(State::FocusVisible, |s| {
            s.stroke(color(0x58c3df)).stroke_width(1)
        })
        .when(!enabled, |e| e.disabled().opacity(0.38))
        .when(ui.get(id).pressed, |e| e.fill(color(0x43545a)))
}

fn clicked(ui: &Ui, id: &str) -> bool {
    let r = ui.get(id);
    r.clicked || r.key_activated
}

// Bounded named candidates only. OS font loading occurs once at startup, never
// during layout; these bytes are runtime dependencies, not part of our executable.
const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;
struct OsFallback {
    font: Font,
    bytes: u64,
}
static OS_FALLBACKS: OnceLock<Vec<OsFallback>> = OnceLock::new();
fn read_fallback(path: &FsPath) -> Result<Option<OsFallback>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_FONT_BYTES {
        return Err(format!(
            "{}: not a regular font within the 32 MiB cap",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FONT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FONT_BYTES {
        return Err(format!("{} grew beyond the 32 MiB cap", path.display()));
    }
    let size = bytes.len() as u64;
    let font = Font::from_index(bytes, 0).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(OsFallback { font, bytes: size }))
}
fn fallback_candidates() -> [Vec<PathBuf>; 2] {
    if cfg!(target_os = "windows") {
        let root = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
            .join("Fonts");
        [
            vec![root.join("segoeui.ttf"), root.join("arial.ttf")],
            vec![
                root.join("meiryo.ttc"),
                root.join("msyh.ttc"),
                root.join("msgothic.ttc"),
            ],
        ]
    } else if cfg!(target_os = "macos") {
        [
            vec![
                PathBuf::from("/System/Library/Fonts/GeezaPro.ttc"),
                PathBuf::from("/System/Library/Fonts/Supplemental/Geeza Pro.ttc"),
            ],
            vec![
                PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
                PathBuf::from("/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc"),
            ],
        ]
    } else {
        [
            vec![
                PathBuf::from("/usr/share/fonts/noto/NotoSansArabic-Regular.ttf"),
                PathBuf::from("/usr/share/fonts/truetype/noto/NotoSansArabic-Regular.ttf"),
                PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            ],
            vec![
                PathBuf::from("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
                PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            ],
        ]
    }
}
fn os_fallbacks() -> &'static [OsFallback] {
    OS_FALLBACKS.get_or_init(|| {
        let mut fonts = Vec::new();
        for (group, candidates) in fallback_candidates().into_iter().enumerate() {
            let mut found = false;
            for path in candidates {
                match read_fallback(&path) {
                    Ok(Some(font)) => {
                        eprintln!(
                            "font fallback: {} ({} runtime bytes)",
                            path.display(),
                            font.bytes
                        );
                        fonts.push(font);
                        found = true;
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("font fallback skipped: {error}"),
                }
            }
            if !found {
                eprintln!(
                    "font fallback unavailable for {}; script coverage remains unqualified",
                    if group == 0 { "Arabic" } else { "CJK" }
                );
            }
        }
        fonts
    })
}

pub fn make_ui() -> Result<Ui, String> {
    let theme = Theme {
        text: 13.0,
        corners: Corners {
            field: 3.0,
            box_: 3.0,
            ..Corners::default()
        },
        palette: Palette {
            mode: Mode::Dark,
            neutral: Pigment::new(210.0, 0.008),
            primary: Pigment::new(221.0, 0.11),
            ground: Some(0.23),
            ..Palette::NEUTRAL
        },
        ..Theme::default()
    };
    let font = Font::new(epaint_default_fonts::UBUNTU_LIGHT).map_err(|e| e.to_string())?;
    let mut ui = Ui::new(theme).font(font);
    for fallback in os_fallbacks() {
        ui = ui.fallback_font(fallback.font.clone());
    }
    Ok(ui)
}

struct TabView {
    browser: Option<Browser>,
    fixture_count: Option<usize>,
    /// Element-id prefix: "" for the main pane, "b/" for the second pane of a split.
    pfx: &'static str,
    /// Only the focused pane of a split reacts to the keyboard.
    active_pane: bool,
    sizes: dirsize::DirSizes,
    /// Type-to-select buffer and when its last letter arrived.
    ahead: (String, Option<std::time::Instant>),
    list: ListState,
    path_text: String,
    filter: String,
    // Cursor/Inspector helper only; selected row fill/count come from selection.
    selected: Option<usize>,
    selection: Selection,
    listing_serial: u64,
    discard_listing_input: bool,
    home: PathBuf,
    show_rail: bool,
    show_inspector: bool,
    inspector_width: f64,
    rail_width: f64,
    rows_built: usize,
    visible: std::ops::Range<usize>,
    viewport: Size,
    window: Size,
    geometry_changed: bool,
    evidence_focus_role: &'static str,
    resize_reveal: Option<usize>,
    grid: bool,
    grid_cols: usize,
    find: Option<FindPanel>,
    archive: Option<ArchivePanel>,
    reveal_path: Option<PathBuf>,
    preview: preview::Preview,
    thumbs: thumbs::Thumbs,
    watch: Option<(PathBuf, browser_core::watch::DirWatch)>,
    reload_at: Option<std::time::Instant>,
    context_requested: bool,
}

impl TabView {
    fn title(&self) -> String {
        if self.fixture_count.is_some() {
            "Performance fixture".into()
        } else {
            self.browser
                .as_ref()
                .map(|b| {
                    b.path()
                        .file_name()
                        .map_or_else(|| "Filesystem".into(), |s| s.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "Filesystem".into())
        }
    }
    fn location(&self) -> Option<PathBuf> {
        self.browser.as_ref().map(|b| b.path().to_path_buf())
    }
    pub fn new(path: PathBuf) -> Self {
        let path_text = path.to_string_lossy().into_owned();
        Self {
            browser: Some(Browser::new(path)),
            path_text,
            ..Self::base()
        }
    }
    pub fn fixture(count: usize) -> Self {
        let mut result = Self {
            fixture_count: Some(count),
            pfx: "",
            active_pane: true,
            sizes: Default::default(),
            ahead: Default::default(),
            path_text: "/Synthetic / Performance fixture".into(),
            ..Self::base()
        };
        result.reset_listing();
        if count > 8 {
            result.apply_selection(
                SelectAction::Click {
                    index: 8,
                    modifiers: SelectMods::default(),
                },
                true,
            );
        }
        result
    }
    fn base() -> Self {
        Self {
            browser: None,
            fixture_count: None,
            pfx: "",
            active_pane: true,
            sizes: Default::default(),
            ahead: Default::default(),
            list: ListState::uniform_count(0, ROW),
            path_text: String::new(),
            filter: String::new(),
            selected: None,
            selection: Selection::new(ListingRevision::new(), 0),
            listing_serial: 0,
            discard_listing_input: false,
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/")),
            show_rail: false,
            show_inspector: true,
            inspector_width: 250.,
            rail_width: 174.,
            rows_built: 0,
            visible: 0..0,
            viewport: Size::new(320., 240.),
            window: Size::new(320., 240.),
            geometry_changed: false,
            evidence_focus_role: "none",
            resize_reveal: None,
            grid: false,
            grid_cols: 1,
            find: None,
            archive: None,
            reveal_path: None,
            preview: preview::Preview::default(),
            thumbs: thumbs::Thumbs::default(),
            watch: None,
            reload_at: None,
            context_requested: false,
        }
    }
    #[cfg(target_os = "linux")]
    fn copy_files_id(&self) -> String {
        format!("{}copy-files/{}", self.pfx, self.listing_serial)
    }
    #[cfg(target_os = "linux")]
    fn copy_selected_files(&mut self, jobs: &mut AppJobs) {
        let snapshot = self.browser.as_ref().and_then(Browser::snapshot);
        if let Some(clipboard) = &mut jobs.file_clipboard {
            clipboard.copy(snapshot, &self.selection);
        }
    }
    #[cfg(target_os = "linux")]
    fn paste_files_id(&self) -> String {
        format!("{}paste-files/{}", self.pfx, self.listing_serial)
    }
    #[cfg(target_os = "linux")]
    fn paste_files(&mut self, jobs: &mut AppJobs) {
        if jobs.mutations_busy() {
            if let Some(transfers) = jobs.copy_transfers.as_mut().filter(|t| t.busy()) {
                transfers.show();
            }
            return;
        }
        let Some(browser) = self
            .browser
            .as_ref()
            .filter(|b| matches!(b.state(), LoadState::Complete))
        else {
            return;
        };
        if let Some(clipboard) = &mut jobs.file_clipboard {
            clipboard.read_for_paste(browser.path().to_path_buf());
        }
    }
    /// Run a tab-level palette command. Workspace-level commands are handled by the shell.
    fn run_command(&mut self, ui: &mut Ui, jobs: &mut AppJobs, cmd: palette::Cmd) {
        use palette::Cmd;
        let snapshot = || self.browser.as_ref().and_then(Browser::snapshot);
        match cmd {
            Cmd::Open => {
                if let Some(index) = self.selected {
                    self.activate(index);
                }
            }
            Cmd::NewFolder => self.open_create(jobs, true),
            Cmd::NewFile => self.open_create(jobs, false),
            Cmd::Rename => self.open_rename(jobs),
            Cmd::Trash => {
                let snap = snapshot();
                jobs.fileops.trash_selection(snap, &self.selection);
            }
            Cmd::Compress => {
                let snap = snapshot();
                jobs.fileops.compress_selection(snap, &self.selection);
            }
            Cmd::ToggleSplit | Cmd::CopyToOther | Cmd::MoveToOther => {}
            Cmd::Undo => jobs.fileops.undo_last(),
            Cmd::Cut => {
                let snap = snapshot();
                jobs.fileops.cut_selection(snap.clone(), &self.selection);
                #[cfg(target_os = "linux")]
                if let Some(clipboard) = &mut jobs.file_clipboard {
                    clipboard.copy(snap, &self.selection);
                }
            }
            #[cfg(target_os = "linux")]
            Cmd::Copy => self.copy_selected_files(jobs),
            #[cfg(target_os = "linux")]
            Cmd::Paste => self.paste_files(jobs),
            #[cfg(not(target_os = "linux"))]
            Cmd::Copy | Cmd::Paste => {}
            Cmd::SelectAll => self.apply_selection(SelectAction::SelectAll, false),
            Cmd::ToggleHidden => self.toggle_hidden(),
            Cmd::SortName => self.sort_by(SortKey::Name),
            Cmd::SortType => self.sort_by(SortKey::Kind),
            Cmd::SortModified => self.sort_by(SortKey::Modified),
            Cmd::SortSize => self.sort_by(SortKey::Size),
            Cmd::FindFiles => self.open_find(ui),
            Cmd::GoHome => {
                let home = self.home.clone();
                self.navigate(home);
            }
            Cmd::GoUp => self.up(),
            Cmd::GoBack => self.history(false),
            Cmd::GoForward => self.history(true),
            Cmd::Bookmark => {
                if let Some(path) = self.location() {
                    jobs.places.toggle_bookmark(&path);
                }
            }
            Cmd::ToggleInspector => self.show_inspector = !self.show_inspector,
            Cmd::ToggleGrid => self.toggle_grid(),
            Cmd::CopyPath => {
                let path = self
                    .selected
                    .zip(self.browser.as_ref())
                    .and_then(|(i, b)| b.entries().get(i))
                    .map(|e| e.path.clone())
                    .or_else(|| self.location());
                if let Some(path) = path {
                    ui.set_clipboard(path.to_string_lossy().into_owned());
                }
            }
            Cmd::OpenTerminal => {
                if let Some(dir) = self.location() {
                    open_terminal(dir);
                }
            }
            Cmd::NewTab | Cmd::CloseTab | Cmd::ReopenTab | Cmd::NextTab | Cmd::PrevTab => {}
        }
    }
    /// Keep an inotify watch on the listed folder; re-list shortly after external changes.
    fn poll_watch(&mut self) -> bool {
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);
        let Some(browser) = self
            .browser
            .as_ref()
            .filter(|_| self.fixture_count.is_none())
        else {
            return false;
        };
        if !matches!(browser.state(), LoadState::Complete) {
            return false;
        }
        let path = browser.path().to_path_buf();
        if self.watch.as_ref().is_none_or(|(p, _)| *p != path) {
            self.watch = browser_core::watch::DirWatch::new(&path)
                .ok()
                .map(|w| (path.clone(), w));
            self.reload_at = None;
        }
        if self.watch.as_ref().is_some_and(|(_, w)| w.changed()) && self.reload_at.is_none() {
            self.reload_at = Some(std::time::Instant::now() + SETTLE);
        }
        if self
            .reload_at
            .is_some_and(|at| std::time::Instant::now() >= at)
        {
            self.reload_at = None;
            self.sizes.reset();
            // Keep the focused item selected across the re-list (full multi-selection
            // preservation would need path-keyed selection; documented limit).
            self.reveal_path = self
                .selected
                .and_then(|i| self.browser.as_ref()?.entries().get(i))
                .map(|e| e.path.clone());
            if let Some(b) = self.browser.as_mut() {
                b.navigate(path);
            }
            return true;
        }
        false
    }
    fn apply_settings(&mut self, settings: &settings::Settings) {
        if self.fixture_count.is_some() {
            return;
        }
        self.show_inspector = settings.inspector;
        self.grid = settings.grid;
        if let Some(b) = self.browser.as_mut() {
            b.set_show_hidden(settings.show_hidden);
            b.set_sort(settings.sort);
        }
    }
    fn current_settings(&self) -> Option<settings::Settings> {
        let b = self.browser.as_ref()?;
        Some(settings::Settings {
            show_hidden: b.show_hidden(),
            sort: b.sort(),
            inspector: self.show_inspector,
            grid: self.grid,
        })
    }
    fn toggle_hidden(&mut self) {
        if let Some(b) = self.browser.as_mut() {
            b.set_show_hidden(!b.show_hidden());
            self.reset_listing();
        }
    }
    fn sort_by(&mut self, key: SortKey) {
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        let current = browser.sort();
        browser.set_sort(SortOrder {
            key,
            descending: if current.key == key {
                !current.descending
            } else {
                matches!(key, SortKey::Size | SortKey::Modified)
            },
        });
        self.reset_listing();
    }
    fn open_rename(&mut self, jobs: &mut AppJobs) {
        if jobs.mutations_busy() {
            return;
        }
        if self.selection.count() > 1 {
            let snapshot = self.browser.as_ref().and_then(Browser::snapshot);
            jobs.fileops
                .batch_rename_selection(snapshot, &self.selection);
            return;
        }
        let entry = self
            .selected
            .zip(self.browser.as_ref())
            .and_then(|(index, b)| b.entries().get(index));
        if let Some(entry) = entry {
            jobs.fileops.open_prompt(
                fileops::PromptKind::Rename(entry.path.clone()),
                entry.name.to_string_lossy().into_owned(),
            );
        }
    }
    fn open_create(&mut self, jobs: &mut AppJobs, folder: bool) {
        if jobs.mutations_busy() {
            return;
        }
        if let Some(b) = self
            .browser
            .as_ref()
            .filter(|b| matches!(b.state(), LoadState::Complete))
        {
            let dir = b.path().to_path_buf();
            jobs.fileops.open_prompt(
                if folder {
                    fileops::PromptKind::NewFolder(dir)
                } else {
                    fileops::PromptKind::NewFile(dir)
                },
                if folder { "New folder" } else { "New file" }.into(),
            );
        }
    }
    fn count(&self) -> usize {
        self.fixture_count
            .unwrap_or_else(|| self.browser.as_ref().map_or(0, |b| b.entries().len()))
    }
    /// Items per list line: always 1 in list view.
    fn cols(&self) -> usize {
        if self.grid { self.grid_cols.max(1) } else { 1 }
    }
    fn lines(&self) -> usize {
        self.count().div_ceil(self.cols())
    }
    fn line_height(&self) -> f64 {
        if self.grid { TILE_H } else { ROW }
    }
    fn toggle_grid(&mut self) {
        self.grid = !self.grid;
        self.list = ListState::uniform_count(self.lines(), self.line_height());
    }
    /// The second pane of a split: no tab strip, places rail, inspector, find, archives or job rows.
    fn secondary(&self) -> bool {
        !self.pfx.is_empty()
    }
    /// Send the selection to `dest` (the other pane's folder).
    fn send_to(&mut self, jobs: &mut AppJobs, dest: PathBuf, mv: bool) {
        let snap = self.browser.as_ref().and_then(Browser::snapshot);
        jobs.fileops.send_selection(snap, &self.selection, dest, mv);
    }
    fn pid(&self, id: &str) -> String {
        format!("{}{id}", self.pfx)
    }
    fn list_id(&self) -> String {
        format!("{}file-list/revision/{}", self.pfx, self.listing_serial)
    }
    fn reset_listing(&mut self) {
        // Reused index-shaped hit IDs must never dispatch a preceding listing's
        // pointer edge into a new same-sized listing. Row IDs share this epoch.
        self.listing_serial = self
            .listing_serial
            .checked_add(1)
            .expect("listing UI identity space exhausted");
        let revision = self
            .browser
            .as_ref()
            .and_then(Browser::snapshot)
            .map_or_else(ListingRevision::new, |snapshot| snapshot.revision().clone());
        self.selection.reset(revision, self.count());
        self.list = ListState::uniform_count(self.lines(), self.line_height());
        self.selected = None;
        self.resize_reveal = None;
        self.discard_listing_input = true;
    }
    fn apply_selection(&mut self, action: SelectAction, reveal: bool) {
        let revision = self.selection.revision().clone();
        self.selection
            .apply(&revision, action)
            .expect("current listing selection action");
        self.selected = self
            .selection
            .cursor(&revision)
            .expect("current listing cursor");
        if reveal && let Some(index) = self.selected {
            self.list.focus_item(index / self.cols());
        }
    }
    fn refresh(&mut self) -> bool {
        let changed = self.browser.as_mut().is_some_and(Browser::poll);
        if changed {
            self.reset_listing();
            if let Some(path) = self.reveal_path.take() {
                self.selected = self
                    .browser
                    .as_ref()
                    .and_then(|b| b.entries().iter().position(|entry| entry.path == path));
                if let Some(index) = self.selected {
                    self.apply_selection(
                        SelectAction::Click {
                            index,
                            modifiers: SelectMods::default(),
                        },
                        true,
                    );
                }
            }
        }
        changed
    }
    fn navigate(&mut self, path: PathBuf) {
        self.reveal_path = None;
        if let Some(browser) = self.browser.as_mut() {
            browser.navigate(path);
            self.path_text = browser.path().to_string_lossy().into_owned();
            self.selected = None;
            self.filter.clear();
            browser.set_filter("");
            self.reset_listing();
        }
    }
    fn info(&self, index: usize) -> Option<RowInfo> {
        if let Some(count) = self.fixture_count {
            return (index < count).then(|| fixture_row(index));
        }
        self.browser
            .as_ref()?
            .entries()
            .get(index)
            .map(|e| RowInfo {
                name: e.name.to_string_lossy().into_owned(),
                path: e.path.to_string_lossy().into_owned(),
                kind: e.kind,
                size: e.size,
                modified: e.modified,
            })
    }
    /// The last few folders on the way to the current one: (index, label, path).
    fn crumbs(&self) -> Vec<(usize, String, PathBuf)> {
        let Some(path) = self.browser.as_ref().map(|b| b.path().to_path_buf()) else {
            return Vec::new();
        };
        let mut all: Vec<_> = path.ancestors().map(FsPath::to_path_buf).collect();
        all.reverse();
        let skip = all.len().saturating_sub(6);
        all.into_iter()
            .enumerate()
            .skip(skip)
            .map(|(i, p)| {
                let name = p
                    .file_name()
                    .map_or("/".into(), |n| n.to_string_lossy().into_owned());
                (i, name, p)
            })
            .collect()
    }
    fn activate(&mut self, index: usize) {
        let entry = self
            .browser
            .as_ref()
            .and_then(|b| b.entries().get(index))
            .map(|e| (e.path.clone(), e.kind));
        match entry {
            Some((path, EntryKind::Directory)) => self.navigate(path),
            Some((path, EntryKind::File)) if zip_path(&path) && !self.secondary() => {
                self.find = None;
                self.archive = Some(ArchivePanel::new(path));
            }
            Some((path, EntryKind::File)) => open_external(path),
            _ => {}
        }
    }
    /// Jump to the first entry whose name starts with the letters typed within the last second.
    fn type_ahead(&mut self, c: char) {
        let now = std::time::Instant::now();
        if self
            .ahead
            .1
            .is_none_or(|t| now.duration_since(t).as_secs_f32() > 1.)
        {
            self.ahead.0.clear();
        }
        self.ahead = (
            std::mem::take(&mut self.ahead.0) + &c.to_lowercase().to_string(),
            Some(now),
        );
        let prefix = self.ahead.0.clone();
        let hit = self.browser.as_ref().and_then(|b| {
            b.entries()
                .iter()
                .position(|e| e.name.to_string_lossy().to_lowercase().starts_with(&prefix))
        });
        if let Some(index) = hit {
            self.apply_selection(
                SelectAction::Move {
                    movement: Movement::To(index),
                    modifiers: SelectMods::default(),
                },
                true,
            );
        }
    }
    fn shortcut(&mut self, ui: &mut Ui, jobs: &mut AppJobs) {
        let mut keys = ui.shortcuts().to_vec();
        if ui.focus_is_text() {
            keys.extend(
                ui.keys(ui.focus_key().unwrap_or(&self.pid("path-input")))
                    .iter()
                    .filter(|p| {
                        (p.mods.ctrl || p.mods.cmd) && matches!(p.key, Key::Char('l' | 'f' | 'F'))
                    })
                    .copied(),
            );
        }
        let files_focused = ui
            .focus_key()
            .is_none_or(|id| id.starts_with(&self.pid("file-list")));
        if let Some(id) = ui
            .focus_key()
            .filter(|id| id.starts_with(&self.pid("file-list")))
        {
            for c in ui
                .text(id)
                .to_owned()
                .chars()
                .filter(|c| !c.is_control() && *c != ' ')
            {
                self.type_ahead(c);
            }
        }
        for press in keys {
            let control = press.mods.ctrl || press.mods.cmd;
            if files_focused && !press.mods.alt {
                let modifiers = SelectMods {
                    control,
                    shift: press.mods.shift,
                };
                let cols = self.cols() as isize;
                let page =
                    (self.viewport.height / self.line_height()).floor().max(1.) as isize * cols;
                let movement = match press.key {
                    Key::Up => Some(Movement::Offset(-cols)),
                    Key::Down => Some(Movement::Offset(cols)),
                    Key::Left if self.grid => Some(Movement::Offset(-1)),
                    Key::Right if self.grid => Some(Movement::Offset(1)),
                    Key::Home => Some(Movement::First),
                    Key::End => Some(Movement::Last),
                    Key::PageUp => Some(Movement::Offset(-page)),
                    Key::PageDown => Some(Movement::Offset(page)),
                    _ => None,
                };
                if let Some(movement) = movement {
                    self.apply_selection(
                        SelectAction::Move {
                            movement,
                            modifiers,
                        },
                        true,
                    );
                } else if control && matches!(press.key, Key::Char('a' | 'A')) {
                    self.apply_selection(SelectAction::SelectAll, false);
                } else if control && press.mods.shift && matches!(press.key, Key::Char('c' | 'C')) {
                    self.run_command(ui, jobs, palette::Cmd::CopyPath);
                } else if control && matches!(press.key, Key::Char('c' | 'C')) {
                    #[cfg(target_os = "linux")]
                    self.copy_selected_files(jobs);
                } else if control && matches!(press.key, Key::Char('x' | 'X')) {
                    let snapshot = self.browser.as_ref().and_then(Browser::snapshot);
                    jobs.fileops
                        .cut_selection(snapshot.clone(), &self.selection);
                    #[cfg(target_os = "linux")]
                    if let Some(clipboard) = &mut jobs.file_clipboard {
                        clipboard.copy(snapshot, &self.selection);
                    }
                } else if control && matches!(press.key, Key::Char('v' | 'V')) {
                    #[cfg(target_os = "linux")]
                    self.paste_files(jobs);
                } else if !control && press.key == Key::Delete {
                    let snapshot = self.browser.as_ref().and_then(Browser::snapshot);
                    jobs.fileops.trash_selection(snapshot, &self.selection);
                } else if !control && press.key == Key::Function(2) {
                    self.open_rename(jobs);
                } else if control && matches!(press.key, Key::Char('z' | 'Z')) {
                    jobs.fileops.undo_last();
                } else if control && matches!(press.key, Key::Char('n' | 'N')) {
                    self.open_create(jobs, press.mods.shift);
                } else if control && press.key == Key::Space {
                    self.apply_selection(SelectAction::ToggleCursor, false);
                } else if !control
                    && press.key == Key::Enter
                    && let Some(index) = self.selected
                {
                    self.activate(index);
                }
            }
            if (press.mods.ctrl || press.mods.cmd)
                && matches!(press.key, Key::Char('d' | 'D'))
                && self.fixture_count.is_none()
                && let Some(path) = self.location()
            {
                jobs.places.toggle_bookmark(&path);
            }
            if (press.mods.ctrl || press.mods.cmd) && matches!(press.key, Key::Char('g' | 'G')) {
                self.toggle_grid();
            }
            if (press.mods.ctrl || press.mods.cmd) && matches!(press.key, Key::Char('h' | 'H')) {
                self.toggle_hidden();
            }
            if (press.mods.ctrl || press.mods.cmd) && press.key == Key::Char('l') {
                ui.focus(self.pid("path-input"));
            }
            if (press.mods.ctrl || press.mods.cmd)
                && matches!(press.key, Key::Char('f' | 'F'))
                && self.fixture_count.is_none()
            {
                if press.mods.shift {
                    self.open_find(ui);
                } else {
                    ui.focus(self.pid("filter-input"));
                }
            }
            if press.key == Key::Escape {
                jobs.fileops.cancel_prompt();
                self.apply_selection(SelectAction::Clear, false);
                ui.focus(self.pid("file-list"));
            }
            if files_focused
                && self.inspector_fits()
                && !press.mods.alt
                && !control
                && press.key == Key::Space
            {
                self.show_inspector = !self.show_inspector;
            }
            if press.mods.alt && press.key == Key::Up {
                self.up();
            }
            if press.mods.alt && press.key == Key::Left {
                self.history(false);
            }
            if press.mods.alt && press.key == Key::Right {
                self.history(true);
            }
        }
    }
    fn open_find(&mut self, ui: &mut Ui) {
        if self.fixture_count.is_none()
            && !self.secondary()
            && let Some(browser) = &self.browser
        {
            self.find = Some(FindPanel::new(browser.path().to_path_buf()));
            self.archive = None;
            ui.focus("find-query");
        }
    }
    fn build_find(&mut self, ui: &mut Ui) -> Option<El> {
        let (tree, action) = self.find.as_mut()?.build(ui);
        match action {
            None => return Some(tree),
            Some(FindAction::Close) => self.find = None,
            Some(FindAction::Open { path, kind }) => {
                self.find = None;
                if kind == browser_search::EntryKind::Directory {
                    self.navigate(path);
                } else if let Some(parent) = path.parent() {
                    self.navigate(parent.to_path_buf());
                    self.reveal_path = Some(path);
                }
            }
        }
        ui.focus(self.pid("file-list"));
        None
    }
    fn inspector_fits(&self) -> bool {
        let rail = self.show_rail && self.window.width >= RAIL_MIN + CONTENT_MIN + GAP;
        !self.secondary()
            && self.window.width
                >= CONTENT_MIN + INSPECTOR_MIN + GAP + if rail { self.rail_width + GAP } else { 0. }
    }
    fn build_archive(&mut self, ui: &mut Ui, jobs: &mut AppJobs) -> Option<El> {
        let busy = jobs.mutations_busy();
        self.archive.as_mut()?.set_mutations_busy(busy);
        let (tree, action) = self.archive.as_mut()?.build(ui);
        let focus = match action {
            None => return Some(tree),
            Some(ArchiveAction::Close) => "file-list",
            Some(ArchiveAction::GoTo) => "path-input",
            Some(ArchiveAction::Extract(selection)) => {
                if !jobs.mutations_busy() {
                    let destination = self
                        .browser
                        .as_ref()
                        .map(|browser| browser.path().join("Extracted ZIP"))
                        .unwrap_or_else(|| self.home.join("Extracted ZIP"));
                    let extraction = jobs.extraction.get_or_insert_with(ExtractionUi::new);
                    self.archive.as_ref().unwrap().prepare_extraction(
                        extraction,
                        selection,
                        destination,
                    );
                }
                // A newly prepared controller was not the owner of this input
                // batch. Its future raw-input editor must receive no old input.
                return jobs.build_extraction(ui, &Input::default()).or(Some(tree));
            }
        };
        self.archive = None;
        self.discard_listing_input = true;
        ui.focus(focus);
        None
    }
    fn up(&mut self) {
        if let Some(b) = self.browser.as_mut() {
            b.up();
            b.set_filter("");
            self.path_text = b.path().to_string_lossy().into_owned();
            self.selected = None;
            self.filter.clear();
            self.reset_listing();
        }
    }
    fn history(&mut self, forward: bool) {
        if let Some(b) = self.browser.as_mut() {
            if forward {
                b.forward();
            } else {
                b.back();
            }
            b.set_filter("");
            self.path_text = b.path().to_string_lossy().into_owned();
            self.selected = None;
            self.filter.clear();
            self.reset_listing();
        }
    }
    fn evidence(&self, jobs: &AppJobs) -> String {
        if let Some(extraction) = jobs.extraction.as_ref().filter(|panel| panel.visible()) {
            return extraction.evidence();
        }
        #[cfg(target_os = "linux")]
        if let Some(transfers) = jobs.copy_transfers.as_ref().filter(|t| t.visible()) {
            return transfers.evidence();
        }
        if let Some(find) = &self.find {
            return find.evidence();
        }
        if let Some(archive) = &self.archive {
            return archive.evidence();
        }
        let ranges = self
            .selection
            .ranges(self.selection.revision())
            .expect("current listing ranges");
        let bounded_ranges = ranges
            .iter()
            .take(16)
            .map(|range| format!("[{},{}]", range.start, range.end))
            .collect::<Vec<_>>()
            .join(",");
        let cursor_selected = self.selected.is_some_and(|index| {
            self.selection
                .contains(self.selection.revision(), index)
                .expect("current listing cursor")
        });
        format!(
            "{{\"schema_version\":1,\"mode\":\"{}\",\"model_count\":{},\"rows_built\":{},\"selected_index\":{},\"selected_count\":{},\"selection_range_count\":{},\"selection_ranges\":[{}],\"selection_ranges_complete\":{},\"cursor_selected\":{},\"inspector_open\":{},\"focus_role\":\"{}\",\"location_text_bytes\":{},\"file_clipboard\":{},\"visible_start\":{},\"visible_end\":{},\"os_font_fallback_count\":{},\"os_font_fallback_bytes\":{},\"row_height\":{},\"overscan\":{},\"viewport_width\":{},\"viewport_height\":{},\"window_width\":{},\"window_height\":{},\"layout\":\"intrinsic flex rows and columns; measured collection viewport\",\"limitations\":[\"Uniform list state is O(1); catalogs and fragmented selection can grow with item count\",\"Native parity and latency unqualified; copy is bounded and per-item\"]}}",
            if self.fixture_count.is_some() {
                "synthetic"
            } else {
                "filesystem"
            },
            self.count(),
            self.rows_built,
            self.selected
                .map(|index| index.to_string())
                .unwrap_or_else(|| "null".into()),
            self.selection.count(),
            ranges.len(),
            bounded_ranges,
            ranges.len() <= 16,
            cursor_selected,
            self.show_inspector && self.inspector_fits(),
            self.evidence_focus_role,
            self.path_text.len(),
            jobs.clipboard_evidence(),
            self.visible.start,
            self.visible.end,
            OS_FALLBACKS.get().map_or(0, Vec::len),
            OS_FALLBACKS
                .get()
                .map_or(0, |fonts| fonts.iter().map(|f| f.bytes).sum::<u64>()),
            ROW,
            OVERSCAN,
            self.viewport.width,
            self.viewport.height,
            self.window.width,
            self.window.height
        )
    }
    fn rail(&mut self, ui: &mut Ui, places: &places::Places) -> El {
        let bookmarks = [
            ("home", "Home", self.home.clone(), Glyph::Home),
            (
                "documents",
                "Documents",
                self.home.join("Documents"),
                Glyph::Folder,
            ),
            (
                "downloads",
                "Downloads",
                self.home.join("Downloads"),
                Glyph::Folder,
            ),
            (
                "root",
                "Filesystem",
                PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }),
                Glyph::Folder,
            ),
        ];
        let mut children = vec![detail("Quick access").pad((10, 8))];
        for (key, name, path, icon) in bookmarks {
            let id = format!("bookmark-{key}");
            if self.fixture_count.is_none() && clicked(ui, &id) {
                self.navigate(path.clone());
            }
            let current = self.browser.as_ref().is_some_and(|b| b.path() == path);
            let item = row([
                glyph(
                    icon,
                    if matches!(icon, Glyph::Folder) {
                        color(0xe2bd55)
                    } else {
                        muted()
                    },
                ),
                label(name).grow(1).min_w(0),
            ])
            .gap(8)
            .pad((10, 6));
            children.push(
                action(
                    ui,
                    &id,
                    &format!("Open {name}"),
                    item,
                    self.fixture_count.is_none(),
                    current,
                )
                .w(pct(100))
                .pad(0),
            );
        }
        let sections: [(&str, &str, PlaceItems); 3] = [
            (
                "bookmark",
                "Bookmarks · Ctrl+D",
                places
                    .bookmarks
                    .iter()
                    .map(|p| (place_name(p), p.clone()))
                    .collect(),
            ),
            (
                "volume",
                "Volumes",
                places
                    .volumes()
                    .into_iter()
                    .map(|v| (v.name, v.path))
                    .collect(),
            ),
            (
                "recent",
                "Recent",
                places
                    .recent
                    .iter()
                    .map(|p| (place_name(p), p.clone()))
                    .collect(),
            ),
        ];
        for (kind, title, items) in sections {
            if items.is_empty() {
                continue;
            }
            children.push(detail(title).pad((10, 8)));
            for (index, (name, path)) in items.into_iter().enumerate() {
                let id = format!("place-{kind}-{index}");
                if self.fixture_count.is_none() && clicked(ui, &id) {
                    self.navigate(path.clone());
                }
                let current = self.browser.as_ref().is_some_and(|b| b.path() == path);
                children.push(
                    action(
                        ui,
                        &id,
                        &format!("Open {}", path.display()),
                        row([
                            glyph(Glyph::Folder, color(0xe2bd55)),
                            label(name).grow(1).min_w(0).lines(1).clip(),
                        ])
                        .gap(8)
                        .pad((10, 6)),
                        self.fixture_count.is_none(),
                        current,
                    )
                    .w(pct(100))
                    .pad(0),
                );
            }
        }
        children.push(spacer());
        children.push(detail("File browser").pad((10, 8)));
        col(children)
            .w(self.rail_width)
            .min_w(RAIL_MIN)
            .h(pct(100))
            .fill(chrome())
            .clip()
            .scroll()
            .id("places")
    }
    fn inspector(&self, ui: &Ui) -> El {
        let content = self.selected.and_then(|i| self.info(i));
        let header = row([
            label("Inspector").grow(1),
            action(
                ui,
                "close-inspector",
                "Hide inspector",
                glyph(Glyph::Close, muted()),
                true,
                false,
            ),
        ])
        .align(Align::Center)
        .pad((10, 6));
        let mut fields = vec![header, hairline()];
        if let Some(info) = content {
            fields.push(
                col([
                    glyph(
                        if info.kind == EntryKind::Directory {
                            Glyph::Folder
                        } else {
                            Glyph::File
                        },
                        if info.kind == EntryKind::Directory {
                            color(0xe2bd55)
                        } else {
                            muted()
                        },
                    )
                    .square(48),
                    label(info.name.clone()).wrap().w(pct(100)),
                    detail(info.kind_label()),
                ])
                .gap(10)
                .pad((14, 22))
                .align(Align::Center),
            );
            fields.push(hairline());
            fields.push(
                col([
                    detail("Location"),
                    label(info.path).wrap().w(pct(100)),
                    detail("Size"),
                    label(info.size.map(bytes).unwrap_or_else(|| "Not loaded".into())),
                ])
                .gap(8)
                .pad(14),
            );
            let preview = self.preview.view(self.inspector_width);
            let has_preview = preview.is_some();
            if let Some(view) = preview {
                fields.push(hairline());
                fields.push(view);
            }
            fields.push(spacer());
            fields.push(
                detail(if self.fixture_count.is_some() {
                    "Synthetic entry · no file on disk"
                } else if self
                    .selected
                    .and_then(|i| self.browser.as_ref()?.entries().get(i))
                    .is_some_and(|entry| entry.kind == EntryKind::File && zip_path(&entry.path))
                {
                    "Press Enter or double-click to browse this ZIP."
                } else if has_preview {
                    ""
                } else {
                    "Special files have no preview."
                })
                .wrap()
                .pad(14),
            );
        } else {
            fields.push(
                col([
                    glyph(Glyph::Inspector, muted()).square(32),
                    label("No focused item"),
                    detail("Choose an item to inspect its details.").wrap(),
                ])
                .gap(12)
                .pad(20),
            );
            fields.push(spacer());
        }
        col(fields)
            .w(self.inspector_width)
            .min_w(INSPECTOR_MIN)
            .h(pct(100))
            .fill(chrome())
            .clip()
            .scroll()
            .id("inspector")
            .named("Selection inspector")
    }
}

struct RowInfo {
    name: String,
    path: String,
    kind: EntryKind,
    size: Option<u64>,
    modified: Option<std::time::SystemTime>,
}
impl RowInfo {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            EntryKind::Directory => "Folder",
            EntryKind::File => "File",
            EntryKind::Symlink => "Symbolic link",
            EntryKind::Other => "Other",
        }
    }
}
fn fixture_row(index: usize) -> RowInfo {
    let named = [
        "Applications",
        "Archive",
        "Audio",
        "Documents",
        "Downloads",
        "Projects",
        "README.md",
        "Cargo.toml",
        "design-system.fig",
        "field-recording.wav",
        "release-notes.txt",
        "A long filename that keeps the file column intrinsic and clips at its boundary.md",
        "résumé.pdf",
        "日本語のファイル.txt",
        "مرحبا.txt",
    ];
    let name = if index < named.len() {
        named[index].into()
    } else {
        format!("document-{index:07}.txt")
    };
    RowInfo {
        path: format!("/Synthetic/Performance fixture/{name}"),
        name,
        kind: if index < 6 {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        size: if index < 6 {
            None
        } else {
            Some((index as u64 + 1) * 12_947)
        },
        modified: Some(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(1_760_000_000 + index as u64 * 3_600),
        ),
    }
}
/// ponytail: UTC; local time needs a tz database, add when users ask.
fn date(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / 86_400) as i64 + 719_468, secs % 86_400);
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}
type PlaceItems = Vec<(String, PathBuf)>;
/// Hand a file to the desktop's default handler. Detached; the worker only reaps the child.
fn open_external(path: PathBuf) {
    std::thread::spawn(move || {
        if let Ok(mut child) = std::process::Command::new("xdg-open")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            let _ = child.wait();
        }
    });
}
/// Start the user's terminal in `dir`: `$TERMINAL`, then common emulators. Detached.
fn open_terminal(dir: PathBuf) {
    std::thread::spawn(move || {
        let env = std::env::var("TERMINAL").ok();
        let candidates = env.iter().map(String::as_str).chain([
            "xdg-terminal-exec",
            "x-terminal-emulator",
            "kitty",
            "alacritty",
            "konsole",
            "gnome-terminal",
            "xfce4-terminal",
            "xterm",
        ]);
        for program in candidates {
            if let Ok(mut child) = std::process::Command::new(program)
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let _ = child.wait();
                return;
            }
        }
    });
}
fn place_name(path: &FsPath) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}
fn bytes(size: u64) -> String {
    if size >= 1_073_741_824 {
        format!("{:.1} GB", size as f64 / 1_073_741_824.)
    } else if size >= 1_048_576 {
        format!("{:.1} MB", size as f64 / 1_048_576.)
    } else if size >= 1024 {
        format!("{:.1} KB", size as f64 / 1024.)
    } else {
        format!("{size} B")
    }
}

fn zip_path(path: &FsPath) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
}

impl TabView {
    fn build(&mut self, ui: &mut Ui, jobs: &mut AppJobs, strip: Option<El>) -> El {
        if std::mem::take(&mut jobs.copy_return_pending) {
            self.discard_listing_input = true;
            if self.find.is_some() {
                ui.focus("find-query");
            } else if let Some(archive) = &mut self.archive {
                archive.restore_focus();
            } else {
                ui.focus(self.pid("file-list"));
            }
        }
        let leaving_subview = self.find.is_some() || self.archive.is_some();
        if let Some(tree) = self.build_find(ui) {
            return jobs.with_mutation_status(ui, tree);
        }
        if let Some(tree) = self.build_archive(ui, jobs) {
            return jobs.with_mutation_status(ui, tree);
        }
        self.refresh();
        let preview_path = self
            .selected
            .filter(|_| self.show_inspector && !self.secondary() && self.fixture_count.is_none())
            .and_then(|i| self.browser.as_ref()?.entries().get(i))
            .filter(|e| matches!(e.kind, EntryKind::File | EntryKind::Directory))
            .map(|e| e.path.clone());
        self.preview.show(preview_path.as_deref());
        for (id, key) in [
            ("sort-name", SortKey::Name),
            ("sort-kind", SortKey::Kind),
            ("sort-modified", SortKey::Modified),
            ("sort-size", SortKey::Size),
        ] {
            if clicked(ui, &self.pid(id)) {
                self.sort_by(key);
            }
        }
        let discard_listing_input = std::mem::take(&mut self.discard_listing_input);
        if discard_listing_input
            && ui
                .focus_key()
                .is_none_or(|id| id.starts_with(&self.pid("file-list")))
        {
            ui.focus(self.pid("file-list"));
        }
        if let Some(scene) = ui.scene() {
            if let Some(frame) = scene.surface(&self.pid("browser-root")) {
                self.window = frame.frame.size;
            }
            if let Some(frame) = scene.surface(&self.pid("file-list")) {
                let size = frame.frame.size;
                if size.width > 0. && size.height > 0. {
                    self.viewport = size;
                }
            }
        }
        if let Some(index) = self
            .resize_reveal
            .take()
            .filter(|index| *index < self.count())
        {
            self.list
                .scroll_to_item(index / self.cols(), ScrollTo::Nearest);
        }
        if !leaving_subview && !discard_listing_input && self.active_pane {
            self.shortcut(ui, jobs);
        }
        if let Some(tree) = self.build_find(ui) {
            return jobs.with_mutation_status(ui, tree);
        }
        if let Some(tree) = self.build_archive(ui, jobs) {
            return jobs.with_mutation_status(ui, tree);
        }
        if clicked(ui, &self.pid("find-anywhere")) {
            self.open_find(ui);
            if let Some(tree) = self.build_find(ui) {
                return jobs.with_mutation_status(ui, tree);
            }
        }
        if clicked(ui, &self.pid("nav-back")) {
            self.history(false);
        }
        if clicked(ui, &self.pid("nav-forward")) {
            self.history(true);
        }
        if clicked(ui, &self.pid("nav-up")) {
            self.up();
        }
        if clicked(ui, &self.pid("path-click")) {
            ui.focus(self.pid("path-input"));
        }
        if clicked(ui, &self.pid("menu")) {
            self.context_requested = true;
        }
        if let Some(target) = self
            .crumbs()
            .into_iter()
            .find_map(|(i, _, p)| clicked(ui, &self.pid(&format!("crumb/{i}"))).then_some(p))
        {
            self.navigate(target);
        }
        #[cfg(target_os = "linux")]
        if clicked(ui, &self.copy_files_id()) {
            self.copy_selected_files(jobs);
        }
        #[cfg(target_os = "linux")]
        if clicked(ui, &self.paste_files_id()) {
            self.paste_files(jobs);
        }
        if clicked(ui, "toggle-places") {
            self.show_rail = !self.show_rail;
        }
        if clicked(ui, "toggle-inspector") {
            self.show_inspector = !self.show_inspector;
        }
        if clicked(ui, "close-inspector") {
            self.show_inspector = false;
        }
        let rail_delta = ui.get("rail-divider").drag_delta.x;
        let inspector_delta = ui.get("inspector-divider").drag_delta.x;
        self.rail_width = (self.rail_width + rail_delta).clamp(RAIL_MIN, 280.);
        self.inspector_width = (self.inspector_width - inspector_delta).clamp(INSPECTOR_MIN, 420.);
        let real = self.fixture_count.is_none();
        let can_back = self.browser.as_ref().is_some_and(|b| b.can_back());
        let can_forward = self.browser.as_ref().is_some_and(|b| b.can_forward());
        let can_up = self
            .browser
            .as_ref()
            .is_some_and(|b| b.path().parent().is_some());
        let nav_back = action(
            ui,
            &self.pid("nav-back"),
            "Back · Alt+Left",
            glyph(Glyph::Back, muted()),
            can_back,
            false,
        );
        let nav_forward = action(
            ui,
            &self.pid("nav-forward"),
            "Forward · Alt+Right",
            glyph(Glyph::Forward, muted()),
            can_forward,
            false,
        );
        let nav_up = action(
            ui,
            &self.pid("nav-up"),
            "Parent folder · Alt+Up",
            glyph(Glyph::Up, muted()),
            can_up,
            false,
        );
        let show_rail = !self.secondary()
            && self.show_rail
            && self.window.width >= RAIL_MIN + CONTENT_MIN + GAP;
        let show_inspector = self.show_inspector && self.inspector_fits();
        let places = action(
            ui,
            "toggle-places",
            "Toggle places",
            glyph(Glyph::Home, muted()),
            self.window.width >= RAIL_MIN + CONTENT_MIN + GAP,
            show_rail,
        );
        let inspector = action(
            ui,
            "toggle-inspector",
            if self.inspector_fits() {
                "Toggle inspector · Space"
            } else {
                "Inspector needs more space; hide Places or widen the window"
            },
            glyph(Glyph::Inspector, muted()),
            self.inspector_fits(),
            show_inspector,
        );
        let pfx = self.pfx;
        let crumb_count = self.crumbs().last().map_or(0, |c| c.0 + 1);
        let crumbs = row(self
            .crumbs()
            .into_iter()
            .enumerate()
            .flat_map(|(n, (i, name, p))| {
                let last = i + 1 == crumb_count;
                let button = action(
                    ui,
                    &format!("{pfx}crumb/{i}"),
                    &p.to_string_lossy(),
                    text(name)
                        .text_size(13)
                        .fill(if last { ink() } else { muted() }),
                    real,
                    false,
                )
                .fill(ground());
                if n == 0 {
                    vec![button]
                } else {
                    vec![detail("›"), button]
                }
            }))
        .gap(2)
        .align(Align::Center)
        .shrink(1)
        .min_w(0)
        .clip();
        let editing = ui.focus_key().is_some_and(|k| k == self.pid("path-input"));
        let path = if editing {
            let path = text_edit(
                ui,
                self.pid("path-input"),
                &mut self.path_text,
                TextOpts::default(),
            );
            if real && path.changed.submitted {
                let target = self
                    .browser
                    .as_ref()
                    .filter(|b| b.path().to_string_lossy() == self.path_text)
                    .map(|b| b.path().to_path_buf())
                    .unwrap_or_else(|| {
                        let path = PathBuf::from(&self.path_text);
                        if path.is_absolute() {
                            path
                        } else {
                            self.browser
                                .as_ref()
                                .map(|b| b.path().join(&path))
                                .unwrap_or(path)
                        }
                    });
                self.navigate(target);
                ui.focus(self.pid("file-list"));
            }
            path.el
                .grow(1)
                .basis(0)
                .min_w(0)
                .clip()
                .named("Location · Enter to navigate · Ctrl+L")
                .fill(ground())
        } else {
            row([
                crumbs,
                col(Vec::<El>::new())
                    .grow(1)
                    .h(pct(100))
                    .min_w(8)
                    .id(self.pid("path-click"))
                    .focusable()
                    .cursor(Cursor::Text),
            ])
            .grow(1)
            .basis(0)
            .min_w(0)
            .h(28)
            .pad((8, 0))
            .align(Align::Center)
            .radius(3)
            .clip()
            .named("Location · click to edit · Ctrl+L")
            .fill(ground())
        }
        .when(!real, |e| e.disabled());
        let find_action = action(
            ui,
            &self.pid("find-anywhere"),
            "Find anywhere · Ctrl+Shift+F",
            glyph(Glyph::Search, muted()),
            real,
            false,
        );
        let mut toolbar_items = vec![nav_back, nav_forward, nav_up];
        if !self.secondary() {
            toolbar_items.push(places);
        }
        toolbar_items.push(path);
        #[cfg(target_os = "linux")]
        if let Some(clipboard) = &jobs.file_clipboard {
            toolbar_items.push(action(
                ui,
                &self.copy_files_id(),
                clipboard.availability(),
                label("Copy"),
                real && self.selection.count() > 0 && clipboard.can_copy(),
                false,
            ));
        }
        if !self.secondary() {
            toolbar_items.extend([find_action, inspector]);
        }
        toolbar_items.push(action(
            ui,
            &self.pid("menu"),
            "Commands · Ctrl+P",
            col([
                block(3, 3).radius(2).fill(muted()),
                block(3, 3).radius(2).fill(muted()),
                block(3, 3).radius(2).fill(muted()),
            ])
            .gap(2),
            real,
            false,
        ));
        let toolbar = row(toolbar_items)
            .gap(3)
            .pad((6, 5))
            .align(Align::Center)
            .fill(chrome())
            .w(pct(100));
        let tab_name = if self.fixture_count.is_some() {
            "Performance fixture".into()
        } else {
            self.browser
                .as_ref()
                .and_then(|b| b.path().file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Filesystem".into())
        };
        let tab = row([
            glyph(Glyph::Folder, color(0xe2bd55)),
            label(tab_name).min_w(0).clip(),
        ])
        .gap(7)
        .pad((12, 8))
        .fill(ground())
        .radius(3)
        .min_w(0)
        .shrink(1)
        .named("Current folder tab");
        let tabs = strip.unwrap_or_else(|| {
            row([tab, spacer(), detail("Local files").pad((10, 0))])
                .align(Align::Center)
                .fill(chrome())
                .w(pct(100))
        });
        let wide_columns = self.viewport.width >= 500.;
        let modified_column = self.viewport.width >= 700.;
        let current = self.browser.as_ref().map(Browser::sort).unwrap_or_default();
        let head = |ui: &Ui, key: SortKey, id: &str, title: &str| {
            let arrow = match (current.key == key, current.descending) {
                (true, false) => " ▲",
                (true, true) => " ▼",
                _ => "",
            };
            row([detail(format!("{title}{arrow}"))])
                .id(id)
                .a11y(A11y::Button)
                .named(format!("Sort by {title}"))
                .focusable()
                .cursor(Cursor::Hand)
                .when(ui.get(id).hovered, |e| e.fill(color(0x293235)))
        };
        let mut head_cells = vec![
            head(ui, SortKey::Name, &self.pid("sort-name"), "Name")
                .grow(1)
                .basis(0)
                .min_w(0),
        ];
        if wide_columns {
            head_cells.push(
                head(ui, SortKey::Kind, &self.pid("sort-kind"), "Type")
                    .reserve("Symbolic link")
                    .min_w(94),
            );
        }
        if modified_column {
            head_cells.push(
                head(
                    ui,
                    SortKey::Modified,
                    &self.pid("sort-modified"),
                    "Modified",
                )
                .reserve("0000-00-00 00:00")
                .min_w(118),
            );
        }
        head_cells.push(
            head(ui, SortKey::Size, &self.pid("sort-size"), "Size")
                .reserve("999.9 MB")
                .min_w(78)
                .align_self(Align::End),
        );
        let header = row(head_cells)
            .gap(12)
            .pad((12, 7))
            .fill(chrome())
            .w(pct(100))
            .shrink(0);
        if self.grid {
            let cols = ((self.viewport.width - 8.) / TILE_W).floor().max(1.) as usize;
            if cols != self.grid_cols {
                self.grid_cols = cols;
                self.list = ListState::uniform_count(self.lines(), self.line_height());
            }
        }
        if self.list.len() != self.lines() {
            self.reset_listing();
        }
        let list_id = self.list_id();
        let fixture = self.fixture_count;
        let entries = self.browser.as_ref().map(|b| b.entries());
        let (grid, cols, count) = (self.grid, self.cols(), self.count());
        let thumbs = &mut self.thumbs;
        if let Some(b) = self.browser.as_ref().filter(|_| fixture.is_none()) {
            self.sizes.follow(b.path());
        }
        let sizes = &self.sizes;
        let mut wanted: Vec<PathBuf> = Vec::new();
        let revision = self.selection.revision().clone();
        let selection = &mut self.selection;
        let discard_listing_input = discard_listing_input || self.discard_listing_input;
        let mut pending_activation = None;
        let mut pending_selection = None;
        let mut context_request = false;
        let list = uniform_list(
            ui,
            &list_id,
            &mut self.list,
            ListOptions {
                overscan: OVERSCAN,
                keyboard: false,
                ..ListOptions::new(self.viewport.width.max(1.), self.viewport.height.max(1.))
            },
            |ui, item| {
                if grid {
                    let first = item.index * cols;
                    let mut tiles = Vec::new();
                    for index in first..(first + cols).min(count) {
                        let info = if fixture.is_some() {
                            fixture_row(index)
                        } else {
                            let e = &entries.expect("live browser entries")[index];
                            RowInfo {
                                name: e.name.to_string_lossy().into_owned(),
                                path: e.path.to_string_lossy().into_owned(),
                                kind: e.kind,
                                size: e.size,
                                modified: e.modified,
                            }
                        };
                        let tile_id = format!("{}/tile/{index}", item.id);
                        let interaction = ui.get(&tile_id);
                        if !discard_listing_input
                            && interaction.pressed
                            && matches!(
                                interaction.button,
                                Some(Button::Primary | Button::Secondary)
                            )
                        {
                            let mods = interaction.press_mods;
                            let secondary = interaction.button == Some(Button::Secondary);
                            if !secondary || !selection.contains(&revision, index).unwrap_or(false)
                            {
                                selection
                                    .apply(
                                        &revision,
                                        SelectAction::Click {
                                            index,
                                            modifiers: SelectMods {
                                                control: !secondary && (mods.ctrl || mods.cmd),
                                                shift: !secondary && mods.shift,
                                            },
                                        },
                                    )
                                    .expect("visible tile belongs to current listing");
                                pending_selection = Some(index);
                            }
                            context_request |= secondary;
                        }
                        if !discard_listing_input && interaction.double_clicked {
                            pending_activation = Some(index);
                        }
                        let active = selection
                            .contains(&revision, index)
                            .expect("visible tile belongs to current listing");
                        let thumb = (fixture.is_none()
                            && info.kind == EntryKind::File
                            && thumbs::is_image(FsPath::new(&info.path)))
                        .then(|| thumbs.get(FsPath::new(&info.path)))
                        .flatten();
                        tiles.push(
                            col([
                                match thumb {
                                    Some(img) => {
                                        col([]).w(52).h(40).fill(Fill::Image(img, Fit::Contain))
                                    }
                                    None => entry_glyph(
                                        info.kind == EntryKind::Directory,
                                        active,
                                        color(0xb9c9cf),
                                    )
                                    .square(36),
                                },
                                label(info.name.clone())
                                    .when(active, |e| e.fill(selected_ink()))
                                    .wrap()
                                    .lines(2)
                                    .w(pct(100))
                                    .align_self(Align::Center)
                                    .clip(),
                            ])
                            .gap(6)
                            .pad((4, 8))
                            .align(Align::Center)
                            .w(TILE_W - 8.)
                            .h(TILE_H - 8.)
                            .radius(4)
                            .fill(if active {
                                selected()
                            } else if interaction.hovered {
                                color(0x293235)
                            } else {
                                ground()
                            })
                            .id(tile_id)
                            .focusable()
                            .tip(info.name.clone())
                            .named(info.name)
                            .clip(),
                        );
                    }
                    return row(tiles).gap(8).pad((4, 4)).h(TILE_H).w(pct(100));
                }
                let info = if fixture.is_some() {
                    fixture_row(item.index)
                } else {
                    let e = &entries.expect("live browser entries")[item.index];
                    RowInfo {
                        name: e.name.to_string_lossy().into_owned(),
                        path: e.path.to_string_lossy().into_owned(),
                        kind: e.kind,
                        size: e.size,
                        modified: e.modified,
                    }
                };
                let interaction = ui.get(&item.id);
                if !discard_listing_input
                    && interaction.pressed
                    && interaction.button == Some(Button::Primary)
                {
                    let mods = interaction.press_mods;
                    selection
                        .apply(
                            &revision,
                            SelectAction::Click {
                                index: item.index,
                                modifiers: SelectMods {
                                    control: mods.ctrl || mods.cmd,
                                    shift: mods.shift,
                                },
                            },
                        )
                        .expect("visible row belongs to current listing");
                    pending_selection = Some(item.index);
                }
                if !discard_listing_input
                    && interaction.pressed
                    && interaction.button == Some(Button::Secondary)
                {
                    if !selection.contains(&revision, item.index).unwrap_or(false) {
                        selection
                            .apply(
                                &revision,
                                SelectAction::Click {
                                    index: item.index,
                                    modifiers: SelectMods::default(),
                                },
                            )
                            .expect("visible row belongs to current listing");
                        pending_selection = Some(item.index);
                    }
                    context_request = true;
                }
                if !discard_listing_input && interaction.double_clicked {
                    pending_activation = Some(item.index);
                }
                let active = selection
                    .contains(&revision, item.index)
                    .expect("visible row belongs to current listing");
                let name = row([
                    entry_glyph(info.kind == EntryKind::Directory, active, color(0xb9c9cf)),
                    label(info.name.clone())
                        .when(active, |e| e.fill(selected_ink()))
                        .lines(1)
                        .min_w(0)
                        .grow(1)
                        .basis(0)
                        .clip(),
                ])
                .gap(8)
                .align(Align::Center)
                .min_w(0)
                .grow(1)
                .basis(0);
                let size_text = match info.size {
                    Some(n) => bytes(n),
                    None if info.kind == EntryKind::Directory && fixture.is_none() => {
                        match sizes.get(FsPath::new(&info.path)) {
                            Some(size) => size.text(),
                            None => {
                                wanted.push(PathBuf::from(&info.path));
                                "…".into()
                            }
                        }
                    }
                    None => "—".into(),
                };
                let size = detail(size_text)
                    .when(active, |e| e.fill(selected_ink()))
                    .min_w(78)
                    .reserve("999.9 MB");
                let mut cells = vec![name];
                if wide_columns {
                    cells.push(
                        detail(info.kind_label())
                            .when(active, |e| e.fill(selected_ink()))
                            .min_w(94)
                            .reserve("Symbolic link"),
                    );
                }
                if modified_column {
                    cells.push(
                        detail(info.modified.map_or_else(|| "—".into(), date))
                            .when(active, |e| e.fill(selected_ink()))
                            .min_w(118)
                            .reserve("0000-00-00 00:00"),
                    );
                }
                cells.push(size);
                row(cells)
                    .gap(12)
                    .pad((12, 0))
                    .h(ROW)
                    .w(pct(100))
                    .align(Align::Center)
                    // uniform_list's stable item wrapper owns pointer/focus.
                    // A Hover style here would create a second unnamed hit target
                    // and swallow the wrapper's click, dropping keyboard focus.
                    .fill(if active {
                        selected()
                    } else if interaction.hovered {
                        color(0x293235)
                    } else {
                        ground()
                    })
                    .tip(info.name.clone())
                    .named(info.name)
                    .clip()
            },
        );
        for path in wanted {
            self.sizes.want(path);
        }
        self.context_requested |= context_request;
        self.rows_built = list.changed.rows_built;
        self.visible = list.changed.visible;
        if pending_selection.is_some() {
            self.selected = self
                .selection
                .cursor(&revision)
                .expect("current listing cursor");
        }
        if let Some(index) = pending_activation {
            self.activate(index);
            if let Some(tree) = self.build_archive(ui, jobs) {
                return jobs.with_extraction_status(ui, tree);
            }
        }
        // The collection's finite dimensions are visibility hints. Its actual box is
        // allocated by flex constraints, then read back for next-frame virtualization.
        let list = col([list.el.w(pct(100)).h(pct(100)).min_h(0).min_w(0)])
            .id(self.pid("file-list"))
            .focusable()
            .w(pct(100))
            .h(ROW)
            .grow(1)
            .basis(0)
            .shrink(1)
            .min_w(0)
            .min_h(0)
            .fill(ground());
        let state = self.browser.as_ref().map(|b| b.state().clone());
        let list = match &state {
            Some(LoadState::Loading) => col([
                label("Loading folder…"),
                detail("You can open another location while this folder loads."),
            ])
            .gap(7)
            .pad(18)
            .grow(1)
            .basis(0)
            .min_h(0)
            .fill(ground())
            .id(self.pid("file-list")),
            Some(LoadState::Failed(error)) => col([
                label("Could not open this folder").fill(color(0xffb4ab)),
                detail(error.clone()).wrap(),
                detail("Enter another location above, or go back.").wrap(),
            ])
            .gap(9)
            .pad(18)
            .grow(1)
            .basis(0)
            .min_h(0)
            .fill(ground())
            .id(self.pid("file-list")),
            _ if self.count() == 0 => col([
                label(if self.filter.is_empty() {
                    "This folder is empty"
                } else {
                    "No matching items"
                }),
                detail(if self.filter.is_empty() {
                    "Choose another folder from Places or enter a location."
                } else {
                    "Try a different filter, or clear it to show every item."
                })
                .wrap(),
            ])
            .gap(9)
            .pad(18)
            .grow(1)
            .basis(0)
            .min_h(0)
            .fill(ground())
            .id(self.pid("file-list")),
            _ => list,
        };
        let files = col(if self.grid {
            vec![list]
        } else {
            vec![header, hairline(), list]
        })
        .grow(1)
        .basis(0)
        .min_w(0)
        .min_h(0)
        .h(pct(100))
        .fill(ground())
        .clip()
        .id(self.pid("files-pane"));
        let mut panes = Vec::new();
        if show_rail {
            panes.push(self.rail(ui, &jobs.places));
            panes.push(divider("rail-divider", "Resize places"));
        }
        panes.push(files);
        if show_inspector {
            panes.push(divider("inspector-divider", "Resize inspector"));
            panes.push(self.inspector(ui));
        }
        let panes = row(panes)
            .grow(1)
            .basis(0)
            .min_h(0)
            .min_w(0)
            .w(pct(100))
            .clip();
        let filter = text_input(ui, self.pid("filter-input"), &mut self.filter);
        if real && filter.changed {
            if let Some(b) = self.browser.as_mut() {
                b.set_filter(&self.filter);
            }
            self.reset_listing();
        }
        let filter = filter
            .el
            .w(clamp(96., 28., 250.))
            .min_w(0)
            .fill(ground())
            .named("Filter this folder · Ctrl+F")
            .when(!real, |e| e.disabled())
            .tip(if real {
                "Filter current folder"
            } else {
                "Filtering is disabled for the synthetic rendering fixture"
            });
        let (folders, files) = match (&self.browser, self.fixture_count) {
            (Some(b), None) if b.entries().len() <= 500_000 => {
                let dirs = b
                    .entries()
                    .iter()
                    .filter(|e| e.kind == EntryKind::Directory)
                    .count();
                (dirs, b.entries().len() - dirs)
            }
            _ => (0, self.count()),
        };
        let chip = |icon: El, text: String, hot: bool| {
            row([icon, detail(text).when(hot, |e| e.fill(selected_ink()))])
                .gap(5)
                .pad((8, 3))
                .align(Align::Center)
                .radius(3)
                .fill(if hot { selected() } else { ground() })
                .shrink(0)
        };
        let status = row([
            glyph(Glyph::Search, muted()),
            filter,
            detail(match &state {
                Some(LoadState::Loading) => "Loading…",
                Some(LoadState::Failed(_)) => "Folder unavailable",
                _ => "",
            })
            .min_w(0)
            .clip(),
            spacer(),
            chip(glyph(Glyph::Folder, muted()), folders.to_string(), false),
            chip(glyph(Glyph::File, muted()), files.to_string(), false),
            chip(
                glyph(Glyph::Inspector, muted()),
                format!("{}/{}", self.selection.count(), self.count()),
                self.selection.count() > 0,
            ),
        ])
        .gap(7)
        .pad((8, 4))
        .align(Align::Center)
        .fill(chrome())
        .w(pct(100));
        let mut regions = if self.secondary() {
            vec![toolbar]
        } else {
            vec![tabs, hairline(), toolbar]
        };
        regions.extend([hairline(), panes, hairline(), status]);
        if self.secondary() {
            return col(regions)
                .w(pct(100))
                .h(pct(100))
                .min_w(0)
                .min_h(0)
                .fill(ground())
                .id(self.pid("browser-root"))
                .named("Second file browser");
        }
        if let Some(clipboard_status) = jobs.clipboard_status(ui) {
            regions.extend([hairline(), clipboard_status]);
        }
        if let Some(row) = jobs.fileops.build(ui) {
            regions.extend([hairline(), row]);
        }
        let tree = col(regions)
            .w(pct(100))
            .h(pct(100))
            .min_w(0)
            .min_h(0)
            .fill(ground())
            .id(self.pid("browser-root"))
            .named("File browser");
        jobs.with_transfer_statuses(ui, tree)
    }
    fn changed(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.geometry_changed);
        changed |= self.refresh();
        changed |= self.preview.poll();
        changed |= self.poll_watch();
        changed |= self.sizes.poll();
        changed |= self.grid && self.thumbs.poll();
        if let Some(find) = &mut self.find {
            changed |= find.poll();
        }
        if let Some(archive) = &mut self.archive {
            changed |= archive.poll();
        }
        changed
    }
    fn after_frame(&mut self, ui: &mut Ui) {
        self.evidence_focus_role = match ui.focus_key() {
            Some(id) if id == self.pid("path-input") => "location",
            Some(id) if id == self.pid("filter-input") => "filter",
            Some(id) if id.starts_with(&self.pid("file-list")) => "list",
            Some(_) => "other",
            None => "none",
        };
        if let Some(find) = &mut self.find {
            find.after_frame(ui);
            return;
        }
        if let Some(archive) = &mut self.archive {
            archive.after_frame(ui);
            return;
        }
        if let Some(scene) = ui.scene() {
            if let Some(frame) = scene.surface(&self.pid("browser-root")) {
                self.geometry_changed |= self.window != frame.frame.size;
                self.window = frame.frame.size;
            }
            if let Some(frame) = scene.surface(&self.pid("file-list")) {
                let next = frame.frame.size;
                if next.width > 0. && next.height > 0. {
                    if self.viewport != next {
                        // Preserve a visible active item across a constrained resize;
                        // do not jump back to a selection the user scrolled away from.
                        self.resize_reveal =
                            self.selected.filter(|index| self.visible.contains(index));
                        self.geometry_changed = true;
                    }
                    self.viewport = next;
                }
            }
        }
    }
}
fn divider(id: &str, name: &str) -> El {
    col([block(1, 0).h(pct(100)).fill(line())])
        .center()
        .w(GAP)
        .h(pct(100))
        .shrink(0)
        .fill(chrome())
        .id(id)
        .named(name)
        .cursor(Cursor::ResizeH)
        .on(State::Hover, |s| s.fill(color(0x45616b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(view: &mut BrowserView, ui: &mut Ui, size: Size) {
        for _ in 0..4 {
            let tree = view.build(ui, &Input::default());
            ui.frame(tree, Some(size), Input::default(), 0.0).unwrap();
            view.after_frame(ui);
        }
    }
    #[test]
    fn million_rows_remain_virtual_and_intrinsic_across_resizes() {
        let mut view = BrowserView::fixture(1_000_000);
        let mut ui = make_ui().unwrap();
        for size in [
            Size::new(640., 480.),
            Size::new(1024., 720.),
            Size::new(1600., 1000.),
            Size::new(320., 240.),
        ] {
            settle(&mut view, &mut ui, size);
            assert_eq!(view.tab().count(), 1_000_000);
            assert!(
                view.tab().rows_built <= (view.tab().viewport.height / ROW).ceil() as usize + 6,
                "{}",
                view.evidence()
            );
            assert!(view.tab().rows_built < 100, "{}", view.evidence());
            let scene = ui.scene().unwrap();
            let root = scene.surface("browser-root").unwrap().frame;
            let files = scene.surface("files-pane").unwrap().frame;
            let list = scene.surface("file-list").unwrap().frame;
            assert_eq!(root.size, size);
            assert!(files.x >= 0. && files.right() <= size.width + 0.1);
            assert!(list.size.height > 0. && list.bottom() < size.height);
            if let Some(inspector) = scene.surface("inspector") {
                assert!(inspector.frame.x >= files.right());
            }
        }
    }
    #[test]
    fn empty_fixture_has_a_distinct_usable_state() {
        let mut view = BrowserView::fixture(0);
        let mut ui = make_ui().unwrap();
        settle(&mut view, &mut ui, Size::new(640., 480.));
        assert_eq!(view.tab().rows_built, 0);
        assert!(
            ui.scene()
                .unwrap()
                .surface("file-list")
                .unwrap()
                .frame
                .size
                .height
                > ROW
        );
    }
}

#[cfg(test)]
mod interaction_and_font_tests {
    use super::*;
    #[test]
    fn escape_clears_selection_until_real_navigation() {
        let mut view = BrowserView::fixture(50);
        let mut ui = make_ui().unwrap();
        let size = Size::new(1024., 720.);
        let step = |view: &mut BrowserView, ui: &mut Ui, input: Input| {
            let tree = view.build(ui, &Input::default());
            ui.frame(tree, Some(size), input, 0.0).unwrap();
            view.after_frame(ui);
        };
        for _ in 0..4 {
            step(&mut view, &mut ui, Input::default());
        }
        assert_eq!(view.tab().selected, Some(8));
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
        assert_eq!(view.tab().selection.count(), 0);
        assert_eq!(view.tab().selected, Some(8));
        step(
            &mut view,
            &mut ui,
            Input {
                keys: vec![KeyPress {
                    key: Key::Down,
                    mods: Mods::default(),
                }],
                ..Input::default()
            },
        );
        for _ in 0..2 {
            step(&mut view, &mut ui, Input::default());
        }
        assert_eq!(view.tab().selected, Some(9));
    }
    #[test]
    fn pointer_selection_then_end_reaches_the_last_virtual_row() {
        let mut view = BrowserView::fixture(1_000_000);
        let mut ui = make_ui().unwrap();
        let size = Size::new(1024., 720.);
        let step = |view: &mut BrowserView, ui: &mut Ui, input: Input| {
            let tree = view.build(ui, &Input::default());
            ui.frame(tree, Some(size), input, 0.016).unwrap();
            view.after_frame(ui);
        };
        for _ in 0..4 {
            step(&mut view, &mut ui, Input::default());
        }
        let first = ui
            .scene()
            .unwrap()
            .surface(&format!("{}/item/0", view.tab().list_id()))
            .unwrap()
            .frame;
        let point = Point::new(
            first.x + first.size.width / 2.,
            first.y + first.size.height / 2.,
        );
        let pointer = |down| PointerInput {
            pos: Some(point),
            buttons: Buttons::default().set(Button::Primary, down),
            ..PointerInput::default()
        };
        // Native hosts deliver movement, press and release as separate frames.
        step(&mut view, &mut ui, Input::from(pointer(false)));
        step(&mut view, &mut ui, Input::from(pointer(true)));
        step(&mut view, &mut ui, Input::from(pointer(false)));
        step(&mut view, &mut ui, Input::from(pointer(false)));
        assert_eq!(
            view.tab().selected,
            Some(0),
            "pointer press must select the clicked row"
        );
        assert_eq!(
            ui.focus_key(),
            Some(format!("{}/item/0", view.tab().list_id()).as_str()),
            "pointer must focus the collection row rather than painted decoration"
        );
        step(
            &mut view,
            &mut ui,
            Input {
                pointer: pointer(false),
                keys: vec![KeyPress {
                    key: Key::End,
                    mods: Mods::default(),
                }],
                ..Input::default()
            },
        );
        step(&mut view, &mut ui, Input::from(pointer(false)));
        step(&mut view, &mut ui, Input::from(pointer(false)));
        assert_eq!(view.tab().selected, Some(999_999));
        assert_eq!(
            ui.focus_key(),
            Some(format!("{}/item/999999", view.tab().list_id()).as_str())
        );
        assert!(view.tab().visible.contains(&999_999), "{}", view.evidence());
        assert!(view.tab().rows_built < 40, "{}", view.evidence());
        for _ in 0..4 {
            let tree = view.build(&mut ui, &Input::default());
            ui.frame(tree, Some(Size::new(640., 480.)), Input::default(), 0.016)
                .unwrap();
            view.after_frame(&mut ui);
        }
        assert!(
            view.tab().visible.contains(&999_999),
            "resize hid the active row: {}",
            view.evidence()
        );
        assert!(view.tab().rows_built < 40, "{}", view.evidence());
    }

    #[test]
    fn available_named_fallbacks_shape_fixture_scripts_without_notdef() {
        for (group, candidates) in fallback_candidates().into_iter().enumerate() {
            let Some(fallback) = candidates
                .iter()
                .find_map(|path| read_fallback(path).ok().flatten())
            else {
                eprintln!("optional font group {group} absent; not qualified on this host");
                continue;
            };
            let sample = if group == 0 {
                "مرحبا"
            } else {
                "日本語のファイル"
            };
            let primary = Font::new(epaint_default_fonts::UBUNTU_LIGHT).unwrap();
            let mut ui = Ui::default().font(primary).fallback_font(fallback.font);
            ui.frame(
                label(sample).id("sample"),
                Some(Size::new(400., 80.)),
                Input::default(),
                0.0,
            )
            .unwrap();
            let scene = ui.scene().unwrap();
            let run = scene
                .paint
                .iter()
                .find(|p| p.key.as_str() == "sample" && p.text.is_some())
                .unwrap()
                .text
                .as_ref()
                .unwrap();
            assert!(!run.glyphs.is_empty());
            assert!(
                run.glyphs.iter().all(|g| g.id != 0),
                "missing glyph in {sample}"
            );
            assert!(
                run.glyphs.iter().any(|g| g.font > 0),
                "fallback was not used for {sample}"
            );
        }
    }
    #[test]
    fn fallback_loader_rejects_non_font_without_panicking() {
        let invalid = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(read_fallback(&invalid).is_err());
    }
}

#[cfg(test)]
mod multiselection_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const CTRL: Mods = Mods {
        ctrl: true,
        shift: false,
        alt: false,
        cmd: false,
    };
    const SHIFT: Mods = Mods {
        ctrl: false,
        shift: true,
        alt: false,
        cmd: false,
    };
    const BOTH: Mods = Mods {
        ctrl: true,
        shift: true,
        alt: false,
        cmd: false,
    };

    struct Harness {
        view: BrowserView,
        ui: Ui,
        size: Size,
    }
    impl Harness {
        fn fixture(count: usize) -> Self {
            let mut result = Self {
                view: BrowserView::fixture(count),
                ui: make_ui().unwrap(),
                size: Size::new(1024., 720.),
            };
            result.settle();
            result
        }
        fn step(&mut self, input: Input) {
            // Match native host ordering: build sees prior routed responses;
            // the new Input is not routed until ui.frame below.
            let tree = self.view.build(&mut self.ui, &input);
            self.ui.frame(tree, Some(self.size), input, 0.016).unwrap();
            self.view.after_frame(&mut self.ui);
        }
        fn settle(&mut self) {
            for _ in 0..4 {
                self.step(Input::default());
            }
        }
        fn key(&mut self, key: Key, mods: Mods) {
            self.step(Input {
                keys: vec![KeyPress { key, mods }],
                ..Input::default()
            });
            self.settle();
        }
        fn row_point(&self, index: usize) -> Point {
            let id = format!("{}/item/{index}", self.view.tab().list_id());
            let row = self.ui.scene().unwrap().surface(&id).unwrap().frame;
            Point::new(row.x + row.size.width / 2., row.y + row.size.height / 2.)
        }
        fn click_point(&mut self, point: Point, mods: Mods) {
            let input = |down, mods| {
                Input::from(PointerInput {
                    pos: Some(point),
                    buttons: Buttons::default().set(Button::Primary, down),
                    mods,
                })
            };
            self.step(input(false, Mods::default()));
            self.step(input(true, mods));
            // Releasing modifiers before pointer release must not reinterpret
            // the preceding response, nor toggle twice on release/click.
            self.step(input(false, Mods::default()));
            self.settle();
        }
        fn click(&mut self, index: usize, mods: Mods) {
            self.click_point(self.row_point(index), mods);
        }
        fn indices(&self) -> Vec<usize> {
            self.view
                .tab()
                .selection
                .ranges(self.view.tab().selection.revision())
                .unwrap()
                .iter()
                .cloned()
                .flatten()
                .collect()
        }
        fn cyan_rows(&self) -> usize {
            let list = self.ui.scene().unwrap().surface("file-list").unwrap().frame;
            self.ui
                .scene()
                .unwrap()
                .paint
                .iter()
                .filter(|paint| {
                    paint.paint == mui::scene::Paint::Solid(selected())
                        && paint.offset.y >= list.y
                        && paint.offset.y < list.bottom()
                })
                .count()
        }
    }

    #[test]
    fn pointer_modifiers_use_press_response_and_ranges_shrink_without_double_toggle() {
        let mut h = Harness::fixture(50);
        h.click(1, Mods::default());
        assert_eq!(h.indices(), [1]);
        h.click(5, CTRL);
        assert_eq!(h.indices(), [1, 5]);
        h.click(9, BOTH);
        assert_eq!(h.indices(), [1, 5, 6, 7, 8, 9]);
        h.click(3, BOTH);
        assert_eq!(h.indices(), [1, 3, 4, 5]);
        h.click(7, SHIFT);
        assert_eq!(h.indices(), [5, 6, 7]);
        h.key(Key::Up, SHIFT);
        assert_eq!(h.indices(), [5, 6]);
        h.key(Key::Up, SHIFT);
        assert_eq!(h.indices(), [5]);
        h.click(5, CTRL);
        assert!(h.indices().is_empty());
    }

    #[test]
    fn control_cursor_and_space_leave_other_rows_unselected_and_inspector_unchanged() {
        let mut h = Harness::fixture(50);
        h.click(1, Mods::default());
        assert!(h.ui.focus_key().unwrap().ends_with("/item/1"));
        let inspector = h.view.tab().show_inspector;
        h.key(Key::Down, CTRL);
        assert_eq!(h.view.tab().selected, Some(2));
        assert_eq!(h.indices(), [1]);
        assert!(h.ui.focus_key().unwrap().ends_with("/item/2"));
        assert_eq!(
            h.cyan_rows(),
            1,
            "cursor row must not become selected merely from focus"
        );
        h.key(Key::Space, CTRL);
        assert_eq!(h.indices(), [1, 2]);
        assert_eq!(h.view.tab().show_inspector, inspector);
        h.key(Key::Space, CTRL);
        assert_eq!(h.indices(), [1]);
        h.key(Key::Escape, Mods::default());
        assert!(h.indices().is_empty());
        assert_eq!(h.view.tab().selected, Some(2));
        assert_eq!(h.cyan_rows(), 0);
        assert_eq!(h.ui.focus_key(), Some("file-list"));
        h.key(Key::Down, Mods::default());
        assert_eq!(h.indices(), [3]);
        assert!(
            h.ui.focus_key().unwrap().ends_with("/item/3"),
            "stable wrapper focus must route navigation"
        );
    }

    #[test]
    fn million_row_select_all_is_lazy_and_keeps_manual_scroll_and_cursor() {
        let mut h = Harness::fixture(1_000_000);
        h.click(1, Mods::default());
        h.key(Key::End, Mods::default());
        assert_eq!(h.view.tab().selected, Some(999_999));
        // Move the viewport independently of the cursor before selecting all.
        h.view
            .tab_mut()
            .list
            .scroll_to_item(500_000, ScrollTo::Start);
        h.settle();
        let scroll = h.view.tab().list.scroll_offset();
        let visible = h.view.tab().visible.clone();
        h.key(Key::Char('a'), CTRL);
        assert_eq!(h.view.tab().selection.count(), 1_000_000);
        assert_eq!(
            h.view
                .tab()
                .selection
                .ranges(h.view.tab().selection.revision())
                .unwrap(),
            std::slice::from_ref(&(0..1_000_000))
        );
        assert_eq!(h.view.tab().list.scroll_offset(), scroll);
        assert_eq!(h.view.tab().visible, visible);
        assert_eq!(h.view.tab().selected, Some(999_999));
        assert!(h.view.tab().rows_built < 40, "{}", h.view.evidence());
        assert!(h.view.evidence().contains("\"selected_count\":1000000"));
    }

    #[test]
    fn toolbar_shows_clickable_breadcrumbs_inside_the_window_until_the_path_is_edited() {
        let fixture = Fixture::new();
        let mut h = Harness {
            view: BrowserView::new(fixture.0.clone()),
            ui: make_ui().unwrap(),
            size: Size::new(1024., 720.),
        };
        complete(&mut h);
        let scene = h.ui.scene().unwrap();
        let root = scene.surface("browser-root").unwrap().frame;
        let crumbs = h.view.tab().crumbs();
        assert!(crumbs.len() >= 2);
        for (i, _, _) in crumbs {
            let f = scene.surface(&format!("crumb/{i}")).unwrap().frame;
            assert!(
                f.x >= root.x && f.right() <= root.right(),
                "crumb {i} overflows"
            );
        }
        assert!(
            scene.surface("path-input").is_none(),
            "editor only while editing"
        );
    }

    #[test]
    fn text_field_control_a_edits_text_without_selecting_files() {
        let fixture = Fixture::new();
        for name in ["alpha", "beta"] {
            std::fs::write(fixture.0.join(name), b"payload").unwrap();
        }
        let mut h = Harness {
            view: BrowserView::new(fixture.0.clone()),
            ui: make_ui().unwrap(),
            size: Size::new(1024., 720.),
        };
        complete(&mut h);
        h.click(0, Mods::default());
        let field = h.ui.scene().unwrap().surface("path-click").unwrap().frame;
        let point = Point::new(
            field.x + field.size.width / 2.,
            field.y + field.size.height / 2.,
        );
        h.click_point(point, Mods::default());
        h.settle();
        assert!(
            h.ui.focus_is_text(),
            "pointer must genuinely enter the editor"
        );
        h.key(Key::Char('a'), CTRL);
        assert_eq!(h.indices(), [0]);
        h.step(Input {
            text: "replacement".into(),
            ..Input::default()
        });
        h.settle();
        assert_eq!(h.view.tab().path_text, "replacement");
        assert_eq!(h.indices(), [0]);
    }

    #[test]
    fn accepted_listing_does_not_take_focus_from_location_editor() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("alpha"), b"payload").unwrap();
        let mut h = Harness {
            view: BrowserView::new(fixture.0.clone()),
            ui: make_ui().unwrap(),
            size: Size::new(1024., 720.),
        };
        complete(&mut h);
        let field = h.ui.scene().unwrap().surface("path-click").unwrap().frame;
        h.click_point(
            Point::new(
                field.x + field.size.width / 2.,
                field.y + field.size.height / 2.,
            ),
            Mods::default(),
        );
        h.settle();
        assert!(h.ui.focus_is_text());
        let focus = h.ui.focus_key().unwrap().to_owned();
        // Complete a real worker refresh while the editor retains input focus.
        h.view.tab_mut().navigate(fixture.0.clone());
        complete(&mut h);
        assert_eq!(h.ui.focus_key(), Some(focus.as_str()));
        assert!(h.ui.focus_is_text());
        h.key(Key::Char('a'), CTRL);
        h.step(Input {
            text: "still editing".into(),
            ..Input::default()
        });
        h.settle();
        assert_eq!(h.view.tab().path_text, "still editing");
        assert!(h.indices().is_empty());
    }

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/find-generated")
                .join(format!(
                    "selection-{}-{}",
                    std::process::id(),
                    NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn complete(h: &mut Harness) {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            h.step(Input::default());
            if !matches!(
                h.view.tab().browser.as_ref().unwrap().state(),
                LoadState::Loading
            ) {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        h.settle();
    }

    #[test]
    fn same_count_refresh_mints_new_hit_identity_and_rejects_stale_row_press() {
        let fixture = Fixture::new();
        for name in ["alpha", "beta"] {
            std::fs::write(fixture.0.join(name), b"data").unwrap();
        }
        let mut h = Harness {
            view: BrowserView::new(fixture.0.clone()),
            ui: make_ui().unwrap(),
            size: Size::new(1024., 720.),
        };
        complete(&mut h);
        h.click(0, Mods::default());
        let revision = h.view.tab().selection.revision().clone();
        let old_id = h.view.tab().list_id();
        let point = h.row_point(1);
        h.step(Input::from(PointerInput {
            pos: Some(point),
            ..PointerInput::default()
        }));
        h.step(Input::from(PointerInput {
            pos: Some(point),
            buttons: Buttons::default().set(Button::Primary, true),
            ..PointerInput::default()
        }));
        std::fs::remove_file(fixture.0.join("alpha")).unwrap();
        std::fs::write(fixture.0.join("gamma"), b"new").unwrap();
        h.view.tab_mut().navigate(fixture.0.clone());
        complete(&mut h);
        assert_eq!(h.view.tab().count(), 2);
        assert_ne!(h.view.tab().list_id(), old_id);
        assert_ne!(*h.view.tab().selection.revision(), revision);
        assert_eq!(
            h.view.tab().selection.ranges(&revision),
            Err(browser_core::selection::SelectionError::StaleRevision)
        );
        assert!(h.indices().is_empty());
        assert_eq!(h.view.tab().selected, None);
        h.step(Input::from(PointerInput {
            pos: Some(point),
            ..PointerInput::default()
        }));
        h.settle();
        assert!(
            h.indices().is_empty(),
            "old pointer capture must not select a replacement row"
        );
        h.click(0, Mods::default());
        assert_eq!(h.indices(), [0]);
        assert_eq!(
            h.view.tab().browser.as_ref().unwrap().entries()[0].path,
            fixture.0.join("beta")
        );
    }

    #[test]
    fn find_reveal_path_selects_matching_native_entry_after_listing_delivery() {
        let fixture = Fixture::new();
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"native-\xff".to_vec())
        };
        #[cfg(not(unix))]
        let name = std::ffi::OsString::from("native-name");
        let native = fixture.0.join(name);
        std::fs::write(&native, b"payload").unwrap();
        std::fs::write(fixture.0.join("another"), b"payload").unwrap();
        let mut h = Harness {
            view: BrowserView::new(fixture.0.clone()),
            ui: make_ui().unwrap(),
            size: Size::new(1024., 720.),
        };
        // This is the same navigation/reveal handshake as FindAction::Open.
        h.view.tab_mut().navigate(fixture.0.clone());
        h.view.tab_mut().reveal_path = Some(native.clone());
        complete(&mut h);
        let index = h.view.tab().selected.unwrap();
        assert_eq!(h.indices(), [index]);
        assert_eq!(
            h.view.tab().browser.as_ref().unwrap().entries()[index].path,
            native
        );
        assert!(
            h.ui.focus_key()
                .unwrap()
                .ends_with(&format!("/item/{index}"))
        );
        h.key(Key::Down, CTRL);
        assert_eq!(h.view.tab().selection.count(), 1);
    }

    #[test]
    fn selected_mask_hands_off_exact_worker_listing_and_survives_same_count_refresh() {
        use browser_core::listing::{MaterializeLimits, TicketError, TicketLimits};
        let fixture = Fixture::new();
        let a = fixture.0.join("alpha");
        let b = fixture.0.join("beta");
        std::fs::write(&a, b"alpha").unwrap();
        std::fs::write(&b, b"beta").unwrap();
        let mut h = Harness {
            view: BrowserView::new(fixture.0.clone()),
            ui: make_ui().unwrap(),
            size: Size::new(1024., 720.),
        };
        complete(&mut h);
        h.key(Key::Char('a'), CTRL);
        let snapshot = h.view.tab().browser.as_ref().unwrap().snapshot().unwrap();
        assert_eq!(snapshot.revision(), h.view.tab().selection.revision());
        let ticket = snapshot
            .ticket(&h.view.tab().selection, TicketLimits::default())
            .unwrap();
        let original_revision = ticket.revision().clone();
        std::fs::remove_file(&a).unwrap();
        std::fs::write(fixture.0.join("gamma"), b"replacement").unwrap();
        h.view.tab_mut().navigate(fixture.0.clone());
        complete(&mut h);
        assert_ne!(h.view.tab().selection.revision(), &original_revision);
        assert!(matches!(
            ticket.materialize(
                h.view.tab().selection.revision(),
                MaterializeLimits::default()
            ),
            Err(TicketError::StaleRevision)
        ));
        // An already-authorized action retains its old native identities. It
        // cannot silently change from alpha/beta to the refreshed beta/gamma.
        let mut materialization = ticket
            .materialize(&original_revision, MaterializeLimits::default())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let paths = loop {
            if let Some(result) = materialization.poll() {
                break result.unwrap();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(paths.paths(), &[a, b]);
        assert_eq!(paths.revision(), &original_revision);
        assert_eq!(h.view.tab().selection.count(), 0);
    }
}
