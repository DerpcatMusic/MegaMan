#![cfg(target_os = "linux")]

use browser_archive::*;
use std::{
    fs,
    path::PathBuf,
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
                "jobs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn put(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        path
    }
    fn no_staging(&self) {
        assert!(fs::read_dir(&self.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".megaman-archive-")
        }));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Clone)]
struct Record {
    name: Vec<u8>,
    bytes: Vec<u8>,
    mode: u32,
    flags: u16,
    method: u16,
}
impl Record {
    fn file(name: &str, bytes: &[u8]) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            bytes: bytes.to_vec(),
            mode: 0o100600,
            flags: 0,
            method: 0,
        }
    }
}
fn u16le(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}
fn u32le(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}
/// Independently generated classic Stored ZIP records, including deliberately
/// malformed names and repeated central records that convenience writers reject.
fn stored(records: &[Record]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    for record in records {
        offsets.push(bytes.len() as u32);
        u32le(&mut bytes, 0x04034b50);
        u16le(&mut bytes, 20);
        u16le(&mut bytes, record.flags);
        u16le(&mut bytes, record.method);
        u16le(&mut bytes, 0);
        u16le(&mut bytes, 0x21);
        u32le(&mut bytes, crc32fast::hash(&record.bytes));
        u32le(&mut bytes, record.bytes.len() as u32);
        u32le(&mut bytes, record.bytes.len() as u32);
        u16le(&mut bytes, record.name.len() as u16);
        u16le(&mut bytes, 0);
        bytes.extend(&record.name);
        bytes.extend(&record.bytes);
    }
    let central_start = bytes.len();
    for (record, offset) in records.iter().zip(offsets) {
        u32le(&mut bytes, 0x02014b50);
        u16le(&mut bytes, 0x0314);
        u16le(&mut bytes, 20);
        u16le(&mut bytes, record.flags);
        u16le(&mut bytes, record.method);
        u16le(&mut bytes, 0);
        u16le(&mut bytes, 0x21);
        u32le(&mut bytes, crc32fast::hash(&record.bytes));
        u32le(&mut bytes, record.bytes.len() as u32);
        u32le(&mut bytes, record.bytes.len() as u32);
        u16le(&mut bytes, record.name.len() as u16);
        u16le(&mut bytes, 0);
        u16le(&mut bytes, 0);
        u16le(&mut bytes, 0);
        u16le(&mut bytes, 0);
        u32le(&mut bytes, record.mode << 16);
        u32le(&mut bytes, offset);
        bytes.extend(&record.name);
    }
    let central_size = bytes.len() - central_start;
    u32le(&mut bytes, 0x06054b50);
    u16le(&mut bytes, 0);
    u16le(&mut bytes, 0);
    u16le(&mut bytes, records.len() as u16);
    u16le(&mut bytes, records.len() as u16);
    u32le(&mut bytes, central_size as u32);
    u32le(&mut bytes, central_start as u32);
    u16le(&mut bytes, 0);
    bytes
}
fn wait<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = read() {
            return result;
        }
        assert!(Instant::now() < end, "worker delivery timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn browser(path: PathBuf) -> (ArchiveBrowser, Box<BrowserPage>) {
    let browser = ArchiveBrowser::open(BrowserOptions::new(path)).unwrap();
    let page = wait(|| {
        browser
            .poll()
            .filter(|p| p.catalog.state != BrowserCatalogState::Opening)
    });
    (browser, page)
}
fn jobs() -> ExtractionJobs {
    ExtractionJobs::new(ExtractionJobOptions::default()).unwrap()
}
fn prepared(jobs: &ExtractionJobs) -> ExtractionTicket {
    wait(|| {
        jobs.poll().and_then(|p| {
            assert_ne!(
                p.state,
                ExtractionJobState::Terminal,
                "preparation failed: {:?}",
                p.outcome
            );
            p.ticket
        })
    })
}
fn terminal(jobs: &ExtractionJobs) -> ExtractionJobSnapshot {
    wait(|| {
        jobs.poll()
            .filter(|s| s.state == ExtractionJobState::Terminal)
    })
}
fn items(page: &BrowserPage, items: Vec<ExtractionItem>) -> ExtractionSelection {
    ExtractionSelection::Items {
        generation: page.generation,
        items,
    }
}
fn failure(snapshot: &ExtractionJobSnapshot, expected: ErrorKind) {
    let Some(ExtractionOutcome::Failed(error)) = &snapshot.outcome else {
        panic!("not failure: {snapshot:?}")
    };
    assert_eq!(error.kind, expected);
    assert!(error.published.is_none());
    assert!(error.residue.is_none());
    assert!(error.message.len() <= 1024);
}
fn committed(snapshot: &ExtractionJobSnapshot, count: usize, bytes: u64) {
    match &snapshot.outcome {
        Some(ExtractionOutcome::Committed {
            entries,
            bytes: actual,
            ..
        }) => assert_eq!((*entries, *actual), (count, bytes)),
        other => panic!("not committed: {other:?}"),
    }
    assert_eq!(snapshot.verified_records, count);
    assert_eq!(snapshot.decoded_bytes, bytes);
}

