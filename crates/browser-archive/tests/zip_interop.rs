//! Opt-in compatibility checks against an independent installed 7-Zip.
//! Run with `cargo test -p browser-archive --test zip_interop -- --ignored`.
//! Passwords here are public disposable fixture data, never user credentials.
#![cfg(target_os = "linux")]

use browser_archive::*;
use std::{
    ffi::OsString,
    fs::{self, File},
    io::Cursor,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const PASSWORD: &str = "megaman-owned-fixture-password";
const UNICODE_NAME: &str = "folder/unicodé_日本語.txt";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".test-work")
            .join(format!(
                "interop-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn payloads(&self) -> Vec<(&'static str, Vec<u8>)> {
        let mut state = 0x9e37_79b9_u32;
        let binary = (0..65_537)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        vec![
            (UNICODE_NAME, "Unicode payload: 🦀\n".as_bytes().to_vec()),
            ("empty", Vec::new()),
            ("binary.dat", binary),
        ]
    }

    fn write_inputs(&self) {
        fs::create_dir_all(self.path("input/folder")).unwrap();
        fs::create_dir_all(self.path("input/empty-directory")).unwrap();
        for (name, bytes) in self.payloads() {
            fs::write(self.path("input").join(name), bytes).unwrap();
        }
        fs::write(self.path("outside-sentinel"), b"must survive").unwrap();
    }

    // File-backed output avoids pipe deadlocks; deadline kills and reaps the
    // owned child. No shell, inherited stdin, or password prompt is involved.
    fn seven_zip(&self, args: &[OsString], succeeds: bool) {
        let stdout = File::create(self.path("7z-stdout")).unwrap();
        let stderr = File::create(self.path("7z-stderr")).unwrap();
        let mut child = Command::new("7z")
            .args(args)
            .current_dir(self.path("input"))
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("this explicit interoperability test requires 7z on PATH");
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned 7z fixture exceeded 20-second deadline");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            status.success(),
            succeeds,
            "7z {args:?}: {status}\n{}\n{}",
            fs::read_to_string(self.path("7z-stdout")).unwrap(),
            fs::read_to_string(self.path("7z-stderr")).unwrap()
        );
    }

    fn external_archive(&self, method: &str, encryption: &str) -> PathBuf {
        let archive = self.path("external.zip");
        self.seven_zip(
            &[
                "a".into(),
                "-tzip".into(),
                format!("-mm={method}").into(),
                format!("-mem={encryption}").into(),
                format!("-p{PASSWORD}").into(),
                archive.as_os_str().into(),
                "folder".into(),
                "empty".into(),
                "binary.dat".into(),
                "empty-directory".into(),
            ],
            true,
        );
        archive
    }

    fn external_test(&self, archive: &Path, password: &str, succeeds: bool) {
        self.seven_zip(
            &[
                "t".into(),
                format!("-p{password}").into(),
                archive.as_os_str().into(),
            ],
            succeeds,
        );
    }

    fn verify_output(&self, output: &Path) {
        for (name, bytes) in self.payloads() {
            assert_eq!(fs::read(output.join(name)).unwrap(), bytes, "{name}");
        }
        assert!(output.join("empty-directory").is_dir());
        assert_eq!(
            fs::read(self.path("outside-sentinel")).unwrap(),
            b"must survive"
        );
        self.no_staging();
    }

    fn no_staging(&self) {
        assert!(fs::read_dir(&self.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".megaman-archive-")
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

#[test]
#[ignore = "requires installed 7z; run explicitly with --ignored"]
fn reads_and_extracts_seven_zip_aes128_aes192_aes256_and_zipcrypto() {
    for encryption in ["AES128", "AES192", "AES256", "ZipCrypto"] {
        for method in ["Copy", "Deflate"] {
            let f = Fixture::new();
            f.write_inputs();
            let archive = f.external_archive(method, encryption);
            f.external_test(&archive, PASSWORD, true);
            let cancel = Cancellation::default();
            let ctl = control(&cancel);
            let session = ArchiveSession::open(&archive, Limits::default(), &ctl).unwrap();
            for (name, expected) in f.payloads() {
                let entry = session.entries().iter().find(|e| e.name == name).unwrap();
                assert!(entry.encrypted);
                assert!(entry.issue.is_none());
                let mut actual = Vec::new();
                session
                    .read_entry(entry.id, &mut actual, Some(PASSWORD.as_bytes()), &ctl)
                    .unwrap();
                assert_eq!(actual, expected);
            }
            let ids: Vec<_> = session.entries().iter().map(|e| e.id).collect();
            for (name, password, kind) in [
                ("no-password", None, ErrorKind::CredentialRequired),
                (
                    "wrong-password",
                    Some(b"wrong".as_slice()),
                    ErrorKind::Integrity,
                ),
            ] {
                let output = f.path(name);
                let error = extract_new(&session, &ids, &output, password, &ctl).unwrap_err();
                assert_eq!(error.kind, kind);
                assert!(error.published.is_none());
                assert!(error.residue.is_none());
                assert!(!output.exists());
                f.no_staging();
            }
            let output = f.path("output");
            extract_new(&session, &ids, &output, Some(PASSWORD.as_bytes()), &ctl).unwrap();
            f.verify_output(&output);
        }
    }
}

#[test]
#[ignore = "requires installed 7z; run explicitly with --ignored"]
fn seven_zip_reads_and_extracts_created_aes256_stored_and_deflate() {
    for deflate_level in [None, Some(6)] {
        let f = Fixture::new();
        f.write_inputs();
        let mut sources: Vec<_> = f
            .payloads()
            .into_iter()
            .map(|(name, _)| CreateSource::file(f.path("input").join(name), name))
            .collect();
        sources.push(CreateSource::directory("empty-directory"));
        let archive = f.path("created.zip");
        let cancel = Cancellation::default();
        create_new(
            &sources,
            &archive,
            &CreateOptions {
                deflate_level,
                aes256: true,
            },
            Some(PASSWORD.as_bytes()),
            Limits::default(),
            &control(&cancel),
        )
        .unwrap();
        f.external_test(&archive, PASSWORD, true);
        f.external_test(&archive, "wrong", false);
        let output = f.path("output");
        let mut output_arg = OsString::from("-o");
        output_arg.push(output.as_os_str());
        f.seven_zip(
            &[
                "x".into(),
                "-y".into(),
                output_arg,
                format!("-p{PASSWORD}").into(),
                archive.as_os_str().into(),
            ],
            true,
        );
        f.verify_output(&output);
    }
}

#[test]
#[ignore = "requires installed 7z; run explicitly with --ignored"]
fn both_readers_reject_corrupted_seven_zip_aes_authentication_without_publication() {
    let f = Fixture::new();
    f.write_inputs();
    let archive = f.external_archive("Deflate", "AES256");
    f.external_test(&archive, PASSWORD, true);
    let mut bytes = fs::read(&archive).unwrap();
    // Use the backend only to locate the authentication trailer. Independent
    // 7-Zip must also reject the mutation, so this is not a self-round-trip.
    let tag_end = {
        let mut index = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        (0..index.len())
            .find_map(|i| {
                let entry = index.by_index_raw(i).unwrap();
                (entry.name_raw() == b"binary.dat")
                    .then(|| entry.data_start().unwrap() + entry.compressed_size())
            })
            .unwrap()
    };
    bytes[usize::try_from(tag_end).unwrap() - 1] ^= 1;
    fs::write(&archive, bytes).unwrap();
    f.external_test(&archive, PASSWORD, false);
    let cancel = Cancellation::default();
    let ctl = control(&cancel);
    let session = ArchiveSession::open(&archive, Limits::default(), &ctl).unwrap();
    let ids: Vec<_> = session.entries().iter().map(|e| e.id).collect();
    let output = f.path("output");
    let error = extract_new(&session, &ids, &output, Some(PASSWORD.as_bytes()), &ctl).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Integrity);
    assert!(error.published.is_none());
    assert!(error.residue.is_none());
    assert!(!output.exists());
    f.no_staging();
    assert_eq!(
        fs::read(f.path("outside-sentinel")).unwrap(),
        b"must survive"
    );
}
