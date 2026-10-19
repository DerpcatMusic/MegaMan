use browser_archive::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".test-work")
            .join(format!(
                "browser-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn put(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Clone)]
struct Entry {
    name: Vec<u8>,
    bytes: Vec<u8>,
    method: u16,
    flags: u16,
}
impl Entry {
    fn new(name: &str) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            bytes: b"payload".to_vec(),
            method: 0,
            flags: 1 << 11,
        }
    }
}
fn word(out: &mut Vec<u8>, value: u16) {
    out.extend(value.to_le_bytes());
}
fn dword(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}
// Independent fixture writer permits duplicate and unsafe names which ordinary
// archive writers deliberately reject. No member is inflated to build a fixture.
fn archive(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for entry in entries {
        offsets.push(out.len() as u32);
        dword(&mut out, 0x04034b50);
        word(&mut out, 20);
        word(&mut out, entry.flags);
        word(&mut out, entry.method);
        word(&mut out, 0);
        word(&mut out, 0x21);
        dword(&mut out, crc32fast::hash(&entry.bytes));
        dword(&mut out, entry.bytes.len() as u32);
        dword(&mut out, entry.bytes.len() as u32);
        word(&mut out, entry.name.len() as u16);
        word(&mut out, 0);
        out.extend(&entry.name);
        out.extend(&entry.bytes);
    }
    let start = out.len();
    for (entry, offset) in entries.iter().zip(offsets) {
        dword(&mut out, 0x02014b50);
        word(&mut out, 0x0314);
        word(&mut out, 20);
        word(&mut out, entry.flags);
        word(&mut out, entry.method);
        word(&mut out, 0);
        word(&mut out, 0x21);
        dword(&mut out, crc32fast::hash(&entry.bytes));
        dword(&mut out, entry.bytes.len() as u32);
        dword(&mut out, entry.bytes.len() as u32);
        word(&mut out, entry.name.len() as u16);
        word(&mut out, 0);
        word(&mut out, 0);
        word(&mut out, 0);
        word(&mut out, 0);
        dword(
            &mut out,
            if entry.name.ends_with(b"/") {
                0o040700 << 16
            } else {
                0o100600 << 16
            },
        );
        dword(&mut out, offset);
        out.extend(&entry.name);
    }
    let size = out.len() - start;
    dword(&mut out, 0x06054b50);
    word(&mut out, 0);
    word(&mut out, 0);
    word(&mut out, entries.len() as u16);
    word(&mut out, entries.len() as u16);
    dword(&mut out, size as u32);
    dword(&mut out, start as u32);
    word(&mut out, 0);
    out
}
fn directory(name: &str) -> Entry {
    let mut entry = Entry::new(name);
    entry.bytes.clear();
    entry
}
fn terminal(browser: &ArchiveBrowser, id: u64) -> Box<BrowserPage> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(page) = browser.poll() {
            assert_eq!(page.request_id, id, "stale mailbox delivery");
            if page.catalog.state != BrowserCatalogState::Opening {
                for node in &page.nodes {
                    if let Some(prefix) = &node.navigation {
                        assert_eq!(
                            ArchivePrefix::from_components(prefix.components().to_vec()).unwrap(),
                            *prefix
                        );
                    }
                }
                return page;
            }
        }
        assert!(
            Instant::now() < deadline,
            "archive browser did not deliver request {id}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn list(
    browser: &ArchiveBrowser,
    id: u64,
    prefix: ArchivePrefix,
    offset: usize,
    limit: usize,
) -> Box<BrowserPage> {
    browser
        .request_list(ListRequest {
            request_id: id,
            prefix,
            offset,
            limit,
        })
        .unwrap();
    terminal(browser, id)
}
fn open(path: &Path) -> ArchiveBrowser {
    ArchiveBrowser::open(BrowserOptions::new(path.to_owned())).unwrap()
}

