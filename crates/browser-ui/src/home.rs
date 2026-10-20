//! Home page: user folders, drives with usage, bookmarks and recent folders as cards.
use super::*;

fn card(ui: &mut Ui, id: &str, title: &str, sub: Option<String>, used: Option<u64>) -> El {
    let mut lines = vec![label(title).text_size(14)];
    if let Some(sub) = sub {
        lines.push(detail(sub));
    }
    if let Some(used) = used {
        lines.push(
            row([
                block(0, 3)
                    .w(pct(used.min(100) as f32))
                    .radius(2)
                    .fill(if used >= 90 {
                        color(0xd9534f)
                    } else {
                        selected()
                    }),
                spacer(),
            ])
            .w(pct(100))
            .radius(2)
            .fill(ground()),
        );
    }
    action(
        ui,
        id,
        title,
        row([
            glyph(Glyph::Folder, color(0xe2bd55)),
            col(lines).gap(4).grow(1).min_w(0),
        ])
        .gap(12)
        .align(Align::Center)
        .pad((14, 12))
        .w(pct(100)),
        true,
        false,
    )
    .w(pct(100))
    .pad(0)
    .radius(8)
    .fill(chrome())
    .on(State::Hover, |s| s.fill(line()))
}
/// Lay cards out three to a row so the grid stays inside any window width.
fn grid(cards: Vec<El>) -> El {
    let mut rows = Vec::new();
    let mut cards = cards.into_iter();
    loop {
        let chunk: Vec<El> = cards
            .by_ref()
            .take(3)
            .map(|c| c.grow(1).basis(0).min_w(0))
            .collect();
        if chunk.is_empty() {
            break;
        }
        let missing = 3 - chunk.len();
        let mut r = chunk;
        r.extend((0..missing).map(|_| spacer().grow(1).basis(0)));
        rows.push(row(r).gap(10).w(pct(100)));
    }
    col(rows).gap(10).w(pct(100))
}
fn section(title: &str, cards: Vec<El>) -> Option<El> {
    (!cards.is_empty()).then(|| {
        col([detail(title.to_uppercase()).pad((2, 0)), grid(cards)])
            .gap(8)
            .w(pct(100))
    })
}
impl TabView {
    /// Returns the path to open when a card was clicked.
    pub(super) fn home_page(
        &mut self,
        ui: &mut Ui,
        places: &places::Places,
    ) -> (El, Option<PathBuf>) {
        let mut open = None;
        let mut take = |ui: &mut Ui, id: String, path: &FsPath| {
            if clicked(ui, &id) {
                open = Some(path.to_path_buf());
            }
        };
        let mut users = Vec::new();
        for name in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Videos",
        ] {
            let path = self.home.join(name);
            if path.is_dir() {
                let id = format!("home-user-{name}");
                take(ui, id.clone(), &path);
                users.push(card(ui, &id, name, None, None));
            }
        }
        let id = "home-user-home".to_string();
        take(ui, id.clone(), &self.home);
        users.insert(0, card(ui, &id, "Home", None, None));
        let mut drives = Vec::new();
        for d in places.drives() {
            let id = format!("home-drive-{}", d.path.display());
            take(ui, id.clone(), &d.path);
            let used = (d.total - d.free.min(d.total)) * 100 / d.total;
            let sub = format!("{} free of {}  ·  {used}%", bytes(d.free), bytes(d.total));
            drives.push(card(ui, &id, &d.name, Some(sub), Some(used)));
        }
        let mut marks = Vec::new();
        for (kind, list) in [("bookmark", &places.bookmarks), ("recent", &places.recent)] {
            let mut cards = Vec::new();
            for (i, p) in list.iter().enumerate() {
                let id = format!("home-{kind}-{i}");
                take(ui, id.clone(), p);
                cards.push(card(
                    ui,
                    &id,
                    &place_name(p),
                    Some(p.display().to_string()),
                    None,
                ));
            }
            marks.push(cards);
        }
        let [bookmarks, recent] = <[_; 2]>::try_from(marks).unwrap_or_default();
        let sections = [
            section("User directories", users),
            section("Drives", drives),
            section("Bookmarks", bookmarks),
            section("Recent", recent),
        ];
        let page = col(std::iter::once(label("Home").text_size(22).pad((2, 4)))
            .chain(sections.into_iter().flatten())
            .collect::<Vec<_>>())
        .gap(20)
        .pad(24)
        .w(pct(100));
        (
            col([page])
                .grow(1)
                .basis(0)
                .min_w(0)
                .min_h(0)
                .h(pct(100))
                .fill(ground())
                .id(self.pid("home-page")),
            open,
        )
    }
}