#[test]
fn exact_directory_record_differs_from_recursive_logical_folder_and_overlap_deduplicates() {
    let f = Fixture::new();
    let path = f.put(
        "a.zip",
        &stored(&[
            Record::file("dir/", b""),
            Record::file("dir/a", b"alpha"),
            Record::file("dir/deep/b", b"beta"),
        ]),
    );
    let (browser, page) = browser(path);
    let id = page.nodes[0].record_id.unwrap();
    let jobs = jobs();
    jobs.prepare(&browser, items(&page, vec![ExtractionItem::Record(id)]))
        .unwrap();
    jobs.start(prepared(&jobs), f.path("record"), None).unwrap();
    let done = terminal(&jobs);
    committed(&done, 1, 0);
    assert!(f.path("record/dir").is_dir());
    assert!(!f.path("record/dir/a").exists());
    jobs.acknowledge(done.id).unwrap();
    wait(|| {
        match jobs.prepare(
            &browser,
            items(
                &page,
                vec![
                    ExtractionItem::Folder(page.nodes[0].navigation.clone().unwrap()),
                    ExtractionItem::Record(id),
                ],
            ),
        ) {
            Ok(id) => Some(id),
            Err(ExtractionSubmitError::Busy) => None,
            Err(e) => panic!("{e:?}"),
        }
    });
    jobs.start(prepared(&jobs), f.path("folder"), None).unwrap();
    committed(&terminal(&jobs), 3, 9);
    assert_eq!(fs::read(f.path("folder/dir/a")).unwrap(), b"alpha");
    assert_eq!(fs::read(f.path("folder/dir/deep/b")).unwrap(), b"beta");
    f.no_staging();
}

#[test]
fn folder_expansion_includes_members_beyond_the_first_browser_page_and_native_source_is_preserved()
{
    let f = Fixture::new();
    let records: Vec<_> = (0..530)
        .map(|i| Record::file(&format!("implicit/{i:04}"), b"x"))
        .collect();
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        let path =
            f.0.join(std::ffi::OsString::from_vec(b"source-\xff.zip".to_vec()));
        fs::write(&path, stored(&records)).unwrap();
        path
    };
    #[cfg(not(unix))]
    let path = f.put("source.zip", &stored(&records));
    let (browser, page) = browser(path.clone());
    assert!(page.nodes[0].synthetic);
    let jobs = jobs();
    jobs.prepare(
        &browser,
        items(
            &page,
            vec![ExtractionItem::Folder(
                page.nodes[0].navigation.clone().unwrap(),
            )],
        ),
    )
    .unwrap();
    let ticket = prepared(&jobs);
    browser.cancel();
    drop(browser);
    jobs.start(ticket, f.path("out"), None).unwrap();
    let done = terminal(&jobs);
    assert_eq!(done.source.as_ref(), &path);
    committed(&done, 530, 530);
    assert_eq!(fs::read_dir(f.path("out/implicit")).unwrap().count(), 530);
    assert_eq!(fs::read(f.path("out/implicit/0529")).unwrap(), b"x");
    f.no_staging();
}

#[test]
fn duplicate_unsafe_and_unsupported_selections_fail_before_staging() {
    let f = Fixture::new();
    for (ordinal, records, kind) in [
        (
            0,
            vec![Record::file("same", b"a"), Record::file("same", b"b")],
            ErrorKind::AmbiguousEntry,
        ),
        (
            1,
            vec![Record::file("safe", b"a"), Record::file("../bad", b"b")],
            ErrorKind::UnsafePath,
        ),
        (
            2,
            {
                let mut r = Record::file("codec", b"a");
                r.method = 93;
                vec![r]
            },
            ErrorKind::Unsupported,
        ),
    ] {
        let path = f.put(&format!("bad-{ordinal}.zip"), &stored(&records));
        let (browser, page) = browser(path);
        assert_eq!(page.catalog.state, BrowserCatalogState::Complete);
        let jobs = jobs();
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        failure(&terminal(&jobs), kind);
        f.no_staging();
    }
}

