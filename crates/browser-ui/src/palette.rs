//! Command palette: a filtered list of every app command, run with Enter.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Cmd {
    Open,
    NewTab,
    CloseTab,
    ReopenTab,
    NextTab,
    PrevTab,
    NewFolder,
    NewFile,
    Rename,
    Trash,
    Compress,
    Undo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    ToggleHidden,
    SortName,
    SortType,
    SortModified,
    SortSize,
    FindFiles,
    GoHome,
    GoUp,
    GoBack,
    GoForward,
    Bookmark,
    ToggleInspector,
    ToggleGrid,
    CopyPath,
    OpenTerminal,
    GoTo,
    ToggleSplit,
    CopyToOther,
    MoveToOther,
}
pub(super) const COMMANDS: &[(Cmd, &str, &str)] = &[
    (Cmd::Open, "Open", "Enter"),
    (Cmd::NewTab, "New tab", "Ctrl+T"),
    (Cmd::CloseTab, "Close tab", "Ctrl+W"),
    (Cmd::ReopenTab, "Reopen closed tab", "Ctrl+Shift+T"),
    (Cmd::NextTab, "Next tab", "Ctrl+Tab"),
    (Cmd::PrevTab, "Previous tab", "Ctrl+Shift+Tab"),
    (Cmd::NewFolder, "New folder", "Ctrl+Shift+N"),
    (Cmd::NewFile, "New file", "Ctrl+N"),
    (Cmd::Rename, "Rename", "F2"),
    (Cmd::Trash, "Move to Trash", "Delete"),
    (Cmd::Compress, "Compress to ZIP", ""),
    (Cmd::Undo, "Undo", "Ctrl+Z"),
    (Cmd::Cut, "Cut", "Ctrl+X"),
    (Cmd::Copy, "Copy", "Ctrl+C"),
    (Cmd::Paste, "Paste", "Ctrl+V"),
    (Cmd::GoTo, "Go to anywhere…", "Ctrl+K"),
    (Cmd::ToggleSplit, "Toggle split view", "Ctrl+\\"),
    (Cmd::CopyToOther, "Copy to other pane", "F5"),
    (Cmd::MoveToOther, "Move to other pane", "F6"),
    (Cmd::SelectAll, "Select all", "Ctrl+A"),
    (Cmd::ToggleHidden, "Show or hide hidden files", "Ctrl+H"),
    (Cmd::SortName, "Sort by name", ""),
    (Cmd::SortType, "Sort by type", ""),
    (Cmd::SortModified, "Sort by modified date", ""),
    (Cmd::SortSize, "Sort by size", ""),
    (Cmd::FindFiles, "Find files in this folder", "Ctrl+Shift+F"),
    (Cmd::GoHome, "Go to Home", ""),
    (Cmd::GoUp, "Go to parent folder", "Alt+Up"),
    (Cmd::GoBack, "Go back", "Alt+Left"),
    (Cmd::GoForward, "Go forward", "Alt+Right"),
    (Cmd::Bookmark, "Bookmark this folder", "Ctrl+D"),
    (Cmd::ToggleInspector, "Toggle inspector", "Space"),
    (Cmd::CopyPath, "Copy full path", "Ctrl+Shift+C"),
    (Cmd::OpenTerminal, "Open terminal here", ""),
    (
        Cmd::ToggleGrid,
        "Switch between list and grid view",
        "Ctrl+G",
    ),
];
/// Right-click menu, in display order.
const CONTEXT: &[Cmd] = &[
    Cmd::Open,
    Cmd::Rename,
    Cmd::Cut,
    Cmd::Copy,
    Cmd::Paste,
    Cmd::Trash,
    Cmd::Compress,
    Cmd::NewFolder,
    Cmd::NewFile,
    Cmd::CopyPath,
    Cmd::OpenTerminal,
    Cmd::Bookmark,
];
const SHOWN: usize = 8;

/// Case-insensitive subsequence match; earlier and tighter matches rank first.
pub(super) fn matches(query: &str) -> Vec<(Cmd, &'static str, &'static str)> {
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut scored: Vec<(usize, usize, (Cmd, &str, &str))> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(order, c)| {
            let name = c.1.to_lowercase();
            let (mut at, mut first, mut last) = (0, None, 0);
            for ch in &q {
                let found = name[at..].find(*ch)?;
                let pos = at + found;
                first.get_or_insert(pos);
                last = pos;
                at = pos + ch.len_utf8();
            }
            Some((first.map_or(0, |f| f * 64 + (last - f)), order, *c))
        })
        .collect();
    scored.sort_by_key(|(score, order, _)| (*score, *order));
    scored.into_iter().map(|(_, _, c)| c).collect()
}