#[test]
fn implicit_and_explicit_directories_navigate_without_host_member_paths() {
    let f = Fixture::new();
    let path = f.put(
        "source.not-a-zip-extension",
        &archive(&[
            Entry::new("docs/a.txt"),
            Entry::new("docs/deeper/b.txt"),
            directory("empty/"),
            Entry::new("root.bin"),
        ]),
    );
    let browser = open(&path);
    let root = terminal(&browser, 0);
    assert_eq!(root.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(root.catalog.records_seen, 4);
    assert_eq!(root.format, Some(ArchiveFormat::Zip));
    assert_eq!(root.source.as_ref(), &path);
    assert_eq!(
        root.nodes
            .iter()
            .map(|n| n.display_name.as_str())
            .collect::<Vec<_>>(),
        ["docs", "empty", "root.bin"]
    );
    assert!(root.nodes[0].synthetic);
    assert!(!root.nodes[1].synthetic);
    assert!(root.nodes[1].record_id.is_some());
    let docs = root.nodes[0].navigation.clone().unwrap();
    assert_eq!(docs.components(), ["docs"]);
    assert_eq!(docs.parent(), Some(ArchivePrefix::root()));
    let page = list(&browser, 1, docs.clone(), 0, 512);
    assert_eq!(page.source.as_ref(), &path);
    assert_eq!(
        page.nodes
            .iter()
            .map(|n| n.display_name.as_str())
            .collect::<Vec<_>>(),
        ["deeper", "a.txt"]
    );
    let deeper = page.nodes[0].navigation.clone().unwrap();
    assert_eq!(deeper.components(), ["docs", "deeper"]);
    assert_eq!(deeper.parent(), Some(docs));
    let leaf = list(&browser, 2, deeper, 0, 512);
    assert_eq!(leaf.nodes[0].display_name, "b.txt");
    assert!(leaf.nodes[0].navigation.is_none());
    let again = list(&browser, 3, ArchivePrefix::root(), 0, 512);
    assert_eq!(again.nodes[0].id, root.nodes[0].id);
    assert_eq!(again.generation, browser.generation());
}

#[test]
fn large_folder_has_complete_stable_pages_beyond_the_first_512() {
    let f = Fixture::new();
    let entries: Vec<_> = (0..1700)
        .rev()
        .map(|i| Entry::new(&format!("file-{i:04}")))
        .collect();
    let path = f.put("large.zip", &archive(&entries));
    let browser = open(&path);
    let mut page = terminal(&browser, 0);
    let mut names = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let mut request = 0;
    loop {
        assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
        assert_eq!(page.total_children, 1700);
        assert!(page.nodes.len() <= MAX_BROWSER_PAGE_SIZE);
        for node in &page.nodes {
            names.push(node.display_name.clone());
            assert!(ids.insert(node.id));
        }
        let Some(offset) = page.next_offset else {
            break;
        };
        request += 1;
        page = list(&browser, request, ArchivePrefix::root(), offset, 512);
    }
    assert_eq!(names.len(), 1700);
    assert_eq!(names.first().unwrap(), "file-0000");
    assert_eq!(names.last().unwrap(), "file-1699");
    let beyond = list(
        &browser,
        request + 1,
        ArchivePrefix::root(),
        usize::MAX,
        512,
    );
    assert!(beyond.nodes.is_empty());
    assert!(beyond.next_offset.is_none());
}

#[test]
fn duplicates_unsafe_names_and_disabled_codecs_remain_visible() {
    let f = Fixture::new();
    let mut invalid = Entry::new("bad");
    invalid.name = vec![0xff];
    let mut unsupported = Entry::new("unsupported");
    unsupported.method = 12;
    let path = f.put(
        "mixed.zip",
        &archive(&[
            Entry::new("same"),
            Entry::new("same"),
            directory("dup/"),
            directory("dup/"),
            Entry::new("dup/child"),
            Entry::new("../escape"),
            invalid,
            unsupported,
        ]),
    );
    let browser = open(&path);
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.records_seen, 8);
    assert_eq!(page.nodes.len(), 8);
    let duplicate: Vec<_> = page
        .nodes
        .iter()
        .filter(|node| node.display_name == "same")
        .collect();
    assert_eq!(duplicate.len(), 2);
    assert_ne!(duplicate[0].id, duplicate[1].id);
    for node in duplicate {
        assert!(node.navigation.is_none());
        assert_eq!(
            node.issue.as_ref().unwrap().kind,
            BrowserIssueKind::AmbiguousName
        );
    }
    let invalid: Vec<_> = page
        .nodes
        .iter()
        .filter(|node| {
            node.issue
                .as_ref()
                .is_some_and(|i| i.kind == BrowserIssueKind::UnsafeName)
        })
        .collect();
    assert_eq!(invalid.len(), 2);
    assert!(invalid.iter().all(|node| node.navigation.is_none()));
    let unsupported = page
        .nodes
        .iter()
        .find(|n| n.display_name == "unsupported")
        .unwrap();
    assert_eq!(
        unsupported.issue.as_ref().unwrap().kind,
        BrowserIssueKind::UnsupportedCompression
    );
    let dup: Vec<_> = page
        .nodes
        .iter()
        .filter(|n| n.display_name == "dup")
        .collect();
    assert_eq!(dup.len(), 3);
    assert_eq!(dup.iter().filter(|n| n.navigation.is_some()).count(), 1);
    let child = list(
        &browser,
        1,
        dup.iter().find_map(|n| n.navigation.clone()).unwrap(),
        0,
        512,
    );
    assert_eq!(child.nodes[0].display_name, "child");
    assert!(ArchivePrefix::from_components(vec!["..".into()]).is_err());
    assert!(ArchivePrefix::from_components(vec!["a/b".into()]).is_err());
}