#[test]
fn incomplete_scan_and_index_limit_reject_preparation_admission() {
    let f = Fixture::new();
    let path = f.put("a.zip", &stored(&[Record::file("deep/a", b"x")]));
    for index_limit in [false, true] {
        let mut options = BrowserOptions::new(path.clone());
        if index_limit {
            options.index_bytes = 512;
        } else {
            options.limits.entries = 0;
        }
        let browser = ArchiveBrowser::open(options).unwrap();
        let page = wait(|| {
            browser
                .poll()
                .filter(|p| p.catalog.state != BrowserCatalogState::Opening)
        });
        assert_eq!(page.catalog.state, BrowserCatalogState::LimitReached);
        assert_eq!(
            jobs().prepare(&browser, ExtractionSelection::All),
            Err(ExtractionSubmitError::NotReady)
        );
        f.no_staging();
    }
}

#[test]
fn stale_browser_generation_and_foreign_record_ids_cannot_rebind() {
    let f = Fixture::new();
    let path = f.put("a.zip", &stored(&[Record::file("a", b"x")]));
    let (first, old) = browser(path.clone());
    let (second, current) = browser(path);
    for selection in [
        items(
            &old,
            vec![ExtractionItem::Record(old.nodes[0].record_id.unwrap())],
        ),
        items(
            &current,
            vec![ExtractionItem::Record(old.nodes[0].record_id.unwrap())],
        ),
    ] {
        let jobs = jobs();
        jobs.prepare(&second, selection).unwrap();
        failure(&terminal(&jobs), ErrorKind::StaleEntry);
    }
    drop(first);
    f.no_staging();
}

#[test]
fn replacement_before_prepare_and_after_ticket_never_reads_replacement() {
    let f = Fixture::new();
    for before_prepare in [true, false] {
        let bytes = stored(&[Record::file("a", b"original")]);
        let path = f.put(
            if before_prepare {
                "before.zip"
            } else {
                "after.zip"
            },
            &bytes,
        );
        let (browser, _) = browser(path.clone());
        let jobs = jobs();
        if before_prepare {
            fs::rename(f.put("replacement", &bytes), &path).unwrap();
        }
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        if !before_prepare {
            let ticket = prepared(&jobs);
            fs::rename(f.put("replacement", &bytes), &path).unwrap();
            jobs.start(ticket, f.path("out"), None).unwrap();
        }
        failure(&terminal(&jobs), ErrorKind::SourceChanged);
        assert!(!f.path("out").exists());
        f.no_staging();
    }
}

#[test]
fn conflict_crc_failure_and_cancel_are_truthful_and_leave_destination_absent() {
    let f = Fixture::new();
    let good = stored(&[Record::file("a", b"payload")]);
    for mode in 0..3 {
        let mut bytes = good.clone();
        if mode == 1 {
            bytes[31] ^= 1;
        }
        let (browser, _) = browser(f.put(&format!("{mode}.zip"), &bytes));
        let jobs = jobs();
        let id = jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        let ticket = prepared(&jobs);
        let destination = f.path(&format!("out-{mode}"));
        if mode == 0 {
            fs::create_dir(&destination).unwrap();
            fs::write(destination.join("sentinel"), b"keep").unwrap();
        }
        if mode == 2 {
            jobs.cancel(id).unwrap();
        }
        if mode == 2 {
            // Either the worker has observed cancellation already or start is
            // queued before it does; both paths must terminate without output.
            let _ = jobs.start(ticket, destination.clone(), None);
        } else {
            jobs.start(ticket, destination.clone(), None).unwrap();
        }
        let done = terminal(&jobs);
        failure(
            &done,
            [
                ErrorKind::Conflict,
                ErrorKind::Integrity,
                ErrorKind::Cancelled,
            ][mode],
        );
        if mode == 0 {
            assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"keep");
        } else {
            assert!(!destination.exists());
        }
        f.no_staging();
    }
}

