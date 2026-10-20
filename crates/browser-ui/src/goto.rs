//! GoTo: type to jump to any indexed file or folder on the machine, or type a path to browse it.
//! The catalog is the persistent, memory-mapped qfind snapshot; it is built in the background and
//! never blocks the UI. Queries run on short-lived workers and only the newest answer is shown.
use super::*;
use qfind_core::{Catalog, Config, SearchOpts, Sort, default_snapshot_path};
use std::sync::{Arc, Mutex, mpsc};

const SHOWN: usize = 12;
const LIMIT: usize = 200;

/// The shared catalog: opened (or rebuilt) once, in the background.
#[derive(Clone, Default)]
pub(super) struct CatalogHandle(Arc<Mutex<CatalogState>>);
#[derive(Default)]
enum CatalogState {
    #[default]
    Idle,
    Building,
    Ready(Arc<Catalog>),
    Failed(String),
}
impl CatalogHandle {
    fn start(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*state, CatalogState::Idle) {
            return;
        }
        *state = CatalogState::Building;
        let handle = self.0.clone();
        std::thread::spawn(move || {
            let snapshot = default_snapshot_path();
            let result = Catalog::open(&snapshot)
                .or_else(|_| Catalog::rebuild(Config::load().rebuild_to(&snapshot)));
            *handle.lock().unwrap_or_else(|e| e.into_inner()) = match result {
                Ok(catalog) => {
                    catalog.warm();
                    CatalogState::Ready(Arc::new(catalog))
                }
                Err(error) => CatalogState::Failed(error.to_string()),
            };
        });
    }
    fn ready(&self) -> Option<Arc<Catalog>> {
        match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            CatalogState::Ready(c) => Some(c.clone()),
            _ => None,
        }
    }
    fn status(&self) -> String {
        match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            CatalogState::Idle | CatalogState::Building => "Indexing this machine…".into(),
            CatalogState::Ready(c) => format!("{} indexed items", c.len()),
            CatalogState::Failed(e) => format!("Index unavailable: {e}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Item {
    name: String,
    parent: String,
    path: PathBuf,
    dir: bool,
}
pub(super) enum Outcome {
    Open,
    Close,
    /// Navigate here; for a file, reveal it in its folder. `new_tab` opens a background tab.
    Go {
        path: PathBuf,
        dir: bool,
        new_tab: bool,
    },
}
pub(super) struct Goto {
    catalog: CatalogHandle,
    query: String,
    selected: usize,
    focus: bool,
    id: u64,
    items: Vec<Item>,
    tx: mpsc::Sender<(u64, Vec<Item>)>,
    rx: mpsc::Receiver<(u64, Vec<Item>)>,
    asked: Option<(u64, String)>,
    was_ready: bool,
}

/// Children of the folder named by `typed` that match its last component (path mode).
fn path_items(typed: &str) -> Vec<Item> {
    let expanded = match typed.strip_prefix('~') {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| format!("{}{rest}", h.to_string_lossy()))
            .unwrap_or_else(|| typed.to_owned()),
        None => typed.to_owned(),
    };
    let (dir, stem) = match expanded.rfind('/') {
        Some(i) => (&expanded[..=i], expanded[i + 1..].to_lowercase()),
        None => return Vec::new(),
    };
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Item> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.to_lowercase().starts_with(&stem).then(|| Item {
                dir: e.file_type().is_ok_and(|t| t.is_dir()),
                parent: dir.to_owned(),
                path: e.path(),
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out.truncate(LIMIT);
    out
}
fn catalog_items(catalog: &Catalog, query: &str) -> Vec<Item> {
    let opts = SearchOpts {
        limit: LIMIT,
        sort: Sort::Score,
        show_hidden: false,
        ..SearchOpts::default()
    };
    let Ok(hits) = catalog.search_with(query, opts) else {
        return Vec::new();
    };
    hits.iter()
        .map(|h| {
            let path = h.path();
            Item {
                name: h.name().into_owned(),
                parent: path
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                dir: h.is_dir(),
                path,
            }
        })
        .collect()
}

impl Goto {
    pub(super) fn open(catalog: CatalogHandle) -> Self {
        catalog.start();
        let (tx, rx) = mpsc::channel();
        Self {
            catalog,
            query: String::new(),
            selected: 0,
            focus: true,
            id: 0,
            items: Vec::new(),
            tx,
            rx,
            asked: None,
            was_ready: false,
        }
    }
    fn ask(&mut self) {
        let wanted = self.query.trim().to_owned();
        if self.asked.as_ref().is_some_and(|(_, q)| *q == wanted) {
            return;
        }
        self.id += 1;
        self.asked = Some((self.id, wanted.clone()));
        if wanted.is_empty() {
            self.items.clear();
            return;
        }
        let (id, tx) = (self.id, self.tx.clone());
        let catalog = self.catalog.ready();
        std::thread::spawn(move || {
            let items = if wanted.starts_with('/') || wanted.starts_with('~') {
                path_items(&wanted)
            } else if let Some(catalog) = catalog {
                catalog_items(&catalog, &wanted)
            } else {
                Vec::new()
            };
            let _ = tx.send((id, items));
        });
    }
    /// Newest answers only; true when the list changed.
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok((id, items)) = self.rx.try_recv() {
            if self.asked.as_ref().is_some_and(|(want, _)| *want == id) {
                self.items = items;
                self.selected = 0;
                changed = true;
            }
        }
        // The catalog may finish after the first keystrokes: ask once more when it is ready.
        if !self.was_ready && self.catalog.ready().is_some() {
            self.was_ready = true;
            if !self.query.trim().is_empty() && !self.query.starts_with(['/', '~']) {
                self.asked = None;
                self.ask();
                changed = true;
            }
        }
        changed
    }
    pub(super) fn build(&mut self, ui: &mut Ui) -> (El, Outcome) {
        if std::mem::take(&mut self.focus) {
            ui.focus("goto-input");
        }
        let mut outcome = Outcome::Open;
        let mut moved = 0isize;
        let mut expand = false;
        for press in ui.keys("goto-input").to_vec() {
            match press.key {
                Key::Escape => outcome = Outcome::Close,
                Key::Down => moved += 1,
                Key::Up => moved -= 1,
                Key::Tab => expand = true,
                _ => {}
            }
        }
        let field = text_edit(ui, "goto-input", &mut self.query, TextOpts::default());
        if field.changed.changed {
            self.ask();
        }
        let shown = self.items.len().min(SHOWN);
        if shown > 0 {
            self.selected = (self.selected as isize + moved).rem_euclid(shown as isize) as usize;
        }
        let chosen = self.items.get(self.selected).cloned();
        if expand && let Some(item) = &chosen {
            self.query = format!(
                "{}{}",
                item.path.to_string_lossy(),
                if item.dir { "/" } else { "" }
            );
            self.asked = None;
            self.ask();
            self.focus = true;
        }
        let ctrl = ui
            .keys("goto-input")
            .iter()
            .any(|p| p.key == Key::Enter && (p.mods.ctrl || p.mods.cmd));
        if field.changed.submitted
            && let Some(item) = chosen
        {
            outcome = Outcome::Go {
                path: item.path,
                dir: item.dir,
                new_tab: ctrl,
            };
        }
        let mut rows = vec![
            row([
                glyph(Glyph::Search, muted()),
                field.el.grow(1).basis(0).min_w(0).fill(ground()),
            ])
            .gap(8)
            .pad((10, 6))
            .align(Align::Center),
        ];
        for (i, item) in self.items.iter().take(SHOWN).enumerate() {
            let id = format!("goto-{i}");
            if clicked(ui, &id) {
                outcome = Outcome::Go {
                    path: item.path.clone(),
                    dir: item.dir,
                    new_tab: false,
                };
            }
            let active = i == self.selected;
            rows.push(
                action(
                    ui,
                    &id,
                    &item.path.to_string_lossy(),
                    row([
                        entry_glyph(item.dir, active, muted()),
                        label(item.name.clone())
                            .when(active, |e| e.fill(selected_ink()))
                            .lines(1),
                        detail(format!("› {}", item.parent))
                            .when(active, |e| e.fill(selected_ink()))
                            .lines(1)
                            .min_w(0)
                            .grow(1)
                            .basis(0)
                            .clip(),
                    ])
                    .gap(8)
                    .align(Align::Center)
                    .pad((10, 5))
                    .w(pct(100)),
                    true,
                    active,
                )
                .w(pct(100))
                .pad(0),
            );
        }
        if self.items.is_empty() {
            rows.push(
                detail(if self.query.trim().is_empty() {
                    self.catalog.status()
                } else {
                    "No matches".into()
                })
                .pad((12, 6)),
            );
        }
        rows.push(
            detail("Enter go · Ctrl+Enter new tab · Tab expand · Esc close")
                .pad((12, 5))
                .min_w(0)
                .clip(),
        );
        (
            col(rows)
                .w(pct(100))
                .fill(chrome())
                .shrink(0)
                .id("goto")
                .named("Go to"),
            outcome,
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use qfind_core::Rebuild;
    use std::time::{Duration, Instant};

    fn settle(goto: &mut Goto) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            goto.poll();
            if goto.asked.as_ref().is_some_and(|(id, _)| *id == goto.id) && !goto.items.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn finds_indexed_names_anywhere_and_lists_typed_paths() {
        let dir = std::env::temp_dir().join(format!("goto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        std::fs::write(dir.join("deep/er/alpha_report.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("beta")).unwrap();
        let catalog =
            Catalog::rebuild(Rebuild::new(dir.join("snap")).roots([dir.clone()])).unwrap();
        let handle = CatalogHandle::default();
        *handle.0.lock().unwrap() = CatalogState::Ready(Arc::new(catalog));
        let mut goto = Goto::open(handle);
        goto.query = "alpha".into();
        goto.ask();
        settle(&mut goto);
        let hit = goto.items.first().expect("indexed hit");
        assert_eq!(hit.name, "alpha_report.txt");
        assert!(!hit.dir && hit.parent.ends_with("deep/er"));
        goto.query = format!("{}/be", dir.display());
        goto.items.clear();
        goto.ask();
        settle(&mut goto);
        assert_eq!(goto.items[0].name, "beta");
        assert!(goto.items[0].dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