#[derive(Default)]
pub(super) struct Palette {
    menu: bool,
    query: String,
    selected: usize,
    focus: bool,
}
pub(super) enum Outcome {
    Open,
    Close,
    Run(Cmd),
}
impl Palette {
    /// A fixed context menu: no text field, Escape or a choice closes it.
    pub(super) fn context() -> Self {
        Self {
            menu: true,
            ..Self::default()
        }
    }
    pub(super) fn open() -> Self {
        Self {
            focus: true,
            ..Self::default()
        }
    }
    fn build_menu(&mut self, ui: &mut Ui) -> (El, Outcome) {
        let mut outcome = Outcome::Open;
        if ui.shortcuts().iter().any(|p| p.key == Key::Escape) {
            outcome = Outcome::Close;
        }
        let mut items = Vec::new();
        for (i, cmd) in CONTEXT.iter().enumerate() {
            let Some((_, name, keys)) = COMMANDS.iter().find(|c| c.0 == *cmd) else {
                continue;
            };
            let id = format!("context-{i}");
            if clicked(ui, &id) {
                outcome = Outcome::Run(*cmd);
            }
            items.push(
                action(
                    ui,
                    &id,
                    name,
                    row([label(*name).grow(1).min_w(0), detail(*keys)])
                        .gap(16)
                        .pad((10, 5))
                        .w(pct(100)),
                    true,
                    false,
                )
                .w(pct(100))
                .pad(0),
            );
        }
        (
            col(items)
                .w(pct(100))
                .fill(chrome())
                .shrink(0)
                .id("context-menu")
                .named("Context menu"),
            outcome,
        )
    }
    pub(super) fn build(&mut self, ui: &mut Ui) -> (El, Outcome) {
        if self.menu {
            return self.build_menu(ui);
        }
        if std::mem::take(&mut self.focus) {
            ui.focus("palette-input");
        }
        let mut outcome = Outcome::Open;
        let mut moved = 0isize;
        for press in ui.keys("palette-input").to_vec() {
            match press.key {
                Key::Escape => outcome = Outcome::Close,
                Key::Down => moved += 1,
                Key::Up => moved -= 1,
                _ => {}
            }
        }
        let field = text_edit(ui, "palette-input", &mut self.query, TextOpts::default());
        if field.changed.changed {
            self.selected = 0;
        }
        let list = matches(&self.query);
        let shown = list.len().min(SHOWN);
        if shown > 0 {
            self.selected = (self.selected as isize + moved).rem_euclid(shown as isize) as usize;
        }
        if field.changed.submitted
            && let Some((cmd, _, _)) = list.get(self.selected)
        {
            outcome = Outcome::Run(*cmd);
        }
        let mut items = vec![
            row([
                detail("Command"),
                field.el.grow(1).basis(0).min_w(0).fill(ground()),
            ])
            .gap(8)
            .pad((8, 4))
            .align(Align::Center),
        ];
        for (i, (cmd, name, keys)) in list.iter().take(SHOWN).enumerate() {
            let id = format!("palette-{i}");
            if clicked(ui, &id) {
                outcome = Outcome::Run(*cmd);
            }
            items.push(
                action(
                    ui,
                    &id,
                    name,
                    row([label(*name).grow(1).min_w(0), detail(*keys)])
                        .gap(8)
                        .pad((10, 5))
                        .w(pct(100)),
                    true,
                    i == self.selected,
                )
                .w(pct(100))
                .pad(0),
            );
        }
        if list.is_empty() {
            items.push(detail("No matching command").pad((10, 6)));
        }
        (
            col(items)
                .w(pct(100))
                .fill(chrome())
                .shrink(0)
                .id("palette")
                .named("Command palette"),
            outcome,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subsequence_matching_ranks_prefixes_first_and_rejects_non_matches() {
        assert_eq!(matches("")[0].0, Cmd::Open);
        assert_eq!(matches("rena")[0].0, Cmd::Rename);
        assert!(matches("zzzz").is_empty());
        assert!(CONTEXT.iter().all(|c| COMMANDS.iter().any(|k| k.0 == *c)));
        let names: Vec<_> = matches("new").iter().take(3).map(|c| c.1).collect();
        assert_eq!(names, ["New tab", "New folder", "New file"]);
        assert_eq!(
            matches("sbs")[0].0,
            Cmd::SortSize,
            "subsequence across words"
        );
    }
}