#[test]
fn one_reservation_busy_until_terminal_ack_and_ticket_is_service_bound() {
    let f = Fixture::new();
    let (browser, _) = browser(f.put("a.zip", &stored(&[Record::file("a", b"x")])));
    let jobs = jobs();
    let other = self::jobs();
    let id = jobs.prepare(&browser, ExtractionSelection::All).unwrap();
    assert_eq!(
        jobs.prepare(&browser, ExtractionSelection::All),
        Err(ExtractionSubmitError::Busy)
    );
    assert_eq!(
        jobs.acknowledge(id),
        Err(ExtractionSubmitError::NotTerminal)
    );
    let error = other
        .start(prepared(&jobs), f.path("out"), None)
        .unwrap_err();
    assert_eq!(error.kind, ExtractionSubmitError::StaleJob);
    jobs.start(error.ticket, f.path("out"), error.credential)
        .unwrap();
    committed(&terminal(&jobs), 1, 1);
    jobs.cancel(id).unwrap();
    assert_eq!(
        jobs.prepare(&browser, ExtractionSelection::All),
        Err(ExtractionSubmitError::Busy)
    );
    jobs.acknowledge(id).unwrap();
    let next = wait(|| match jobs.prepare(&browser, ExtractionSelection::All) {
        Ok(id) => Some(id),
        Err(ExtractionSubmitError::Busy) => None,
        Err(e) => panic!("{e}"),
    });
    assert_ne!(next, id);
    drop(prepared(&jobs));
    failure(&terminal(&jobs), ErrorKind::Cancelled);
}

#[test]
fn empty_all_and_empty_explicit_folder_publish_empty_directories() {
    let f = Fixture::new();
    for (n, records) in [vec![], vec![Record::file("empty/", b"")]]
        .into_iter()
        .enumerate()
    {
        let (browser, _) = browser(f.put(&format!("{n}.zip"), &stored(&records)));
        let jobs = jobs();
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        jobs.start(prepared(&jobs), f.path(&format!("out-{n}")), None)
            .unwrap();
        committed(&terminal(&jobs), records.len(), 0);
        assert!(f.path(&format!("out-{n}")).is_dir());
    }
}

#[test]
fn manifest_and_payload_budgets_fail_explicitly_and_credentials_are_bounded_redacted() {
    let f = Fixture::new();
    let (browser, _) = browser(f.put(
        "a.zip",
        &stored(&[Record::file("a", b"123456"), Record::file("b", b"x")]),
    ));
    for (count, bytes) in [(1, 32 << 20), (1_000_000, 127)] {
        let options = ExtractionJobOptions {
            manifest_records: count,
            manifest_bytes: bytes,
            ..ExtractionJobOptions::default()
        };
        let jobs = ExtractionJobs::new(options).unwrap();
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        failure(&terminal(&jobs), ErrorKind::LimitExceeded);
    }
    let mut options = ExtractionJobOptions::default();
    options.limits.total_bytes = 5;
    let jobs = ExtractionJobs::new(options).unwrap();
    jobs.prepare(&browser, ExtractionSelection::All).unwrap();
    failure(&terminal(&jobs), ErrorKind::LimitExceeded);
    assert!(ExtractionCredential::new(&vec![0; 1025]).is_err());
    assert_eq!(
        format!("{:?}", ExtractionCredential::new(b"secret").unwrap()),
        "ExtractionCredential([REDACTED])"
    );
    f.no_staging();
}

#[test]
fn encrypted_empty_directory_credentials_authenticate_before_job_publication() {
    use std::io::Cursor;
    let f = Fixture::new();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "locked/",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .with_aes_encryption_bytes(zip::AesMode::Aes256, b"secret"),
        )
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let (browser, _) = browser(f.put("encrypted.zip", &bytes));
    for (name, credential, expected) in [
        ("missing", None, Some(ErrorKind::CredentialRequired)),
        (
            "wrong",
            Some(ExtractionCredential::new(b"wrong").unwrap()),
            Some(ErrorKind::Integrity),
        ),
        (
            "right",
            Some(ExtractionCredential::new(b"secret").unwrap()),
            None,
        ),
    ] {
        let jobs = jobs();
        jobs.prepare(&browser, ExtractionSelection::All).unwrap();
        jobs.start(prepared(&jobs), f.path(name), credential)
            .unwrap();
        let done = terminal(&jobs);
        if let Some(kind) = expected {
            failure(&done, kind);
            assert!(!f.path(name).exists());
        } else {
            committed(&done, 1, 0);
            assert!(f.path("right/locked").is_dir());
        }
        f.no_staging();
    }
}

