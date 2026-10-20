//! App-owned create / rename / trash / undo. Filesystem calls run on a short-lived worker;
//! the UI only holds the prompt text, a bounded undo stack and a status line.
use super::*;
use browser_core::listing::{Materialization, MaterializeLimits, TicketLimits};
use browser_operations::fsops::{self, Undo};
use std::{ffi::OsString, sync::mpsc};

const UNDO_DEPTH: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PromptKind {
    Rename(PathBuf),
    NewFolder(PathBuf),
    NewFile(PathBuf),
}
pub(super) struct Prompt {
    kind: PromptKind,
    text: String,
    focus: bool,
}
struct Done {
    undo: Vec<Undo>,
    refresh: Vec<PathBuf>,
    failure: Option<String>,
    verb: &'static str,
}
#[derive(Default)]
pub(super) struct FileOps {
    prompt: Option<Prompt>,
    selecting: Option<Materialization>,
    cutting: Option<Materialization>,
    cut: Option<Vec<PathBuf>>,
    running: Option<mpsc::Receiver<Done>>,
    undo: Vec<Vec<Undo>>,
    message: Option<(String, bool)>,
    refresh: Vec<PathBuf>,
}

fn parent_of(path: &FsPath) -> Option<PathBuf> {
    path.parent().map(FsPath::to_path_buf)
}

