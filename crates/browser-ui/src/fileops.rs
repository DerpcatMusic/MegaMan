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
    /// Template rename of several items: `{name}` stem, `{ext}` with dot, `{n}` or `{n:3}` counter.
    Batch(Vec<PathBuf>),
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
    /// Items moved by copy + trash (no inverse is recorded for these).
    copied: usize,
}
#[derive(Default)]
pub(super) struct FileOps {
    prompt: Option<Prompt>,
    selecting: Option<Materialization>,
    cutting: Option<Materialization>,
    renaming: Option<Materialization>,
    zipping: Option<Materialization>,
    cut: Option<Vec<PathBuf>>,
    running: Option<mpsc::Receiver<Done>>,
    undo: Vec<Vec<Undo>>,
    message: Option<(String, bool)>,
    refresh: Vec<PathBuf>,
}

/// Expand a batch template for the `n`th (1-based) path.
fn expand(template: &str, path: &FsPath, n: usize) -> OsString {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let token = &rest[open + 1..open + close];
        match token {
            "name" => out.push_str(&stem),
            "ext" => out.push_str(&ext),
            "n" => out.push_str(&n.to_string()),
            t if t.starts_with("n:") && t[2..].parse::<usize>().is_ok() => {
                let width = t[2..].parse::<usize>().unwrap_or(1).min(12);
                out.push_str(&format!("{n:0width$}"));
            }
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    OsString::from(out)
}
/// `(from, new name)` for every path that actually changes, or why the template is unusable.
pub(super) fn batch_plan(
    template: &str,
    paths: &[PathBuf],
) -> Result<Vec<(PathBuf, OsString)>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut plan = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let name = expand(template, path, i + 1);
        fsops::validate_name(&name)
            .map_err(|e| format!("{}: {}", name.to_string_lossy(), e.message))?;
        if !seen.insert((parent_of(path), name.clone())) {
            return Err(format!(
                "\"{}\" would be used twice; add {{n}}",
                name.to_string_lossy()
            ));
        }
        if path.file_name() != Some(name.as_os_str()) {
            plan.push((path.clone(), name));
        }
    }
    Ok(plan)
}
/// Walk `path` into archive sources named relative to `root`; symlinks and specials are skipped.
fn zip_sources(
    path: &FsPath,
    root: &FsPath,
    out: &mut Vec<browser_archive::CreateSource>,
) -> std::io::Result<()> {
    use browser_archive::CreateSource;
    let meta = std::fs::symlink_metadata(path)?;
    let name = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if meta.is_file() {
        out.push(CreateSource::file(path, name));
    } else if meta.is_dir() {
        out.push(CreateSource::directory(name));
        let mut children: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            zip_sources(&child.path(), root, out)?;
        }
    }
    Ok(())
}
/// Write the selection to a new ZIP in the first item's folder.
fn compress(paths: Vec<PathBuf>) -> Done {
    use browser_archive::{Cancellation, Control, CreateOptions, Limits, create_new};
    let fail = |message: String| Done {
        undo: vec![],
        refresh: vec![],
        failure: Some(message),
        verb: "Compressed",
        copied: 0,
    };
    let Some(parent) = paths
        .first()
        .and_then(|p| p.parent())
        .map(FsPath::to_path_buf)
    else {
        return fail("nothing to compress".into());
    };
    let stem = match paths.as_slice() {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()),
        _ => parent.file_name().map(|n| n.to_string_lossy().into_owned()),
    }
    .unwrap_or_else(|| "Archive".into());
    let zip = fsops::unique_in(&parent, OsString::from(format!("{stem}.zip")).as_os_str());
    let mut sources = Vec::new();
    for path in &paths {
        if let Err(e) = zip_sources(path, &parent, &mut sources) {
            return fail(format!("cannot read {}: {e}", path.display()));
        }
    }
    let cancel = Cancellation::default();
    let control = Control::new(&cancel, std::time::Duration::from_secs(3600), 256 << 30);
    match create_new(
        &sources,
        &zip,
        &CreateOptions::default(),
        None,
        Limits::default(),
        &control,
    ) {
        Ok(_) => Done {
            undo: vec![Undo::Remove(zip)],
            refresh: vec![parent],
            failure: None,
            verb: "Compressed",
            copied: 0,
        },
        Err(e) => fail(e.to_string()),
    }
}
/// Copy `path` into `dest_dir` (verified, never replacing), then send the original to Trash.
fn move_across(path: &FsPath, dest_dir: &FsPath) -> Result<(), String> {
    use browser_operations::{Cancellation, CopyLimits, CopyOptions, CopyOutcome, MetadataPolicy};
    let name = path.file_name().ok_or("path has no name")?;
    let limits = CopyLimits {
        max_entries: 1_000_000,
        max_bytes: 256 << 30,
        ..CopyLimits::default()
    };
    let options = CopyOptions {
        metadata: MetadataPolicy::Basic,
        root_publication: browser_operations::RootPublication::FinalizeBasicAfterPublish,
        ..CopyOptions::default()
    };
    let cancel = Cancellation::default();
    let plan = browser_operations::plan_copy_new_with_options(
        path.to_path_buf(),
        fsops::unique_in(dest_dir, name),
        limits,
        options,
        &cancel,
    )
    .map_err(|e| e.to_string())?;
    match browser_operations::execute_copy_new(plan, &cancel, |_| {}) {
        CopyOutcome::Committed(_) => fsops::trash(path)
            .map(|_| ())
            .map_err(|e| format!("copied, but the original could not be trashed: {e}")),
        CopyOutcome::Cancelled { .. } => Err("cancelled".into()),
        CopyOutcome::Failed { error, .. } => Err(error.to_string()),
    }
}
fn parent_of(path: &FsPath) -> Option<PathBuf> {
    path.parent().map(FsPath::to_path_buf)
}

