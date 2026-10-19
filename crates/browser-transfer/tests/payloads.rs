#![cfg(target_os = "linux")]
use browser_transfer::*;
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

fn native(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}
fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}
fn limits() -> Limits {
    Limits::default()
}

#[test]
fn independent_literal_uri_fixtures_cover_structural_and_filename_bytes() {
    let fixtures: &[(&[u8], &[u8])] = &[
        (b"file:///", b"/"),
        (b"file:/home/person/a", b"/home/person/a"),
        (b"FiLe://LOCALHOST/a", b"/a"),
        (
            b"file:///space%20name/hash%23question%3Fpercent%25",
            b"/space name/hash#question?percent%",
        ),
        (b"file:///line%0Abreak%0Dtab%09", b"/line\nbreak\rtab\t"),
        (b"file:///native-%ff-%FE", b"/native-\xff-\xfe"),
        (b"file:///raw!$&'()*+,;=:@/x", b"/raw!$&'()*+,;=:@/x"),
        (b"file:///slash%5Cname", b"/slash\\name"),
        (b"file:///a//b/../c/./", b"/a//b/../c/./"),
        (b"file:///C:/Users/name", b"/C:/Users/name"),
    ];
    for (uri, expected) in fixtures {
        let path = decode_file_uri(uri, &limits()).unwrap();
        assert_eq!(bytes(&path), *expected);
    }
    // Colon is a legal Linux filename byte. The last fixture does not qualify Windows drives.
}

#[test]
fn canonical_encoding_has_fixed_uppercase_octets_and_no_display_conversion() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"/a b/#?%\n\r", b"file:///a%20b/%23%3F%25%0A%0D"),
        (b"/native-\xff", b"file:///native-%FF"),
        (
            b"/a!$&'()*+,;=:@\\[]",
            b"file:///a%21%24%26%27%28%29%2A%2B%2C%3B%3D%3A%40%5C%5B%5D",
        ),
        (b"/-._~AZaz09", b"file:///-._~AZaz09"),
    ];
    for (path, expected) in cases {
        assert_eq!(
            encode_file_uri(&native(path), &limits()).unwrap(),
            *expected
        );
    }
}

#[test]
fn arbitrary_native_bytes_and_unicode_round_trip_without_normalization() {
    let mut raw = b"/all-native-".to_vec();
    raw.extend((1u8..=255).filter(|b| *b != b'/'));
    for path in [
        native(&raw),
        PathBuf::from("/📁/שלום/العربية/漢字/é"),
        PathBuf::from("/e\u{301}"),
        PathBuf::from("/é"),
    ] {
        let encoded = encode_file_uri(&path, &limits()).unwrap();
        assert!(encoded.is_ascii());
        let decoded = decode_file_uri(&encoded, &limits()).unwrap();
        assert_eq!(bytes(&decoded), bytes(&path));
    }
    assert_ne!(
        encode_file_uri(Path::new("/é"), &limits()).unwrap(),
        encode_file_uri(Path::new("/e\u{301}"), &limits()).unwrap()
    );
}