#[test]
fn incomplete_scan_and_derived_index_are_never_complete_catalogs() {
    let f = Fixture::new();
    let path = f.put(
        "partial.zip",
        &archive(&[Entry::new("a"), Entry::new("b"), Entry::new("c")]),
    );
    let mut options = BrowserOptions::new(path.clone());
    options.limits.metadata_bytes = 2200;
    let browser = ArchiveBrowser::open(options).unwrap();
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.state, BrowserCatalogState::LimitReached);
    assert_eq!(page.catalog.records_seen, 1);
    assert_eq!(page.nodes.len(), 1);
    assert!(page.catalog.error.is_some());
    let mut options = BrowserOptions::new(path);
    options.index_nodes = 1;
    let browser = ArchiveBrowser::open(options).unwrap();
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.state, BrowserCatalogState::LimitReached);
    assert_eq!(page.catalog.records_seen, 3);
    assert_eq!(page.catalog.indexed_records, 1);
    assert_eq!(page.nodes.len(), 1);
}

#[test]
fn corrupted_payload_is_browsed_without_inflation() {
    let f = Fixture::new();
    let mut bytes = archive(&[Entry::new("payload")]);
    bytes[30 + b"payload".len()] ^= 1;
    let path = f.put("crc.zip", &bytes);
    let browser = open(&path);
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(page.nodes[0].display_name, "payload");
    assert_eq!(
        fs::read_dir(&f.0).unwrap().count(),
        1,
        "browsing materialized a member"
    );
    let cancel = Cancellation::default();
    let control = Control::new(&cancel, Duration::from_secs(1), 1024);
    let engine = ArchiveSession::open(&path, Limits::default(), &control).unwrap();
    assert!(
        engine
            .read_entry(engine.entries()[0].id, &mut Vec::new(), None, &control)
            .is_err(),
        "fixture payload was not corrupt"
    );
}

#[test]
fn replacement_is_terminal_and_requires_a_new_browser_generation() {
    let f = Fixture::new();
    let path = f.put("source.zip", &archive(&[Entry::new("old")]));
    let browser = open(&path);
    let original = terminal(&browser, 0);
    let replacement = f.put("replacement.zip", &archive(&[Entry::new("new")]));
    fs::rename(replacement, &path).unwrap();
    let changed = list(&browser, 1, ArchivePrefix::root(), 0, 512);
    assert_eq!(changed.catalog.state, BrowserCatalogState::Failed);
    assert!(!changed.catalog.source_valid);
    assert!(changed.nodes.is_empty());
    assert_eq!(
        changed.error.unwrap().kind,
        BrowserErrorKind::Archive(ErrorKind::SourceChanged)
    );
    let again = list(&browser, 2, ArchivePrefix::root(), 0, 512);
    assert!(again.nodes.is_empty());
    let reopened = open(&path);
    let page = terminal(&reopened, 0);
    assert_ne!(page.generation, original.generation);
    assert_eq!(page.nodes[0].display_name, "new");
}