impl FileOps {
    pub(super) fn busy(&self) -> bool {
        self.selecting.is_some()
            || self.cutting.is_some()
            || self.renaming.is_some()
            || self.running.is_some()
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
        let kind = match prompt.kind {
            PromptKind::Batch(paths) => {
                let prompt = Prompt {
                    kind: PromptKind::Batch(paths.clone()),
                    ..prompt
                };
                return self.commit_batch(paths, prompt);
            }
            other => other,
        };
        let name = OsString::from(prompt.text.trim_end_matches('\n'));
        if let Err(error) = fsops::validate_name(&name) {
            self.message = Some((format!("Invalid name: {}", error.message), true));
            self.prompt = Some(Prompt {
                kind,
                focus: true,
                ..prompt
            });
            return false;
        }
        let (label, verb) = match kind {
            PromptKind::Rename(_) => ("Renaming", "Renamed"),
            _ => ("Creating", "Created"),
        };
        self.spawn(label, move || {
            let (result, dir) = match kind {
                PromptKind::Batch(_) => unreachable!("batch prompts return before this point"),
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
                    copied: 0,
                },
                Err(e) => Done {
                    undo: vec![],
                    refresh: vec![],
                    failure: Some(e.to_string()),
                    verb,
                    copied: 0,
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
    fn commit_batch(&mut self, paths: Vec<PathBuf>, prompt: Prompt) -> bool {
        let plan = match batch_plan(&prompt.text, &paths) {
            Ok(plan) if plan.is_empty() => {
                self.message = Some(("Nothing to rename.".into(), false));
                return false;
            }
            Ok(plan) => plan,
            Err(message) => {
                self.message = Some((message, true));
                self.prompt = Some(Prompt {
                    focus: true,
                    ..prompt
                });
                return false;
            }
        };
        self.spawn("Renaming", move || {
            let mut done = Done {
                undo: Vec::new(),
                refresh: Vec::new(),
                failure: None,
                verb: "Renamed",
                copied: 0,
            };
            // All-or-nothing admission: no target may already exist (including another source).
            if let Some((_, name)) = plan.iter().find(|(from, name)| {
                parent_of(from).is_some_and(|dir| std::fs::symlink_metadata(dir.join(name)).is_ok())
            }) {
                done.failure = Some(format!(
                    "\"{}\" already exists; nothing was renamed",
                    name.to_string_lossy()
                ));
                return done;
            }
            for (from, name) in &plan {
                match fsops::rename(from, name) {
                    Ok(op) => {
                        done.refresh.extend(parent_of(from));
                        done.undo.push(op);
                    }
                    Err(e) => {
                        done.failure = Some(e.to_string());
                        break;
                    }
                }
            }
            done
        });
        true
    }
    /// F2 on several items: materialize them, then ask for a template.
    pub(super) fn batch_rename_selection(
        &mut self,
        snapshot: Option<browser_core::listing::ListingSnapshot>,
        selection: &Selection,
    ) {
        if !self.busy() {
            self.renaming = self.materialize(snapshot, selection, "rename");
        }
    }
    /// Compress the selection into a new "<name>.zip" beside it (never replaces; undo removes it).
    pub(super) fn compress_selection(
        &mut self,
        snapshot: Option<browser_core::listing::ListingSnapshot>,
        selection: &Selection,
    ) {
        if !self.busy() {
            self.zipping = self.materialize(snapshot, selection, "compress");
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
                copied: 0,
            };
            for path in &paths {
                match fsops::move_into(path, &dest) {
                    Ok(op) => {
                        done.refresh.extend(parent_of(path));
                        done.undo.push(op);
                    }
                    Err(e) if e.raw_os_error == Some(18) => {
                        // Different filesystem: verified copy, then the original goes to Trash.
                        match move_across(path, &dest) {
                            Ok(()) => {
                                done.refresh.extend(parent_of(path));
                                done.copied += 1;
                            }
                            Err(message) => {
                                done.failure = Some(message);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        done.failure = Some(e.to_string());
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
                copied: 0,
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
                            copied: 0,
                        }
                    });
                }
                Err(error) => {
                    self.message = Some((format!("Cannot trash selection: {error:?}"), true));
                }
            }
        }
        if let Some(work) = &mut self.zipping
            && let Some(result) = work.poll()
        {
            self.zipping = None;
            changed = true;
            match result {
                Ok(paths) => {
                    let paths = paths.into_paths();
                    self.spawn("Compressing", move || compress(paths));
                }
                Err(error) => self.message = Some((format!("Cannot compress: {error:?}"), true)),
            }
        }
        if let Some(work) = &mut self.renaming
            && let Some(result) = work.poll()
        {
            self.renaming = None;
            changed = true;
            match result {
                Ok(paths) => {
                    self.message = None;
                    self.open_prompt(
                        PromptKind::Batch(paths.into_paths()),
                        "{name}-{n}{ext}".into(),
                    );
                }
                Err(error) => self.message = Some((format!("Cannot rename: {error:?}"), true)),
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
            let copied = done.copied;
            self.message = Some(match done.failure {
                Some(f) => (format!("{} failed: {f}", done.verb), true),
                None if copied > 0 => (
                    format!(
                        "{} · {copied} copied across filesystems, originals in Trash",
                        done.verb
                    ),
                    false,
                ),
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
                PromptKind::Batch(_) => "Rename items to",
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
            let hint = match &prompt.kind {
                PromptKind::Batch(paths) => Some(match batch_plan(&prompt.text, paths) {
                    Ok(plan) => plan
                        .iter()
                        .take(3)
                        .map(|(from, to)| {
                            format!(
                                "{} → {}",
                                from.file_name().unwrap_or_default().to_string_lossy(),
                                to.to_string_lossy()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("   ·   "),
                    Err(message) => message,
                }),
                _ => None,
            };
            let main = row([
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
            .id("name-prompt-row");
            el = Some(match hint {
                Some(hint) => col([
                    main,
                    row([detail(hint).min_w(0).lines(1).clip()])
                        .pad((8, 2))
                        .fill(chrome())
                        .min_w(0),
                ])
                .shrink(0)
                .min_w(0),
                None => main,
            });
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
    fn matching_cut_moves_keeping_both_on_conflict_and_undo_returns_the_files() {
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
        assert_eq!(std::fs::read(dir.join("dest/b (2)")).unwrap(), b"2");
        assert!(
            !dir.join("b").exists(),
            "conflict keeps both, never replaces"
        );
        ops.undo_last();
        settle(&mut ops);
        assert!(dir.join("a").exists() && !dir.join("dest/a").exists());
        assert!(dir.join("b").exists() && !dir.join("dest/b (2)").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn batch_templates_expand_validate_and_reject_collisions() {
        let paths = vec![
            PathBuf::from("/d/a.txt"),
            PathBuf::from("/d/b.tar.gz"),
            PathBuf::from("/d/c"),
        ];
        let plan = batch_plan("img-{n:3}{ext}", &paths).unwrap();
        let names: Vec<_> = plan
            .iter()
            .map(|(_, n)| n.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["img-001.txt", "img-002.gz", "img-003"]);
        assert!(batch_plan("same", &paths).unwrap_err().contains("twice"));
        assert!(
            batch_plan("a/{n}", &paths).is_err(),
            "separators are rejected"
        );
        assert_eq!(
            batch_plan("{name}{ext}", &paths).unwrap().len(),
            0,
            "unchanged names are skipped"
        );
        assert_eq!(expand("{x}{name}", &paths[0], 1), OsString::from("{x}a"));
    }
    #[test]
    fn batch_rename_is_all_or_nothing_on_existing_targets_and_undoes_together() {
        let dir = std::env::temp_dir().join(format!("fileops-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a", "b"] {
            std::fs::write(dir.join(n), n).unwrap();
        }
        let mut ops = FileOps::default();
        ops.open_prompt(
            PromptKind::Batch(vec![dir.join("a"), dir.join("b")]),
            "x{n}".into(),
        );
        assert!(ops.commit_prompt());
        settle(&mut ops);
        assert!(dir.join("x1").exists() && dir.join("x2").exists());
        ops.undo_last();
        settle(&mut ops);
        assert!(dir.join("a").exists() && dir.join("b").exists());
        std::fs::write(dir.join("x2"), "taken").unwrap();
        ops.open_prompt(
            PromptKind::Batch(vec![dir.join("a"), dir.join("b")]),
            "x{n}".into(),
        );
        ops.commit_prompt();
        settle(&mut ops);
        assert!(
            dir.join("a").exists() && dir.join("b").exists(),
            "nothing renamed"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn compress_writes_a_zip_beside_the_selection_and_undo_removes_it() {
        let dir = std::env::temp_dir().join(format!("fileops-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d/sub")).unwrap();
        std::fs::write(dir.join("d/sub/f.txt"), b"hello").unwrap();
        let done = compress(vec![dir.join("d")]);
        assert!(done.failure.is_none(), "{:?}", done.failure);
        let zip = dir.join("d.zip");
        assert!(zip.metadata().unwrap().len() > 0);
        assert_eq!(done.undo, vec![Undo::Remove(zip.clone())]);
        // a second run keeps both
        assert!(compress(vec![dir.join("d")]).failure.is_none());
        assert!(dir.join("d (2).zip").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cross_filesystem_move_copies_verifies_and_trashes_the_original() {
        // tmpfs source, dev-drive destination: guaranteed different filesystems.
        let src = std::path::PathBuf::from(format!("/dev/shm/fileops-xdev-{}", std::process::id()));
        let dest = std::env::temp_dir().join(format!("fileops-xdev-dest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(&src, b"payload").unwrap();
        let mut ops = FileOps::default();
        assert!(ops.start_move(vec![src.clone()], dest.clone()));
        settle(&mut ops);
        let result = std::fs::read(dest.join(src.file_name().unwrap()));
        let _ = std::fs::remove_dir_all(src.parent().unwrap().join(format!(
                ".Trash-{}",
                std::fs::metadata("/proc/self")
                    .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
                    .unwrap_or(0)
            )));
        std::fs::remove_dir_all(&dest).unwrap();
        assert_eq!(result.unwrap(), b"payload");
        assert!(!src.exists(), "original left its folder");
    }
}