#[test]
fn missing_folders_empty_selection_and_input_admission_never_stage() {
    let f = Fixture::new();
    let (browser, page) = browser(f.put("a.zip", &stored(&[Record::file("a", b"x")])));
    let jobs = jobs();
    assert_eq!(
        jobs.prepare(&browser, items(&page, vec![])),
        Err(ExtractionSubmitError::InvalidSelection)
    );
    assert_eq!(
        jobs.prepare(
            &browser,
            items(
                &page,
                vec![ExtractionItem::Record(page.nodes[0].record_id.unwrap()); 513]
            )
        ),
        Err(ExtractionSubmitError::LimitExceeded)
    );
    jobs.prepare(
        &browser,
        items(
            &page,
            vec![ExtractionItem::Folder(
                ArchivePrefix::from_components(vec!["absent".into()]).unwrap(),
            )],
        ),
    )
    .unwrap();
    failure(&terminal(&jobs), ErrorKind::UnsafePath);
    f.no_staging();
}

#[test]
fn foreign_and_recreated_service_ids_and_tickets_cannot_start_cancel_or_acknowledge() {
    let f = Fixture::new();
    let (browser, _) = browser(f.put("a.zip", &stored(&[Record::file("a", b"x")])));
    let first = jobs();
    let second = jobs();
    let old_id = first.prepare(&browser, ExtractionSelection::All).unwrap();
    let old_ticket = prepared(&first);
    let current_id = second.prepare(&browser, ExtractionSelection::All).unwrap();
    let current_ticket = prepared(&second);
    assert_eq!(old_id.sequence(), current_id.sequence());
    assert_ne!(old_id, current_id);
    assert_eq!(second.cancel(old_id), Err(ExtractionSubmitError::StaleJob));
    assert_eq!(
        second.acknowledge(old_id),
        Err(ExtractionSubmitError::StaleJob)
    );
    let returned = second
        .start(old_ticket, f.path("foreign"), None)
        .unwrap_err();
    assert_eq!(returned.kind, ExtractionSubmitError::StaleJob);
    drop(first);
    let recreated = jobs();
    let recreated_id = recreated
        .prepare(&browser, ExtractionSelection::All)
        .unwrap();
    let recreated_ticket = prepared(&recreated);
    assert_eq!(old_id.sequence(), recreated_id.sequence());
    assert_ne!(old_id, recreated_id);
    assert_eq!(
        recreated.cancel(old_id),
        Err(ExtractionSubmitError::StaleJob)
    );
    assert_eq!(
        recreated.acknowledge(old_id),
        Err(ExtractionSubmitError::StaleJob)
    );
    let stale = recreated
        .start(returned.ticket, f.path("old-ticket"), returned.credential)
        .unwrap_err();
    assert_eq!(stale.kind, ExtractionSubmitError::StaleJob);
    drop(stale);
    second
        .start(current_ticket, f.path("second"), None)
        .unwrap();
    recreated
        .start(recreated_ticket, f.path("recreated"), None)
        .unwrap();
    committed(&terminal(&second), 1, 1);
    committed(&terminal(&recreated), 1, 1);
    assert!(!f.path("foreign").exists() && !f.path("old-ticket").exists());
    assert_eq!(
        second.acknowledge(old_id),
        Err(ExtractionSubmitError::StaleJob)
    );
    assert_eq!(
        recreated.acknowledge(old_id),
        Err(ExtractionSubmitError::StaleJob)
    );
    f.no_staging();
}

#[test]
fn preparation_and_execution_deadlines_fail_without_publication_and_start_rejection_returns_ownership()
 {
    let f = Fixture::new();
    let (browser, _) = browser(f.put("a.zip", &stored(&[Record::file("a", b"x")])));
    let options = ExtractionJobOptions {
        preparation_timeout: Duration::ZERO,
        ..ExtractionJobOptions::default()
    };
    let timed = ExtractionJobs::new(options).unwrap();
    timed.prepare(&browser, ExtractionSelection::All).unwrap();
    failure(&terminal(&timed), ErrorKind::Deadline);
    let options = ExtractionJobOptions {
        execution_timeout: Duration::ZERO,
        ..ExtractionJobOptions::default()
    };
    let timed = ExtractionJobs::new(options).unwrap();
    timed.prepare(&browser, ExtractionSelection::All).unwrap();
    let rejected = timed
        .start(
            prepared(&timed),
            PathBuf::from("relative"),
            Some(ExtractionCredential::new(b"secret").unwrap()),
        )
        .unwrap_err();
    assert_eq!(rejected.kind, ExtractionSubmitError::InvalidDestination);
    assert!(rejected.credential.is_some());
    timed
        .start(rejected.ticket, f.path("out"), rejected.credential)
        .unwrap();
    failure(&terminal(&timed), ErrorKind::Deadline);
    assert!(!f.path("out").exists());
    f.no_staging();
}