#[test]
fn invalid_requests_do_not_replace_the_current_request() {
    let f = Fixture::new();
    let path = f.put("source.zip", &archive(&[Entry::new("a")]));
    let browser = open(&path);
    terminal(&browser, 0);
    assert!(matches!(
        browser.request_list(ListRequest::root(1, 0, 513)),
        Err(BrowserSubmitError::PageSize { .. })
    ));
    assert!(matches!(
        browser.request_list(ListRequest::root(0, 0, 512)),
        Err(BrowserSubmitError::NonMonotonicRequest { .. })
    ));
    let missing = ArchivePrefix::from_components(vec!["absent".into()]).unwrap();
    let page = list(&browser, 1, missing, 0, 512);
    assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(page.error.unwrap().kind, BrowserErrorKind::PrefixNotFound);
    let valid = list(&browser, 2, ArchivePrefix::root(), 0, 512);
    assert_eq!(valid.nodes[0].display_name, "a");
    assert!(browser.cancel());
    let cancelled = browser.poll().unwrap();
    assert_eq!(cancelled.request_id, 2);
    assert_eq!(cancelled.catalog.state, BrowserCatalogState::Cancelled);
    assert_eq!(
        browser.request_list(ListRequest::root(3, 0, 1)),
        Err(BrowserSubmitError::Closed)
    );
}

#[test]
fn page_byte_budget_preserves_long_prefixes_and_continuation() {
    let f = Fixture::new();
    let parent = "x".repeat(4000);
    let entries: Vec<_> = (0..400)
        .map(|i| directory(&format!("{parent}/child-{i:04}/")))
        .collect();
    let path = f.put("names.zip", &archive(&entries));
    let browser = open(&path);
    let root = terminal(&browser, 0);
    assert!(root.nodes[0].name_truncated);
    assert_eq!(root.nodes[0].display_name.len(), 1024);
    let prefix = root.nodes[0].navigation.clone().unwrap();
    assert_eq!(prefix.components(), [parent]);
    let first = list(&browser, 1, prefix.clone(), 0, 512);
    assert!(first.nodes.len() < 400);
    assert!(!first.nodes.is_empty());
    assert_eq!(first.total_children, 400);
    let second = list(&browser, 2, prefix, first.next_offset.unwrap(), 512);
    assert_eq!(first.nodes.len() + second.nodes.len(), 400);
    assert!(second.next_offset.is_none());
}

#[cfg(unix)]
#[test]
fn native_source_identity_is_not_lossily_decoded_or_joined_with_members() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let f = Fixture::new();
    let source =
        f.0.join(OsString::from_vec(vec![b'a', 0xff, b'.', b'z', b'i', b'p']));
    fs::write(&source, archive(&[Entry::new("שלום/🦀")])).unwrap();
    let browser = open(&source);
    let root = terminal(&browser, 0);
    assert_eq!(root.source.as_ref(), &source);
    assert_eq!(browser.source(), source);
    let page = list(
        &browser,
        1,
        root.nodes[0].navigation.clone().unwrap(),
        0,
        512,
    );
    assert_eq!(page.nodes[0].display_name, "🦀");
    assert_eq!(page.source.as_ref(), &source);
}

#[test]
fn non_zip_and_deadline_failures_are_explicit() {
    let f = Fixture::new();
    let text = f.put("not.zip", b"ordinary text");
    let browser = open(&text);
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.state, BrowserCatalogState::Failed);
    assert!(page.catalog.error.is_some());
    assert_eq!(page.format, None);
    let path = f.put("valid.zip", &archive(&[Entry::new("a")]));
    let mut options = BrowserOptions::new(path);
    options.list_timeout = Duration::ZERO;
    let browser = ArchiveBrowser::open(options).unwrap();
    let page = terminal(&browser, 0);
    assert_eq!(
        page.error.unwrap().kind,
        BrowserErrorKind::Archive(ErrorKind::Deadline)
    );
    assert!(page.nodes.is_empty());
    assert!(ArchiveBrowser::open(BrowserOptions::new(PathBuf::from("relative.zip"))).is_err());
}