impl FileOps {
    pub(super) fn busy(&self) -> bool {
        self.selecting.is_some() || self.cutting.is_some() || self.running.is_some()
    }
    pub(super) fn open_prompt(&mut self, kind: PromptKind, text: String) {
        self.message = None;
        self.prompt = Some(Prompt {
            kind,
            text,
            focus: true,
        });
    }
    pub(super) fn cancel_prompt(&mut self) {
        self.prompt = None;
    }
    pub(super) fn take_refresh(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.refresh)
    }

    fn spawn(&mut self, verb: &'static str, work: impl FnOnce() -> Done + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.running = Some(rx);
        self.message = Some((format!("{verb}…"), false));
    }
    /// Commit the prompt text. Returns false when nothing was started.
    pub(super) fn commit_prompt(&mut self) -> bool {
        let Some(prompt) = self.prompt.take() else {
            return false;
        };
        if self.busy() {
            self.prompt = Some(prompt);
            return false;
        }
        let name = OsString::from(prompt.text.trim_end_matches('\n'));
        if let Err(error) = fsops::validate_name(&name) {
            self.message = Some((format!("Invalid name: {}", error.message), true));
            self.prompt = Some(Prompt {
                focus: true,
                ..prompt
            });
            return false;
        }
        let (label, verb) = match prompt.kind {
            PromptKind::Rename(_) => ("Renaming", "Renamed"),
            _ => ("Creating", "Created"),
        };
        self.spawn(label, move || {
            let (result, dir) = match prompt.kind {
                PromptKind::Rename(path) => (
                    fsops::rename(&path, &name),
                    parent_of(&path).unwrap_or_default(),
                ),
                PromptKind::NewFolder(dir) => (fsops::create_dir(&dir, &name), dir),
                PromptKind::NewFile(dir) => (fsops::create_file(&dir, &name), dir),
            };
            match result {
                Ok(undo) => Done {
                    undo: vec![undo],
                    refresh: vec![dir],
                    failure: None,
                    verb,
                },
                Err(e) => Done {
                    undo: vec![],
                    refresh: vec![],
                    failure: Some(e.to_string()),
                    verb,
                },
            }
        });
        true
    }
    fn materialize(
        &mut self,
        snapshot: Option<browser_core::listing::ListingSnapshot>,
        selection: &Selection,
        what: &str,
    ) -> Option<Materialization> {
        if selection.count() == 0 {
            self.message = Some((format!("Select files or folders to {what}."), true));
            return None;
        }
        let Some(snapshot) = snapshot else {
            self.message = Some(("Wait for the folder to finish loading.".into(), true));
            return None;
        };
        match snapshot
            .ticket(selection, TicketLimits::default())
            .and_then(|t| t.materialize(snapshot.revision(), MaterializeLimits::default()))
        {
            Ok(work) => {
                self.message = Some(("Preparing selection…".into(), false));
                Some(work)
            }
            Err(error) => {
                self.message = Some((format!("Cannot {what}: {error:?}"), true));
                None
            }
        }
    }
    /// Begin trashing the selection: materialize paths off-thread, then trash each.
    pub(super) fn trash_selection(
        &mut self,
        snapshot: Option<browser_core::listing::ListingSnapshot>,
        selection: &Selection,
    ) {
        if !self.busy() {
            self.selecting = self.materialize(snapshot, selection, "move to Trash");
        }
    }
    /// Remember the selection as cut; the next matching paste moves instead of copying.
    pub(super) fn cut_selection(
        &mut self,
        snapshot: Option<browser_core::listing::ListingSnapshot>,
        selection: &Selection,
    ) {
        if !self.busy() {
            self.cut = None;
            self.cutting = self.materialize(snapshot, selection, "cut");
        }
    }
    /// True (and the cut is consumed) when `paths` is exactly the remembered cut set.
    pub(super) fn take_cut_matching(&mut self, paths: &[PathBuf]) -> bool {
        let mut a: Vec<_> = paths.iter().collect();
        a.sort();
        let matches = self.cut.as_ref().is_some_and(|cut| {
            let mut b: Vec<_> = cut.iter().collect();
            b.sort();
            a == b
        });
        if matches {
            self.cut = None;
        }
        matches
    }
    /// Move paths into `dest` on a worker. Same-filesystem only; never replaces.
    pub(super) fn start_move(&mut self, paths: Vec<PathBuf>, dest: PathBuf) -> bool {
        if self.busy() {
            return false;
        }
        self.spawn("Moved", move || {
            let mut done = Done {
                undo: Vec::new(),
                refresh: vec![dest.clone()],
                failure: None,
                verb: "Moved",
            };
            for path in &paths {
                match fsops::move_into(path, &dest) {
                    Ok(op) => {
                        done.refresh.extend(parent_of(path));
                        done.undo.push(op);
                    }
                    Err(e) => {
                        let cross = e.raw_os_error == Some(18);
                        done.failure = Some(if cross {
                            "different filesystem; copy then delete instead".into()
                        } else {
                            e.to_string()
                        });
                        break;
                    }
                }
            }
            done
        });
        true
    }
    pub(super) fn undo_last(&mut self) {
        if self.busy() {
            return;
        }
        let Some(batch) = self.undo.pop() else {
            self.message = Some(("Nothing to undo.".into(), false));
            return;
        };
        self.spawn("Undoing", move || {
            let mut failure = None;
            let mut refresh = Vec::new();
            for op in batch.iter().rev() {
                match fsops::undo(op) {
                    Ok(()) => refresh.extend(undo_dirs(op)),
                    Err(e) => failure = Some(e.to_string()),
                }
            }
            Done {
                undo: vec![],
                refresh,
                failure,
                verb: "Undo",
            }
        });
    }

    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(work) = &mut self.selecting
            && let Some(result) = work.poll()
        {
            self.selecting = None;
            changed = true;
            match result {
                Ok(paths) => {
                    let paths = paths.into_paths();
                    self.spawn("Moving to Trash", move || {
                        let mut undo = Vec::new();
                        let mut refresh = Vec::new();
                        let mut failure = None;
                        for path in &paths {
                            match fsops::trash(path) {
                                Ok(op) => {
                                    refresh.extend(parent_of(path));
                                    undo.push(op);
                                }
                                Err(e) => {
                                    failure = Some(e.to_string());
                                    break;
                                }
                            }
                        }
                        Done {
                            undo,
                            refresh,
                            failure,
                            verb: "Moved to Trash",
                        }
                    });
                }
                Err(error) => {
                    self.message = Some((format!("Cannot trash selection: {error:?}"), true));
                }
            }
        }
        if let Some(work) = &mut self.cutting
            && let Some(result) = work.poll()
        {
            self.cutting = None;
            changed = true;
            match result {
                Ok(paths) => {
                    let paths = paths.into_paths();
                    self.message =
                        Some((format!("Cut {} items · paste to move", paths.len()), false));
                    self.cut = Some(paths);
                }
                Err(error) => self.message = Some((format!("Cannot cut: {error:?}"), true)),
            }
        }
        if let Some(rx) = &self.running
            && let Ok(done) = rx.try_recv()
        {
            self.running = None;
            changed = true;
            let count = done.undo.len();
            if !done.undo.is_empty() {
                self.undo.push(done.undo);
                if self.undo.len() > UNDO_DEPTH {
                    self.undo.remove(0);
                }
            }
            self.refresh.extend(done.refresh);
            self.message = Some(match done.failure {
                Some(f) => (format!("{} failed: {f}", done.verb), true),
                None if count > 1 => (
                    format!("{} · {count} items · Ctrl+Z to undo", done.verb),
                    false,
                ),
                None => (format!("{} · Ctrl+Z to undo", done.verb), false),
            });
        }
        changed
    }

    pub(super) fn build(&mut self, ui: &mut Ui) -> Option<El> {
        let mut commit = false;
        let mut cancel = false;
        let mut el = None;
        if let Some(prompt) = &mut self.prompt {
            if std::mem::take(&mut prompt.focus) {
                ui.focus("name-prompt");
            }
            let title = match prompt.kind {
                PromptKind::Rename(_) => "Rename to",
                PromptKind::NewFolder(_) => "New folder",
                PromptKind::NewFile(_) => "New file",
            };
            let field = text_edit(ui, "name-prompt", &mut prompt.text, TextOpts::default());
            commit = field.changed.submitted;
            let ok = action(
                ui,
                "name-prompt-ok",
                "Confirm · Enter",
                label("OK"),
                true,
                false,
            );
            let no = action(
                ui,
                "name-prompt-cancel",
                "Cancel · Esc",
                label("Cancel"),
                true,
                false,
            );
            cancel = clicked(ui, "name-prompt-cancel");
            commit |= clicked(ui, "name-prompt-ok");
            el = Some(
                row([
                    detail(title),
                    field
                        .el
                        .grow(1)
                        .basis(0)
                        .min_w(0)
                        .fill(ground())
                        .named(title),
                    ok,
                    no,
                ])
                .gap(7)
                .pad((8, 3))
                .align(Align::Center)
                .fill(chrome())
                .shrink(0)
                .min_w(0)
                .id("name-prompt-row"),
            );
        } else if let Some((text, failed)) = &self.message {
            el = Some(
                row([detail(text.clone())
                    .min_w(0)
                    .lines(1)
                    .clip()
                    .when(*failed, |e| e.fill(color(0xffb4ab)))])
                .pad((8, 3))
                .fill(chrome())
                .shrink(0)
                .min_w(0)
                .id("fileops-status"),
            );
        }
        if cancel {
            self.cancel_prompt();
        } else if commit {
            self.commit_prompt();
        }
        el
    }
}

