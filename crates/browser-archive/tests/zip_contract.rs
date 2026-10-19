use browser_archive::*;
use std::{
    fs,
    io::{self, Cursor, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".test-work")
            .join(format!(
                "{}-{}",
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
fn control(cancel: &Cancellation) -> Control<'_> {
    Control::new(cancel, Duration::from_secs(30), 128 << 20)
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

#[test]
fn list_then_read_exact_record_without_unpacking() {
    let f = Fixture::new();
    let path = f.put(
        "a.zip",
        &stored(&[
            Record::file("dir/a", b"alpha"),
            Record::file("dir/b", b"beta"),
        ]),
    );
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(session.entries().len(), 2);
    assert_eq!(ctl.consumed_bytes(), 0);
    assert_ne!(session.entries()[0].id, session.entries()[1].id);
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    let mut out = Vec::new();
    session
        .read_entry(session.entries()[1].id, &mut out, None, &ctl)
        .unwrap();
    assert_eq!(out, b"beta");
}

#[test]
fn duplicates_remain_distinct_and_are_never_last_wins() {
    let f = Fixture::new();
    let path = f.put(
        "a.zip",
        &stored(&[
            Record::file("same", b"first"),
            Record::file("same", b"last"),
        ]),
    );
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(session.entries().len(), 2);
    assert_ne!(session.entries()[0].id, session.entries()[1].id);
    assert!(session.entries().iter().all(|entry| entry.issue.is_some()));
    let error = session
        .read_entry(session.entries()[0].id, &mut Vec::new(), None, &ctl)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::AmbiguousEntry);
}

#[test]
fn stale_session_ids_and_replaced_sources_fail() {
    let f = Fixture::new();
    let bytes = stored(&[Record::file("a", b"one")]);
    let path = f.put("a.zip", &bytes);
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let first = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    let second = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(
        second.entry(first.entries()[0].id).unwrap_err().kind,
        ErrorKind::StaleEntry
    );
    fs::rename(f.put("replacement", &bytes), &path).unwrap();
    assert_eq!(
        first
            .read_entry(first.entries()[0].id, &mut Vec::new(), None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::SourceChanged
    );
}

#[test]
fn partial_catalog_has_no_readable_session() {
    let f = Fixture::new();
    let path = f.put(
        "a.zip",
        &stored(&[Record::file("a", b"a"), Record::file("b", b"b")]),
    );
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let limits = Limits {
        metadata_bytes: 2200,
        ..Limits::default()
    };
    let (session, catalog) = ArchiveSession::scan(&path, limits, &ctl).unwrap();
    assert!(session.is_none());
    assert_eq!(catalog.entries.len(), 1);
    assert!(matches!(catalog.state, CatalogState::LimitReached));
}

#[test]
fn declared_counts_and_directory_bytes_are_bounded() {
    let f = Fixture::new();
    let bytes = stored(&[Record::file("a", b"a")]);
    let path = f.put("a.zip", &bytes);
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    assert!(
        ArchiveSession::open(
            &path,
            Limits {
                directory_bytes: 1,
                ..Limits::default()
            },
            &ctl
        )
        .is_err()
    );
    let mut corrupt = bytes;
    let n = corrupt.len();
    corrupt[n - 14..n - 12].copy_from_slice(&2u16.to_le_bytes());
    corrupt[n - 12..n - 10].copy_from_slice(&2u16.to_le_bytes());
    let path = f.put("bad-count.zip", &corrupt);
    assert!(ArchiveSession::open(&path, Limits::default(), &ctl).is_err());
}

#[test]
fn traversal_drive_unc_nul_and_reserved_names_are_disabled() {
    for name in [
        "../escape",
        "x/../../escape",
        "/absolute",
        "C:/x",
        "C:x",
        "\\\\server\\x",
        "x\\..\\outside",
        "a\0b",
        "NUL",
        "a:stream",
        "trailing.",
        "a//b",
        "./x",
    ] {
        let f = Fixture::new();
        let path = f.put("bad.zip", &stored(&[Record::file(name, b"bad")]));
        let cancel = Cancellation::default();
        let ctl = control(&cancel);
        let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
        assert!(session.entries()[0].issue.is_some(), "{name:?}");
        assert!(
            extract_new(
                &session,
                &[session.entries()[0].id],
                &f.path("out"),
                None,
                &ctl
            )
            .is_err()
        );
        assert!(!f.path("out").exists());
        f.no_staging();
    }
}

#[test]
fn cp437_and_strict_unicode_names_do_not_lose_identity() {
    let f = Fixture::new();
    let mut cp = Record::file("a", b"ok");
    cp.name = vec![0x82, b'.', b't', b'x', b't'];
    let mut utf = Record::file("שלום-🦀", b"unicode");
    utf.flags = 1 << 11;
    let mut invalid = Record::file("bad", b"bad");
    invalid.name = vec![0xff];
    invalid.flags = 1 << 11;
    let path = f.put("names.zip", &stored(&[cp, utf, invalid]));
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(session.entries()[0].name, "é.txt");
    assert_eq!(session.entries()[0].raw_name[0], 0x82);
    assert_eq!(session.entries()[1].name, "שלום-🦀");
    assert!(session.entries()[2].issue.is_some());
}

#[test]
fn symlinks_and_case_prefix_collisions_do_not_publish() {
    let cases = [
        vec![{
            let mut r = Record::file("link", b"/etc/passwd");
            r.mode = 0o120777;
            r
        }],
        vec![Record::file("A", b"a"), Record::file("a", b"b")],
        vec![
            Record::file("a", b"file"),
            Record::file("a/child", b"child"),
        ],
    ];
    for records in cases {
        let f = Fixture::new();
        let path = f.put("bad.zip", &stored(&records));
        let cancel = Cancellation::default();
        let ctl = control(&cancel);
        let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
        let ids: Vec<_> = session.entries().iter().map(|e| e.id).collect();
        assert!(extract_new(&session, &ids, &f.path("out"), None, &ctl).is_err());
        assert!(!f.path("out").exists());
        f.no_staging();
    }
}

#[test]
fn crc_failure_never_publishes_partial_output() {
    let f = Fixture::new();
    let mut bytes = stored(&[Record::file("a", b"payload")]);
    bytes[31] ^= 1;
    let path = f.put("crc.zip", &bytes);
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(
        extract_new(
            &session,
            &[session.entries()[0].id],
            &f.path("out"),
            None,
            &ctl
        )
        .unwrap_err()
        .kind,
        ErrorKind::Integrity
    );
    assert!(!f.path("out").exists());
    f.no_staging();
}

#[test]
fn actual_decoded_bytes_and_retry_budget_are_bounded() {
    let f = Fixture::new();
    let path = f.put("a.zip", &stored(&[Record::file("a", b"0123456789")]));
    let cancel = Cancellation::default();
    let ctl = Control::new(&cancel, Duration::from_secs(30), 15);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    session
        .read_entry(session.entries()[0].id, &mut Vec::new(), None, &ctl)
        .unwrap();
    assert_eq!(
        session
            .read_entry(session.entries()[0].id, &mut Vec::new(), None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    let ctl = control(&cancel);
    let small = ArchiveSession::open(
        &path,
        Limits {
            member_bytes: 5,
            ..Limits::default()
        },
        &ctl,
    )
    .unwrap();
    assert_eq!(
        small
            .read_entry(small.entries()[0].id, &mut Vec::new(), None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
}

struct FullDisk;
impl Write for FullDisk {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(28))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn output_io_failure_is_not_integrity_success() {
    let f = Fixture::new();
    let path = f.put("a.zip", &stored(&[Record::file("a", b"payload")]));
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    let error = session
        .read_entry(session.entries()[0].id, &mut FullDisk, None, &ctl)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Io);
    assert_eq!(error.io.unwrap().raw_os_error(), Some(28));
}

#[test]
fn nested_archives_are_opaque_and_browsing_does_not_inflate() {
    let f = Fixture::new();
    let inner = stored(&[Record::file("secret", b"inner")]);
    let path = f.put(
        "outer.zip",
        &stored(&[
            Record::file("inner.zip", &inner),
            Record::file("sibling", b"other"),
        ]),
    );
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let outer = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(ctl.consumed_bytes(), 0);
    let mut materialized = Vec::new();
    outer
        .read_entry(outer.entries()[0].id, &mut materialized, None, &ctl)
        .unwrap();
    assert_eq!(materialized, inner);
    assert_eq!(outer.entries().len(), 2);
    assert_eq!(ctl.consumed_bytes(), inner.len() as u64);
}

#[test]
fn unsupported_method_stays_browsable() {
    let f = Fixture::new();
    let mut record = Record::file("a", b"payload");
    record.method = 12;
    let path = f.put("unsupported.zip", &stored(&[record]));
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(session.entries()[0].compression, 12);
    assert_eq!(
        session
            .read_entry(session.entries()[0].id, &mut Vec::new(), None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
}

#[test]
fn cancellation_before_work_creates_nothing() {
    let f = Fixture::new();
    let path = f.put("a.zip", &stored(&[Record::file("a", b"a")]));
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    cancel.cancel();
    assert_eq!(
        extract_new(
            &session,
            &[session.entries()[0].id],
            &f.path("out"),
            None,
            &ctl
        )
        .unwrap_err()
        .kind,
        ErrorKind::Cancelled
    );
    f.no_staging();
}

#[cfg(target_os = "linux")]
#[test]
fn extraction_and_creation_round_trip_unicode_and_directories() {
    let f = Fixture::new();
    let source = f.put("source", b"source payload");
    let empty = f.put("empty", b"");
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let sources = [
        CreateSource::directory("folder"),
        CreateSource::file(&source, "folder/🦀.txt"),
        CreateSource::file(&empty, "empty"),
    ];
    let zip = f.path("created.zip");
    let commit = create_new(
        &sources,
        &zip,
        &CreateOptions::default(),
        None,
        Limits::default(),
        &ctl,
    )
    .unwrap();
    assert_eq!(commit.entries, 3);
    let session = ArchiveSession::open(&zip, Limits::default(), &ctl).unwrap();
    let ids: Vec<_> = session.entries().iter().map(|e| e.id).collect();
    extract_new(&session, &ids, &f.path("out"), None, &ctl).unwrap();
    assert_eq!(
        fs::read(f.path("out/folder/🦀.txt")).unwrap(),
        b"source payload"
    );
    assert!(f.path("out/empty").exists());
    f.no_staging();
}

#[cfg(target_os = "linux")]
#[test]
fn aes256_creation_password_failure_and_authentication() {
    let f = Fixture::new();
    let source = f.put("source", b"encrypted payload");
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let path = f.path("aes.zip");
    create_new(
        &[CreateSource::file(source, "a")],
        &path,
        &CreateOptions {
            aes256: true,
            ..CreateOptions::default()
        },
        Some("סוד🦀".as_bytes()),
        Limits::default(),
        &ctl,
    )
    .unwrap();
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    let id = session.entries()[0].id;
    assert_eq!(
        session
            .read_entry(id, &mut Vec::new(), None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::CredentialRequired
    );
    assert!(
        session
            .read_entry(id, &mut Vec::new(), Some(b"wrong"), &ctl)
            .is_err()
    );
    let mut output = Vec::new();
    session
        .read_entry(id, &mut output, Some("סוד🦀".as_bytes()), &ctl)
        .unwrap();
    assert_eq!(output, b"encrypted payload");
}

#[cfg(target_os = "linux")]
#[test]
fn existing_destination_is_never_replaced() {
    let f = Fixture::new();
    let source = f.put("source", b"data");
    let destination = f.put("existing.zip", b"keep");
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    assert_eq!(
        create_new(
            &[CreateSource::file(source, "a")],
            &destination,
            &CreateOptions::default(),
            None,
            Limits::default(),
            &ctl
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fs::read(destination).unwrap(), b"keep");
    f.no_staging();
}

#[test]
fn deflate_bomb_is_rejected_with_small_policy_not_giant_allocation() {
    let f = Fixture::new();
    let mut bytes = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut bytes);
    writer
        .start_file(
            "zeros",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
    writer.write_all(&vec![0; 2 << 20]).unwrap();
    writer.finish().unwrap();
    let path = f.put("bomb.zip", &bytes.into_inner());
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert_eq!(
        session
            .read_entry(session.entries()[0].id, &mut Vec::new(), None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
}

#[test]
fn zip64_for_small_payload_can_be_read_without_loading_container() {
    let f = Fixture::new();
    let mut bytes = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut bytes);
    writer
        .start_file(
            "a",
            zip::write::SimpleFileOptions::default()
                .large_file(true)
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    writer.write_all(b"zip64").unwrap();
    writer.finish().unwrap();
    let path = f.put("zip64.zip", &bytes.into_inner());
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    let mut output = Vec::new();
    session
        .read_entry(session.entries()[0].id, &mut output, None, &ctl)
        .unwrap();
    assert_eq!(output, b"zip64");
}

#[test]
fn understated_count_and_overlapping_local_headers_are_rejected() {
    let f = Fixture::new();
    let original = stored(&[Record::file("a", b"alpha"), Record::file("b", b"beta")]);
    let mut bytes = original.clone();
    let n = bytes.len();
    bytes[n - 14..n - 12].copy_from_slice(&1u16.to_le_bytes());
    bytes[n - 12..n - 10].copy_from_slice(&1u16.to_le_bytes());
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let path = f.put("count.zip", &bytes);
    assert!(ArchiveSession::open(&path, Limits::default(), &ctl).is_err());
    let mut bytes = original;
    let central: Vec<_> = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, bytes)| *bytes == [0x50, 0x4b, 0x01, 0x02])
        .map(|(offset, _)| offset)
        .collect();
    bytes[central[1] + 42..central[1] + 46].copy_from_slice(&0u32.to_le_bytes());
    let path = f.put("overlap.zip", &bytes);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert!(
        session
            .read_entry(session.entries()[1].id, &mut Vec::new(), None, &ctl)
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn aes_authentication_tag_corruption_never_publishes() {
    let f = Fixture::new();
    let source = f.put("source", b"authenticated payload");
    let path = f.path("aes.zip");
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    create_new(
        &[CreateSource::file(source, "a")],
        &path,
        &CreateOptions {
            aes256: true,
            ..CreateOptions::default()
        },
        Some(b"secret"),
        Limits::default(),
        &ctl,
    )
    .unwrap();
    let mut bytes = fs::read(&path).unwrap();
    let mut backend = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
    let member = backend.by_index_raw(0).unwrap();
    let tag_end = member.data_start().unwrap() + member.compressed_size();
    drop(member);
    drop(backend);
    bytes[tag_end as usize - 1] ^= 1;
    fs::write(&path, bytes).unwrap();
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    assert!(
        extract_new(
            &session,
            &[session.entries()[0].id],
            &f.path("out"),
            Some(b"secret"),
            &ctl
        )
        .is_err()
    );
    assert!(!f.path("out").exists());
    f.no_staging();
}

#[cfg(target_os = "linux")]
#[test]
fn native_non_utf8_source_identity_uses_explicit_archive_name() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let f = Fixture::new();
    let path = f.0.join(OsString::from_vec(vec![b'n', 0xff]));
    fs::write(&path, b"native identity").unwrap();
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let output = f.path("archive.zip");
    create_new(
        &[CreateSource::file(path, "mapped-name")],
        &output,
        &CreateOptions::default(),
        None,
        Limits::default(),
        &ctl,
    )
    .unwrap();
    let session = ArchiveSession::open(&output, Limits::default(), &ctl).unwrap();
    let mut payload = Vec::new();
    session
        .read_entry(session.entries()[0].id, &mut payload, None, &ctl)
        .unwrap();
    assert_eq!(payload, b"native identity");
}

#[cfg(target_os = "linux")]
#[test]
fn created_utf8_deflate_zip_is_readable_by_python_stdlib() {
    let f = Fixture::new();
    let source = f.put("source", b"interoperable");
    let output = f.path("archive.zip");
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    create_new(
        &[CreateSource::file(source, "🦀.txt")],
        &output,
        &CreateOptions::default(),
        None,
        Limits::default(),
        &ctl,
    )
    .unwrap();
    // Independent, preinstalled test reader; the production engine never starts
    // helper processes. Missing Python is a test failure, never a silent pass.
    let status = std::process::Command::new("python3").arg("-c").arg("import sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); assert z.namelist()==['🦀.txt']; assert z.read('🦀.txt')==b'interoperable'; assert z.testzip() is None").arg(output).status().unwrap();
    assert!(status.success());
}

#[cfg(target_os = "linux")]
#[test]
fn deep_path_indexes_are_bounded_before_staging() {
    let f = Fixture::new();
    let source = f.put("source", b"data");
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let name = vec!["component"; 100].join("/");
    let error = create_new(
        &[CreateSource::file(source, name)],
        &f.path("out.zip"),
        &CreateOptions::default(),
        None,
        Limits {
            metadata_bytes: 16 << 10,
            ..Limits::default()
        },
        &ctl,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::LimitExceeded);
    assert!(!f.path("out.zip").exists());
    f.no_staging();
}

#[cfg(target_os = "linux")]
#[test]
fn selected_directory_with_payload_is_rejected_even_with_valid_crc() {
    for corrupt in [false, true] {
        let f = Fixture::new();
        let mut directory = Record::file("dir/", b"seventeen bytes!!");
        directory.mode = 0o040700;
        assert_eq!(directory.bytes.len(), 17);
        let mut bytes = stored(&[directory]);
        if corrupt {
            bytes[30 + b"dir/".len()] ^= 1;
        }
        let path = f.put("payload-directory.zip", &bytes);
        let cancel = Cancellation::default();
        let ctl = control(&cancel);
        let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
        assert_eq!(session.entries()[0].kind, EntryKind::Directory);
        let error = extract_new(
            &session,
            &[session.entries()[0].id],
            &f.path("out"),
            None,
            &ctl,
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Malformed);
        assert!(!f.path("out").exists());
        f.no_staging();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn selected_empty_directory_must_validate_crc_and_codec() {
    for unsupported in [false, true] {
        let f = Fixture::new();
        let mut directory = Record::file("dir/", b"");
        directory.mode = 0o040700;
        if unsupported {
            directory.method = 12;
        }
        let mut bytes = stored(&[directory]);
        if !unsupported {
            // Both headers agree, but an empty stream has CRC zero, not one.
            let central = bytes
                .windows(4)
                .position(|bytes| bytes == [0x50, 0x4b, 0x01, 0x02])
                .unwrap();
            bytes[14..18].copy_from_slice(&1u32.to_le_bytes());
            bytes[central + 16..central + 20].copy_from_slice(&1u32.to_le_bytes());
        }
        let path = f.put("empty-directory.zip", &bytes);
        let cancel = Cancellation::default();
        let ctl = control(&cancel);
        let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
        let error = extract_new(
            &session,
            &[session.entries()[0].id],
            &f.path("out"),
            None,
            &ctl,
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            if unsupported {
                ErrorKind::Unsupported
            } else {
                ErrorKind::Integrity
            }
        );
        assert!(!f.path("out").exists());
        f.no_staging();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn selected_encrypted_empty_directory_authenticates_before_publication() {
    let f = Fixture::new();
    let mut bytes = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut bytes);
    writer
        .start_file(
            "locked/",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .with_aes_encryption_bytes(zip::AesMode::Aes256, b"secret"),
        )
        .unwrap();
    writer.finish().unwrap();
    let mut bytes = bytes.into_inner();
    let path = f.put("encrypted-directory.zip", &bytes);
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&path, Limits::default(), &ctl).unwrap();
    let id = session.entries()[0].id;
    assert_eq!(session.entries()[0].kind, EntryKind::Directory);
    assert_eq!(session.entries()[0].uncompressed_bytes, 0);
    assert!(session.entries()[0].encrypted);
    for (name, password, expected) in [
        ("missing", None, ErrorKind::CredentialRequired),
        ("wrong", Some(b"wrong".as_slice()), ErrorKind::Integrity),
    ] {
        let destination = f.path(name);
        assert_eq!(
            extract_new(&session, &[id], &destination, password, &ctl)
                .unwrap_err()
                .kind,
            expected
        );
        assert!(!destination.exists());
        f.no_staging();
    }
    let committed = extract_new(&session, &[id], &f.path("valid"), Some(b"secret"), &ctl).unwrap();
    assert_eq!(committed.bytes, 0);
    assert_eq!(committed.entries, 1);
    assert!(f.path("valid/locked").is_dir());
    let mut backend = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
    let member = backend.by_index_raw(0).unwrap();
    let tag_end = member.data_start().unwrap() + member.compressed_size();
    drop(member);
    drop(backend);
    bytes[tag_end as usize - 1] ^= 1;
    let corrupt_path = f.put("corrupt-directory.zip", &bytes);
    let corrupt = ArchiveSession::open(&corrupt_path, Limits::default(), &ctl).unwrap();
    assert_eq!(
        extract_new(
            &corrupt,
            &[corrupt.entries()[0].id],
            &f.path("corrupt"),
            Some(b"secret"),
            &ctl
        )
        .unwrap_err()
        .kind,
        ErrorKind::Integrity
    );
    assert!(!f.path("corrupt").exists());
    f.no_staging();
}

#[test]
fn public_member_read_cannot_override_session_total_limit_with_larger_control() {
    let f = Fixture::new();
    let path = f.put("limit.zip", &stored(&[Record::file("ten", b"0123456789")]));
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(
        &path,
        Limits {
            total_bytes: 5,
            ..Limits::default()
        },
        &ctl,
    )
    .unwrap();
    let mut output = Vec::new();
    assert_eq!(
        session
            .read_entry(session.entries()[0].id, &mut output, None, &ctl)
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    assert!(output.is_empty());
    assert_eq!(ctl.consumed_bytes(), 0);
    let exact = ArchiveSession::open(
        &path,
        Limits {
            total_bytes: 10,
            ..Limits::default()
        },
        &ctl,
    )
    .unwrap();
    assert_eq!(
        exact
            .read_entry(exact.entries()[0].id, &mut output, None, &ctl)
            .unwrap(),
        10
    );
    assert_eq!(output, b"0123456789");
}