#[test]
fn excessive_depth_options_reject_before_spawn_and_records_remain_visible_at_default_limit() {
    let f = Fixture::new();
    let name = format!("{}/leaf", vec!["d"; 257].join("/"));
    let path = f.put("depth.zip", &archive(&[Entry::new(&name)]));
    let mut options = BrowserOptions::new(path.clone());
    options.limits.path_components = 300;
    let error = match ArchiveBrowser::open(options) {
        Err(error) => error,
        Ok(_) => panic!("incompatible browser depth admitted"),
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("256"));
    let browser = open(&path);
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(page.catalog.records_seen, 1);
    assert_eq!(page.catalog.indexed_records, 1);
    assert_eq!(page.nodes.len(), 1);
    assert!(page.nodes[0].navigation.is_none());
    assert_eq!(
        page.nodes[0].issue.as_ref().unwrap().kind,
        BrowserIssueKind::UnsafeName
    );
    assert!(page.next_offset.is_none());
}

#[test]
fn cp437_expansion_keeps_oversize_names_visible_and_valid_large_prefix_navigable() {
    let f = Fixture::new();
    let entries: Vec<_> = [21844, 21845]
        .into_iter()
        .map(|length| {
            let mut name = vec![0xb3; length];
            name.push(b'/');
            Entry {
                name,
                bytes: Vec::new(),
                flags: 0,
                method: 0,
            }
        })
        .collect();
    let path = f.put("cp437.zip", &archive(&entries));
    let browser = open(&path);
    let page = terminal(&browser, 0);
    assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(page.catalog.indexed_records, 2);
    assert_eq!(page.nodes.len(), 2);
    assert!(page.next_offset.is_none());
    let valid = page.nodes.iter().find(|n| n.navigation.is_some()).unwrap();
    assert!(valid.name_truncated);
    let prefix = valid.navigation.clone().unwrap();
    assert_eq!(prefix.components()[0].len(), 65532);
    let invalid = page.nodes.iter().find(|n| n.navigation.is_none()).unwrap();
    let issue = invalid.issue.as_ref().unwrap();
    assert_eq!(issue.kind, BrowserIssueKind::UnsafeName);
    assert!(issue.message.contains("decoded") && issue.message.contains("65535"));
    let child = list(&browser, 1, prefix, 0, 512);
    assert!(child.nodes.is_empty());
    assert!(child.error.is_none());
}

#[test]
fn maximum_valid_prefix_first_row_fits_the_page_budget() {
    let f = Fixture::new();
    let mut components = vec!["x".repeat(509)];
    components.extend((0..255).map(|_| "x".repeat(254)));
    assert_eq!(components.iter().map(|c| c.len() + 1).sum::<usize>(), 65535);
    let prefix = ArchivePrefix::from_components(components.clone()).unwrap();
    let name = format!("{}/", components.join("/"));
    assert_eq!(name.len(), 65535);
    let path = f.put("boundary.zip", &archive(&[directory(&name)]));
    let browser = open(&path);
    let root = terminal(&browser, 0);
    assert_eq!(root.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(root.nodes.len(), 1);
    let page = list(&browser, 1, prefix.parent().unwrap(), 0, 512);
    assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
    assert_eq!(page.total_children, 1);
    assert_eq!(
        page.nodes.len(),
        1,
        "first valid row must fit rather than repeat the same offset"
    );
    assert_eq!(page.nodes[0].navigation.as_ref(), Some(&prefix));
    assert!(page.error.is_none());
    assert!(page.next_offset.is_none());
    let empty = list(&browser, 2, prefix, 0, 512);
    assert!(empty.nodes.is_empty());
    assert!(empty.error.is_none());
}