fn undo_dirs(op: &Undo) -> Vec<PathBuf> {
    match op {
        Undo::Remove(p) => parent_of(p).into_iter().collect(),
        Undo::Rename { from, to } => parent_of(from).into_iter().chain(parent_of(to)).collect(),
        Undo::Untrash(t) => parent_of(&t.original).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settle(ops: &mut FileOps) {
        for _ in 0..500 {
            ops.poll();
            if !ops.busy() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("file operation did not finish");
    }
    #[test]
    fn create_rename_undo_and_invalid_names_refresh_the_parent() {
        let dir = std::env::temp_dir().join(format!("fileops-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ops = FileOps::default();
        ops.open_prompt(PromptKind::NewFolder(dir.clone()), "a/b".into());
        assert!(
            !ops.commit_prompt(),
            "separators are rejected and the prompt stays open"
        );
        assert!(ops.prompt.is_some());
        ops.prompt.as_mut().unwrap().text = "a".into();
        assert!(ops.commit_prompt());
        settle(&mut ops);
        assert!(dir.join("a").is_dir());
        assert_eq!(ops.take_refresh(), vec![dir.clone()]);
        ops.open_prompt(PromptKind::Rename(dir.join("a")), "b".into());
        ops.commit_prompt();
        settle(&mut ops);
        assert!(dir.join("b").is_dir() && !dir.join("a").exists());
        ops.undo_last();
        settle(&mut ops);
        assert!(dir.join("a").is_dir());
        ops.undo_last();
        settle(&mut ops);
        assert!(!dir.join("a").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn matching_cut_moves_without_replacing_and_undo_returns_the_files() {
        let dir = std::env::temp_dir().join(format!("fileops-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dest")).unwrap();
        std::fs::write(dir.join("a"), b"1").unwrap();
        std::fs::write(dir.join("b"), b"2").unwrap();
        std::fs::write(dir.join("dest/b"), b"keep").unwrap();
        let mut ops = FileOps {
            cut: Some(vec![dir.join("a"), dir.join("b")]),
            ..FileOps::default()
        };
        assert!(
            !ops.take_cut_matching(&[dir.join("a")]),
            "partial match is a copy"
        );
        assert!(ops.take_cut_matching(&[dir.join("b"), dir.join("a")]));
        assert!(!ops.take_cut_matching(&[dir.join("a")]), "cut is consumed");
        assert!(ops.start_move(vec![dir.join("a"), dir.join("b")], dir.join("dest")));
        settle(&mut ops);
        assert!(dir.join("dest/a").exists() && !dir.join("a").exists());
        assert_eq!(std::fs::read(dir.join("dest/b")).unwrap(), b"keep");
        assert!(dir.join("b").exists(), "conflicting source stays put");
        ops.undo_last();
        settle(&mut ops);
        assert!(dir.join("a").exists() && !dir.join("dest/a").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