#[test]
fn escaped_percent_is_decoded_once_and_cannot_create_a_second_uri_or_separator() {
    let parsed = decode_file_uri(b"file:///literal%252Fname%250A%2523%2525", &limits()).unwrap();
    assert_eq!(bytes(&parsed), b"/literal%2Fname%0A%23%25");
    assert_eq!(
        encode_file_uri(&parsed, &limits()).unwrap(),
        b"file:///literal%252Fname%250A%2523%2525"
    );
    let list = decode_uri_list(b"file:///a%0Afile:%2F%2F%2Fother", &limits());
    assert_eq!(list.unwrap_err().kind, ErrorKind::EncodedSeparator);
    let list = decode_uri_list(b"file:///a%0Afile:///other", &limits()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(bytes(&list[0]), b"/a\nfile:///other");
}

#[test]
fn unsafe_nonlocal_and_malformed_uris_are_rejected_explicitly() {
    let cases: &[(&[u8], ErrorKind)] = &[
        (b"https://example/a", ErrorKind::InvalidScheme),
        (b"smb://host/share", ErrorKind::InvalidScheme),
        (b"file://remote/a", ErrorKind::RemoteAuthority),
        (b"file://127.0.0.1/a", ErrorKind::RemoteAuthority),
        (b"file://[::1]/a", ErrorKind::RemoteAuthority),
        (b"file://localhost./a", ErrorKind::RemoteAuthority),
        (b"file://local%68ost/a", ErrorKind::RemoteAuthority),
        (b"file://localhost:80/a", ErrorKind::RemoteAuthority),
        (b"file://user@localhost/a", ErrorKind::RemoteAuthority),
        (b"file://localhost", ErrorKind::AbsolutePathRequired),
        (b"file:relative", ErrorKind::AbsolutePathRequired),
        (b"file:C:/a", ErrorKind::AbsolutePathRequired),
        (b"file:////server/share", ErrorKind::AmbiguousLeadingSlashes),
        (b"file:///a%2Fb", ErrorKind::EncodedSeparator),
        (b"file:///a%2fb", ErrorKind::EncodedSeparator),
        (b"file:///a%00b", ErrorKind::Nul),
        (b"file:///a\0", ErrorKind::Nul),
        (b"file:///a%", ErrorKind::InvalidEscape),
        (b"file:///a%2", ErrorKind::InvalidEscape),
        (b"file:///a%GG", ErrorKind::InvalidEscape),
        (b"file:///a?q", ErrorKind::QueryOrFragment),
        (b"file:///a#fragment", ErrorKind::QueryOrFragment),
        (b"file:///a b", ErrorKind::InvalidByte),
        (b"file:///a\\b", ErrorKind::InvalidByte),
        (b"file:///a[b]", ErrorKind::InvalidByte),
        (b"file:///native-\xff", ErrorKind::InvalidByte),
        (b"file:///a\nb", ErrorKind::InvalidByte),
    ];
    for (uri, expected) in cases {
        assert_eq!(
            decode_file_uri(uri, &limits()).unwrap_err().kind,
            *expected,
            "{uri:?}"
        );
    }
    for path in [
        native(b"relative"),
        native(b""),
        native(b"//ambiguous"),
        native(b"/nul\0name"),
    ] {
        assert!(encode_file_uri(&path, &limits()).is_err());
    }
}

#[test]
fn registered_uri_list_comments_endings_order_and_duplicates_are_exact() {
    let data = b"# ignored metadata\r\nfile:///a\r\nfile:/b\n#another\nfile://localhost/a";
    let paths = decode_uri_list(data, &limits()).unwrap();
    assert_eq!(
        paths.iter().map(|p| bytes(p)).collect::<Vec<_>>(),
        [b"/a", b"/b", b"/a"]
    );
    assert_eq!(
        encode_uri_list(&paths, &limits()).unwrap(),
        b"file:///a\r\nfile:///b\r\nfile:///a\r\n"
    );
    assert_eq!(
        decode_uri_list(b"file:///a\r", &limits()).unwrap_err().kind,
        ErrorKind::InvalidLineEnding
    );
    assert_eq!(
        decode_uri_list(b"file:///a\r\rb", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidLineEnding
    );
    assert_eq!(
        decode_uri_list(b"file:///a\n\n", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::EmptyLine
    );
    assert_eq!(
        decode_uri_list(b" #not-a-comment\nfile:///a", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidScheme
    );
    assert_eq!(
        decode_uri_list(b"#comment\n", &limits()).unwrap_err().kind,
        ErrorKind::EmptyBatch
    );
    assert_eq!(
        decode_uri_list(b"#hidden\0byte\nfile:///a", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::Nul
    );
    assert_eq!(
        decode_uri_list(b"", &limits()).unwrap_err().kind,
        ErrorKind::EmptyBatch
    );
}

#[test]
fn gnome_fixtures_follow_official_serializer_and_reject_ambiguous_envelopes() {
    for (keyword, intent) in [("copy", CopyIntent::Copy), ("cut", CopyIntent::Cut)] {
        let fixture = format!("{keyword}\nfile:///a%20b\nfile:///native-%FF").into_bytes();
        let decoded = decode_gnome_copied_files(&fixture, &limits()).unwrap();
        assert_eq!(decoded.intent, intent);
        assert_eq!(bytes(&decoded.paths[0]), b"/a b");
        assert_eq!(bytes(&decoded.paths[1]), b"/native-\xff");
        assert_eq!(
            encode_gnome_copied_files(&decoded, &limits()).unwrap(),
            fixture
        );
    }
    for payload in [
        &b"COPY\nfile:///a"[..],
        b"move\nfile:///a",
        b" copy\nfile:///a",
    ] {
        assert_eq!(
            decode_gnome_copied_files(payload, &limits())
                .unwrap_err()
                .kind,
            ErrorKind::InvalidIntent
        );
    }
    for payload in [&b"copy\nfile:///a\n"[..], b"cut\n\nfile:///a"] {
        assert_eq!(
            decode_gnome_copied_files(payload, &limits())
                .unwrap_err()
                .kind,
            ErrorKind::EmptyLine
        );
    }
    assert_eq!(
        decode_gnome_copied_files(b"copy\r\nfile:///a", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidLineEnding
    );
    assert_eq!(
        decode_gnome_copied_files(b"copy", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::EmptyBatch
    );
    assert_eq!(
        decode_gnome_copied_files(b"copy\n#comment", &limits())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidScheme
    );
}

#[test]
fn kde_is_an_exact_intent_side_value_never_a_path_or_delete_instruction() {
    for (intent, wire) in [(CopyIntent::Copy, b"0"), (CopyIntent::Cut, b"1")] {
        assert_eq!(encode_kde_cut_selection(intent), wire);
        assert_eq!(decode_kde_cut_selection(wire).unwrap(), intent);
    }
    for wire in [&b""[..], b"true", b"1\0", b"1garbage", b"0\n", b"2"] {
        assert_eq!(
            decode_kde_cut_selection(wire).unwrap_err().kind,
            ErrorKind::InvalidIntent
        );
    }
}

#[test]
fn mixed_valid_invalid_batches_return_one_located_error_and_no_partial_batch() {
    let err = decode_uri_list(
        b"# metadata\nfile:///valid\nfile://remote/bad\nfile:///another",
        &limits(),
    )
    .unwrap_err();
    assert_eq!(
        (err.kind, err.line, err.item),
        (ErrorKind::RemoteAuthority, Some(3), Some(2))
    );
    let err =
        decode_gnome_copied_files(b"cut\nfile:///valid\nfile:///bad%Q0", &limits()).unwrap_err();
    assert_eq!(
        (err.kind, err.line, err.item),
        (ErrorKind::InvalidEscape, Some(3), Some(2))
    );
    let err = encode_uri_list(&[native(b"/valid"), native(b"relative")], &limits()).unwrap_err();
    assert_eq!(
        (err.kind, err.item),
        (ErrorKind::AbsolutePathRequired, Some(2))
    );
}

#[test]
fn every_limit_has_exact_boundary_behavior_for_whole_batches() {
    let paths = vec![native(b"/a"), native(b"/b")];
    let encoded = b"file:///a\r\nfile:///b\r\n";
    let exact = Limits {
        max_payload_bytes: encoded.len(),
        max_lines: 2,
        max_items: 2,
        max_line_bytes: 9,
        max_path_bytes: 2,
        max_total_path_bytes: 4,
    };
    assert_eq!(encode_uri_list(&paths, &exact).unwrap(), encoded);
    assert_eq!(decode_uri_list(encoded, &exact).unwrap(), paths);
    let failures = [
        (
            Limits {
                max_payload_bytes: encoded.len() - 1,
                ..exact.clone()
            },
            ErrorKind::PayloadTooLarge,
        ),
        (
            Limits {
                max_lines: 1,
                ..exact.clone()
            },
            ErrorKind::TooManyLines,
        ),
        (
            Limits {
                max_items: 1,
                ..exact.clone()
            },
            ErrorKind::TooManyItems,
        ),
        (
            Limits {
                max_line_bytes: 8,
                ..exact.clone()
            },
            ErrorKind::LineTooLong,
        ),
        (
            Limits {
                max_path_bytes: 1,
                ..exact.clone()
            },
            ErrorKind::PathTooLong,
        ),
        (
            Limits {
                max_total_path_bytes: 3,
                ..exact.clone()
            },
            ErrorKind::PathsTooLarge,
        ),
    ];
    for (limits, expected) in failures {
        assert_eq!(encode_uri_list(&paths, &limits).unwrap_err().kind, expected);
        assert_eq!(
            decode_uri_list(encoded, &limits).unwrap_err().kind,
            expected
        );
    }
    let comments = b"#one\n#two\nfile:///a";
    assert_eq!(
        decode_uri_list(
            comments,
            &Limits {
                max_lines: 2,
                ..limits()
            }
        )
        .unwrap_err()
        .kind,
        ErrorKind::TooManyLines
    );
    assert_eq!(
        encode_gnome_copied_files(
            &FileBatch {
                intent: CopyIntent::Copy,
                paths
            },
            &Limits {
                max_lines: 2,
                ..limits()
            }
        )
        .unwrap_err()
        .kind,
        ErrorKind::TooManyLines
    );
    assert_eq!(
        encode_uri_list(&[], &limits()).unwrap_err().kind,
        ErrorKind::EmptyBatch
    );
}

#[test]
fn gnome_header_and_comments_cannot_bypass_physical_line_budgets() {
    let wire = b"cut\nfile:///a\nfile:///b";
    let batch = FileBatch {
        intent: CopyIntent::Cut,
        paths: vec![native(b"/a"), native(b"/b")],
    };
    let exact = Limits {
        max_payload_bytes: wire.len(),
        max_lines: 3,
        max_items: 2,
        max_line_bytes: 9,
        max_path_bytes: 2,
        max_total_path_bytes: 4,
    };
    assert_eq!(encode_gnome_copied_files(&batch, &exact).unwrap(), wire);
    assert_eq!(decode_gnome_copied_files(wire, &exact).unwrap(), batch);
    let err = decode_gnome_copied_files(
        wire,
        &Limits {
            max_lines: 2,
            ..exact
        },
    )
    .unwrap_err();
    assert_eq!((err.kind, err.line), (ErrorKind::TooManyLines, Some(3)));
    let comment = b"#commentlongerthanuri\nfile:///a";
    let err = decode_uri_list(
        comment,
        &Limits {
            max_line_bytes: 9,
            ..limits()
        },
    )
    .unwrap_err();
    assert_eq!((err.kind, err.line), (ErrorKind::LineTooLong, Some(1)));
    let zero = Limits {
        max_payload_bytes: 0,
        max_lines: 0,
        max_items: 0,
        max_line_bytes: 0,
        max_path_bytes: 0,
        max_total_path_bytes: 0,
    };
    assert_eq!(
        decode_uri_list(b"file:///a", &zero).unwrap_err().kind,
        ErrorKind::PayloadTooLarge
    );
    assert_eq!(
        encode_uri_list(&batch.paths, &zero).unwrap_err().kind,
        ErrorKind::TooManyItems
    );
}

#[test]
fn codecs_do_not_require_paths_to_exist_or_resolve_dot_segments() {
    let path = native(b"/this-path-is-not-opened/by-the-codec/../x//./");
    let wire = encode_file_uri(&path, &limits()).unwrap();
    assert_eq!(
        bytes(&decode_file_uri(&wire, &limits()).unwrap()),
        bytes(&path)
    );
}

#[test]
fn python_standard_library_independently_checks_percent_octet_mapping() {
    let mut raw = b"/byte-oracle-".to_vec();
    raw.extend((1u8..=255).filter(|b| *b != b'/'));
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let output = std::process::Command::new("python3").args(["-c", "import sys,urllib.parse; sys.stdout.write('file://' + urllib.parse.quote_from_bytes(bytes.fromhex(sys.argv[1]),safe='/'))", &hex]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        encode_file_uri(&native(&raw), &limits()).unwrap(),
        output.stdout
    );
    let output = std::process::Command::new("python3").args(["-c", "import sys,urllib.parse; sys.stdout.buffer.write(urllib.parse.unquote_to_bytes(sys.argv[1]))", std::str::from_utf8(&output.stdout[7..]).unwrap()]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, raw);
}
